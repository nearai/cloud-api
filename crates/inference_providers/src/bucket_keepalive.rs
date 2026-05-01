//! Shared HTTP/2 keepalive configuration for bucket-style sticky clients.
//!
//! Multi-host inference models (e.g. GLM-5, GLM-5.1, Qwen3.5) sit behind
//! `model-proxy`'s L4 TLS passthrough, which load-balances per TCP connection.
//! `cloud-api` uses one `reqwest::Client` per prefix bucket and relies on the
//! H2 multiplexed connection staying up so completion + signature fetch land
//! on the same backend. If the connection drops, the L4 LB may pick a
//! different backend and the signature fetch returns 404 "Chat id not found".
//!
//! These settings keep the connection alive across long idle gaps:
//! - PINGs every 30s (and crucially `while_idle`, so PINGs fire with 0 streams)
//! - Close fast (10s timeout) on a dead connection so re-verification can run
//! - TCP keepalive as a backstop for silently-dropped paths
//! - `pool_idle_timeout(None)` — liveness comes from PINGs, not a wall-clock
//!   timer that fires regardless of health
//!
//! Both creation paths (legacy eager in `vllm/mod.rs` and verified lazy in
//! `services::inference_provider_pool::PoolBackendVerifier`) call [`apply`]
//! so the configuration cannot drift between them.

use std::time::Duration;

use reqwest::ClientBuilder;

/// HTTP/2 PING interval. Must be shorter than the server's `keepalive_timeout`
/// (CVM nginx is set to `1h` in cvm-compose-files; nginx default is 75s).
pub const H2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Time to wait for a PING ACK before considering the connection dead.
pub const H2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// OS-level TCP keepalive interval. Catches silently-dropped NAT/L4 paths
/// that don't surface as PING-ACK failures.
pub const TCP_KEEPALIVE: Duration = Duration::from_secs(30);

/// Apply bucket-style H2 keepalive settings to a [`ClientBuilder`].
///
/// `http2_keep_alive_while_idle(true)` is load-bearing — without it, PINGs
/// only fire when at least one stream is open, which doesn't keep the
/// connection alive between chats. If you refactor this, keep `while_idle`
/// set to `true`. The integration test in this module verifies a PING does
/// arrive on an idle connection.
pub fn apply(builder: ClientBuilder) -> ClientBuilder {
    apply_with(
        builder,
        H2_KEEPALIVE_INTERVAL,
        H2_KEEPALIVE_TIMEOUT,
        TCP_KEEPALIVE,
    )
}

/// Same as [`apply`], but with overridable durations for tests that need a
/// shorter PING interval to assert behavior synchronously.
pub fn apply_with(
    builder: ClientBuilder,
    h2_interval: Duration,
    h2_timeout: Duration,
    tcp_keepalive: Duration,
) -> ClientBuilder {
    builder
        .http2_keep_alive_interval(h2_interval)
        .http2_keep_alive_timeout(h2_timeout)
        .http2_keep_alive_while_idle(true)
        .tcp_keepalive(tcp_keepalive)
        .pool_idle_timeout(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn constants_are_nonzero() {
        assert!(H2_KEEPALIVE_INTERVAL > Duration::ZERO);
        assert!(H2_KEEPALIVE_TIMEOUT > Duration::ZERO);
        assert!(TCP_KEEPALIVE > Duration::ZERO);
        // Interval must be shorter than nginx's keepalive_timeout default (75s)
        // for the keepalive to actually do its job in the worst case.
        assert!(H2_KEEPALIVE_INTERVAL < Duration::from_secs(75));
    }

    #[test]
    fn apply_returns_buildable_client() {
        let client = apply(reqwest::Client::builder()).build();
        assert!(
            client.is_ok(),
            "client builder should produce a valid client"
        );
    }

    /// HTTP/2 frame types we care about.
    const FRAME_TYPE_DATA: u8 = 0x0;
    const FRAME_TYPE_HEADERS: u8 = 0x1;
    const FRAME_TYPE_SETTINGS: u8 = 0x4;
    const FRAME_TYPE_PING: u8 = 0x6;
    const FRAME_TYPE_WINDOW_UPDATE: u8 = 0x8;

    const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

    /// Read exactly N bytes or return None on EOF/error.
    async fn read_exact(s: &mut tokio::net::TcpStream, buf: &mut [u8]) -> Option<()> {
        s.read_exact(buf).await.ok().map(|_| ())
    }

    /// Read one HTTP/2 frame header + payload. Returns (type, flags, stream_id, payload).
    async fn read_frame(s: &mut tokio::net::TcpStream) -> Option<(u8, u8, u32, Vec<u8>)> {
        let mut hdr = [0u8; 9];
        read_exact(s, &mut hdr).await?;
        let len = ((hdr[0] as u32) << 16) | ((hdr[1] as u32) << 8) | (hdr[2] as u32);
        let frame_type = hdr[3];
        let flags = hdr[4];
        let stream_id = u32::from_be_bytes([hdr[5] & 0x7f, hdr[6], hdr[7], hdr[8]]);
        let mut payload = vec![0u8; len as usize];
        if !payload.is_empty() {
            read_exact(s, &mut payload).await?;
        }
        Some((frame_type, flags, stream_id, payload))
    }

    fn write_frame_header(buf: &mut Vec<u8>, len: usize, ty: u8, flags: u8, stream_id: u32) {
        buf.push(((len >> 16) & 0xff) as u8);
        buf.push(((len >> 8) & 0xff) as u8);
        buf.push((len & 0xff) as u8);
        buf.push(ty);
        buf.push(flags);
        buf.extend_from_slice(&stream_id.to_be_bytes());
    }

    /// Verify that an idle bucket client sends an HTTP/2 PING within the
    /// keepalive interval. This guards against `while_idle(true)` being
    /// silently dropped from `apply()` — without that flag, no PING fires
    /// when there are zero open streams.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_bucket_client_sends_h2_ping() {
        // Bind a raw TCP listener and speak just enough HTTP/2 to:
        //   1. Accept the client preface and SETTINGS exchange
        //   2. Respond to the one HEADERS request with a 200 + empty DATA
        //   3. Idle and observe whether a PING frame arrives
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        // Detected on the server side and reported via this oneshot.
        let (ping_tx, ping_rx) = tokio::sync::oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.expect("accept");

            // 1) Read 24-byte client preface.
            let mut preface = [0u8; 24];
            read_exact(&mut s, &mut preface)
                .await
                .expect("read preface");
            assert_eq!(&preface[..], H2_PREFACE, "client preface mismatch");

            // 2) Read client SETTINGS frame.
            let (ty, _flags, _sid, _payload) =
                read_frame(&mut s).await.expect("read client SETTINGS");
            assert_eq!(ty, FRAME_TYPE_SETTINGS, "expected SETTINGS frame first");

            // 3) Send our SETTINGS (empty) and SETTINGS ACK.
            let mut out = Vec::new();
            // Empty SETTINGS
            write_frame_header(&mut out, 0, FRAME_TYPE_SETTINGS, 0, 0);
            // SETTINGS ACK (flag 0x1)
            write_frame_header(&mut out, 0, FRAME_TYPE_SETTINGS, 0x1, 0);
            s.write_all(&out).await.expect("write our SETTINGS");
            s.flush().await.expect("flush");

            // 4) Loop reading frames; when we see HEADERS for a request, send back a
            //    minimal 200 response, then watch for PING.
            let mut ping_tx = Some(ping_tx);
            loop {
                let Some((ty, _flags, stream_id, _payload)) = read_frame(&mut s).await else {
                    return;
                };
                match ty {
                    FRAME_TYPE_HEADERS => {
                        // Send a tiny HPACK-encoded :status 200.
                        // 0x88 = indexed header, index 8 (":status: 200").
                        let resp_hdrs = [0x88u8];
                        let mut resp = Vec::new();
                        // HEADERS with END_HEADERS (0x4)
                        write_frame_header(
                            &mut resp,
                            resp_hdrs.len(),
                            FRAME_TYPE_HEADERS,
                            0x4,
                            stream_id,
                        );
                        resp.extend_from_slice(&resp_hdrs);
                        // Empty DATA with END_STREAM (0x1)
                        write_frame_header(&mut resp, 0, FRAME_TYPE_DATA, 0x1, stream_id);
                        s.write_all(&resp).await.expect("write response");
                        s.flush().await.expect("flush");
                    }
                    FRAME_TYPE_PING => {
                        if let Some(tx) = ping_tx.take() {
                            let _ = tx.send(());
                        }
                        // Don't ACK — we just need to know one arrived.
                    }
                    FRAME_TYPE_SETTINGS | FRAME_TYPE_WINDOW_UPDATE => {
                        // ignore
                    }
                    _ => {}
                }
            }
        });

        // Build a bucket-style client with very short PING interval (100ms) so the
        // test runs synchronously. http2_prior_knowledge avoids needing TLS.
        let client = apply_with(
            reqwest::Client::builder().http2_prior_knowledge(),
            Duration::from_millis(100),
            Duration::from_secs(2),
            Duration::from_secs(30),
        )
        .build()
        .expect("build client");

        // Send one request to establish the H2 connection, then go idle.
        let url = format!("http://{addr}/");
        let resp = tokio::time::timeout(Duration::from_secs(5), client.get(&url).send())
            .await
            .expect("request did not time out")
            .expect("request succeeded");
        assert_eq!(resp.status(), 200);
        // Drop the response so the stream is fully closed → connection has 0 streams.
        drop(resp);

        // With while_idle=true, a PING must arrive within ~3 intervals.
        // Without it, this would time out.
        tokio::time::timeout(Duration::from_secs(5), ping_rx)
            .await
            .expect("PING frame did not arrive within 5s — http2_keep_alive_while_idle may be off")
            .expect("ping channel closed");

        server.abort();
    }
}
