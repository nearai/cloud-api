use database::DbPool;
use deadpool::Runtime;
use deadpool_postgres::SslMode;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
};
use tokio_postgres::NoTls;

/// Forward PostgreSQL traffic, but discard the first COMMIT's complete reply
/// and close the connection. Optionally roll back instead to test a failed write.
pub struct CommitReplyProxy {
    pub pool: DbPool,
    pub intercepted: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl CommitReplyProxy {
    pub async fn start(roll_back: bool) -> anyhow::Result<Self> {
        let mut config = super::support::pool_config();
        let target = format!(
            "{}:{}",
            config.host.as_deref().unwrap_or("localhost"),
            config.port.unwrap_or(5432)
        );
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        config.host = Some("127.0.0.1".into());
        config.port = Some(listener.local_addr()?.port());
        config.ssl_mode = Some(SslMode::Disable);
        let pool = DbPool::new(config.create_pool(Some(Runtime::Tokio1), NoTls)?);
        let intercepted = Arc::new(AtomicBool::new(false));
        let intercept = intercepted.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((client, _)) = accepted else { break };
                        let target = target.clone();
                        let intercept = intercept.clone();
                        connections.spawn(async move {
                            let _ = forward(client, &target, intercept, roll_back).await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Ok(Self {
            pool,
            intercepted,
            task,
        })
    }
}

impl Drop for CommitReplyProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn payload(reader: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Vec<u8>> {
    let length = reader.read_u32().await?;
    if !(4..=1_048_576).contains(&length) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid test protocol frame",
        ));
    }
    let mut message = vec![0; length as usize];
    message[..4].copy_from_slice(&length.to_be_bytes());
    reader.read_exact(&mut message[4..]).await?;
    Ok(message)
}

async fn forward(
    mut client: TcpStream,
    target: &str,
    intercepted: Arc<AtomicBool>,
    roll_back: bool,
) -> std::io::Result<()> {
    let mut server = TcpStream::connect(target).await?;
    // The untagged startup frame precedes regular tagged protocol messages.
    server.write_all(&payload(&mut client).await?).await?;
    let (mut client_read, mut client_write) = client.split();
    let (mut server_read, mut server_write) = server.split();
    let requests = AtomicUsize::new(0);
    let discard_request = AtomicUsize::new(usize::MAX);
    let to_server = async {
        loop {
            let tag = client_read.read_u8().await?;
            let mut message = payload(&mut client_read).await?;
            let request = if matches!(tag, b'Q' | b'S') {
                requests.fetch_add(1, Ordering::SeqCst) + 1
            } else {
                0
            };
            if tag == b'Q'
                && message[4..] == *b"COMMIT\0"
                && !intercepted.swap(true, Ordering::SeqCst)
            {
                discard_request.store(request, Ordering::SeqCst);
                if roll_back {
                    message = 13u32.to_be_bytes().to_vec();
                    message.extend_from_slice(b"ROLLBACK\0");
                }
            }
            server_write.write_u8(tag).await?;
            server_write.write_all(&message).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), std::io::Error>(())
    };
    let to_client = async {
        let mut startup = true;
        let mut completed_requests = 0;
        loop {
            let tag = server_read.read_u8().await?;
            let message = payload(&mut server_read).await?;
            // A previous query's ReadyForQuery may still be in flight when
            // COMMIT arrives. Only discard replies belonging to COMMIT itself.
            if !startup && discard_request.load(Ordering::SeqCst) == completed_requests + 1 {
                if tag == b'Z' {
                    // ReadyForQuery proves the server finished COMMIT/ROLLBACK.
                    return Ok::<(), std::io::Error>(());
                }
            } else {
                client_write.write_u8(tag).await?;
                client_write.write_all(&message).await?;
            }
            if tag == b'Z' {
                if startup {
                    startup = false;
                } else {
                    completed_requests += 1;
                }
            }
        }
    };
    tokio::select! {
        result = to_server => result,
        result = to_client => result,
    }
}
