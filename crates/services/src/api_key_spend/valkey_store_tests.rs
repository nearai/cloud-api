//! The adapter and the real function library against a real Valkey.
//!
//! The test needs only the address of a disposable Valkey 8 or 9 whose
//! default user is open (as in a fresh container). It loads the library file
//! itself, creates the restricted ACL users and runs the adapter as them.
//! CI runs it against a service container.

use inference_providers::placement_io::ValkeyEndpoint;
use inference_providers::valkey_kv::{KvErrorKind, KvReply, ValkeyKv};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use uuid::Uuid;

use super::keys;
use super::ports::{SpendTotalStore, StoreErrorKind, TotalRead};
use super::valkey_store::{ValkeyTotalStore, FN_GET, LIBRARY};

const USER_A: &str = "spend-it-a";
const USER_B: &str = "spend-it-b";

/// What an operator grants the service's user: its own namespace, no pub/sub,
/// and `FCALL` plus the commands the functions use, nothing else (no `EVAL`).
const ACL_RULES: &str =
    "resetchannels -@all +@connection +fcall +get +set +incrby +exists +del +ttl";

/// A second library, only to make a function touch a key it was not given.
const PEEK_LIBRARY: &str = "#!lua name=spendtest\n\
redis.register_function('spendtest_peek', function(keys, args) return redis.call('GET', args[1]) end)\n";

#[derive(Debug, PartialEq)]
enum Resp {
    Simple(String),
    Error(String),
    Int(i64),
    Bulk(Option<String>),
}

/// A minimal RESP2 client for the operator's side of the test.
struct Raw(BufReader<TcpStream>);

impl Raw {
    async fn connect(addr: &str) -> Self {
        for _ in 0..200 {
            if let Ok(stream) = TcpStream::connect(addr).await {
                return Self(BufReader::new(stream));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("Valkey did not become reachable");
    }

    async fn cmd(&mut self, args: &[&str]) -> Resp {
        let mut out = format!("*{}\r\n", args.len()).into_bytes();
        for a in args {
            out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
            out.extend_from_slice(a.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        self.0.write_all(&out).await.unwrap();
        let mut line = String::new();
        self.0.read_line(&mut line).await.unwrap();
        let line = line.trim_end().to_string();
        let (kind, rest) = line.split_at(1);
        match kind {
            "+" => Resp::Simple(rest.to_string()),
            "-" => Resp::Error(rest.to_string()),
            ":" => Resp::Int(rest.parse().unwrap()),
            "$" => {
                let len: i64 = rest.parse().unwrap();
                if len < 0 {
                    return Resp::Bulk(None);
                }
                let mut buf = vec![0u8; len as usize + 2];
                self.0.read_exact(&mut buf).await.unwrap();
                buf.truncate(len as usize);
                Resp::Bulk(Some(String::from_utf8(buf).unwrap()))
            }
            other => panic!("unexpected reply type {other}"),
        }
    }

    async fn ok(&mut self, args: &[&str]) {
        match self.cmd(args).await {
            Resp::Simple(s) if s == "OK" => {}
            other => panic!("{args:?} -> {other:?}"),
        }
    }

    async fn get(&mut self, key: &str) -> Option<String> {
        match self.cmd(&["GET", key]).await {
            Resp::Bulk(v) => v,
            other => panic!("GET -> {other:?}"),
        }
    }

    async fn int(&mut self, args: &[&str]) -> i64 {
        match self.cmd(args).await {
            Resp::Int(n) => n,
            other => panic!("{args:?} -> {other:?}"),
        }
    }
}

fn connect(addr: &str, user: &str, password: &str) -> ValkeyKv {
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    ValkeyKv::start(
        &ValkeyEndpoint {
            host: host.to_string(),
            port: port.parse().expect("port"),
            tls: false,
            ca_pem: None,
        },
        user,
        password.to_string(),
    )
    .expect("client")
}

/// Waits until the client's background connection is up (a permission error
/// is an answer too).
async fn connected(kv: &ValkeyKv) {
    for _ in 0..200 {
        if kv.get("spend:probe").await != Err(KvErrorKind::Conn) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the client did not connect");
}

fn is_uuid(token: &str) -> bool {
    Uuid::parse_str(token).is_ok()
}

/// Real Valkey with the library loaded by the test, the adapter running as a
/// user that may only touch `spend:v2:<user>:*` and call functions:
///
/// ```text
/// docker run --rm -d -p 127.0.0.1:6379:6379 valkey/valkey:9
/// SPEND_TOTAL_TEST_VALKEY_ADDR=127.0.0.1:6379 \
///   cargo test -p services --lib real_valkey -- --ignored --nocapture
/// ```
///
/// Use a disposable server: the test loads two libraries, creates two ACL
/// users and writes keys under `spend:v2:spend-it-*`, and removes what it
/// can at the end.
#[tokio::test]
#[ignore = "needs SPEND_TOTAL_TEST_VALKEY_ADDR, the address of a disposable Valkey"]
async fn real_valkey_functions_round_trip() {
    let addr = std::env::var("SPEND_TOTAL_TEST_VALKEY_ADDR")
        .expect("SPEND_TOTAL_TEST_VALKEY_ADDR must point at a disposable Valkey");

    // The operator: loads the library and creates the users.
    let mut admin = Raw::connect(&addr).await;
    match admin.cmd(&["INFO", "server"]).await {
        Resp::Bulk(Some(info)) => {
            let version = info
                .lines()
                .find_map(|l| l.strip_prefix("valkey_version:"))
                .unwrap_or("unknown");
            println!("Valkey version: {version}");
        }
        other => panic!("INFO -> {other:?}"),
    }
    assert_eq!(
        admin.cmd(&["FUNCTION", "LOAD", "REPLACE", LIBRARY]).await,
        Resp::Bulk(Some("spendtotal".to_string()))
    );
    assert_eq!(
        admin
            .cmd(&["FUNCTION", "LOAD", "REPLACE", PEEK_LIBRARY])
            .await,
        Resp::Bulk(Some("spendtest".to_string()))
    );
    for (user, password) in [(USER_A, "pw-a"), (USER_B, "pw-b")] {
        let pattern = format!("~spend:v2:{user}:*");
        let password = format!(">{password}");
        let mut args = vec!["ACL", "SETUSER", user, "on", &password, &pattern];
        args.extend(ACL_RULES.split(' '));
        admin.ok(&args).await;
    }

    let probe_a = connect(&addr, USER_A, "pw-a");
    connected(&probe_a).await;
    let kv_b = connect(&addr, USER_B, "pw-b");
    connected(&kv_b).await;
    let store = ValkeyTotalStore::new(connect(&addr, USER_A, "pw-a"), USER_A.to_string(), 600);
    for _ in 0..200 {
        if store.get_total(Uuid::new_v4()).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let id = Uuid::new_v4();
    let t = keys::total_key(USER_A, id);
    let p = keys::pend_key(USER_A, id);
    let g = keys::gen_key(USER_A, id);

    // ADD with no keys returns 0 and creates nothing.
    assert_eq!(store.add(id, 5).await, Ok(0));
    assert_eq!(admin.get(&t).await, None);
    assert_eq!(admin.get(&p).await, None);
    assert_eq!(store.get_total(id).await, Ok(TotalRead::Missing));

    // BEGIN, then ADD 7: pend 7, no total, expiries set.
    let token = store.begin(id).await.unwrap();
    assert!(is_uuid(&token), "a random token, not a counter");
    assert_eq!(admin.get(&g).await.as_deref(), Some(token.as_str()));
    assert_eq!(store.add(id, 7).await, Ok(2));
    assert_eq!(admin.get(&p).await.as_deref(), Some("7"));
    assert_eq!(admin.get(&t).await, None);
    assert!((1..=120).contains(&admin.int(&["TTL", &p]).await));
    assert!((1..=3600).contains(&admin.int(&["TTL", &g]).await));
    // A second BEGIN keeps the token and the pending costs.
    assert_eq!(store.begin(id).await.as_deref(), Ok(token.as_str()));
    assert_eq!(admin.get(&p).await.as_deref(), Some("7"));

    // FINISH(sum 100) = 107, removes pend, replaces the token; the total
    // expires within the jittered re-check interval.
    assert_eq!(store.finish(id, 100, &token).await, Ok(Some(107)));
    assert_eq!(admin.get(&t).await.as_deref(), Some("107"));
    assert_eq!(admin.get(&p).await, None);
    let after_finish = admin.get(&g).await.unwrap();
    assert!(is_uuid(&after_finish));
    assert_ne!(after_finish, token);
    assert!((540..=660).contains(&admin.int(&["TTL", &t]).await));
    assert_eq!(store.get_total(id).await, Ok(TotalRead::Value(107)));

    // ADD 5 -> 112 (only the total exists now) and keeps the expiry.
    assert_eq!(store.add(id, 5).await, Ok(1));
    assert_eq!(admin.get(&t).await.as_deref(), Some("112"));
    assert!(admin.int(&["TTL", &t]).await > 0);

    // A second FINISH with the old token aborts.
    assert_eq!(store.finish(id, 1, &token).await, Ok(None));
    assert_eq!(admin.get(&t).await.as_deref(), Some("112"));

    // INVALIDATE removes total and pend and replaces the token.
    store.invalidate(id).await.unwrap();
    assert_eq!(admin.get(&t).await, None);
    assert_eq!(admin.get(&p).await, None);
    let after_invalidate = admin.get(&g).await.unwrap();
    assert!(is_uuid(&after_invalidate));
    assert_ne!(after_invalidate, after_finish);

    // Totals above 2^53 are exact: the server adds, not Lua.
    let token = store.begin(id).await.unwrap();
    assert_eq!(token, after_invalidate);
    assert_eq!(store.add(id, 2).await, Ok(2));
    assert_eq!(
        store.finish(id, 9_007_199_254_740_993, &token).await,
        Ok(Some(9_007_199_254_740_995))
    );
    assert_eq!(admin.get(&t).await.as_deref(), Some("9007199254740995"));
    assert_eq!(store.add(id, 1).await, Ok(1));
    assert_eq!(admin.get(&t).await.as_deref(), Some("9007199254740996"));
    assert_eq!(
        store.get_total(id).await,
        Ok(TotalRead::Value(9_007_199_254_740_996))
    );

    // A malformed total is reported as such.
    admin.ok(&["SET", &t, "abc", "EX", "100"]).await;
    assert_eq!(store.get_total(id).await, Ok(TotalRead::Malformed));
    // ADD on it fails (INCRBY) and is an error kind, not a panic.
    assert_eq!(store.add(id, 1).await, Err(StoreErrorKind::Other));

    // A total without an expiry was not written by the library: GET drops it
    // (so the next check re-seeds), and so does ADD.
    admin.ok(&["SET", &t, "5"]).await;
    assert_eq!(store.get_total(id).await, Ok(TotalRead::Missing));
    assert_eq!(admin.int(&["EXISTS", &t]).await, 0);
    admin.ok(&["SET", &t, "5"]).await;
    assert_eq!(store.add(id, 1).await, Ok(0));
    assert_eq!(admin.int(&["EXISTS", &t]).await, 0);
    // A pending key without an expiry is dropped as well.
    let token = store.begin(id).await.unwrap();
    admin.ok(&["SET", &p, "40"]).await;
    assert_eq!(store.add(id, 1).await, Ok(0));
    assert_eq!(admin.int(&["EXISTS", &p]).await, 0);
    assert_eq!(store.finish(id, 10, &token).await, Ok(None));
    // ... and FINISH replaces an unexpiring total instead of aborting.
    let token = store.begin(id).await.unwrap();
    admin.ok(&["SET", &t, "5"]).await;
    assert_eq!(store.finish(id, 10, &token).await, Ok(Some(10)));
    assert!(admin.int(&["TTL", &t]).await > 0);

    // A wiped store aborts an in-flight seed: the older seeder's token cannot
    // match the missing token, nor a token issued afterwards.
    store.invalidate(id).await.unwrap();
    let older = store.begin(id).await.unwrap();
    admin.int(&["DEL", &t, &p, &g]).await;
    assert_eq!(store.finish(id, 10, &older).await, Ok(None));
    let newer = store.begin(id).await.unwrap();
    assert_ne!(older, newer);
    assert_eq!(store.finish(id, 10, &older).await, Ok(None));
    assert_eq!(admin.get(&t).await, None);
    assert_eq!(store.finish(id, 100, &newer).await, Ok(Some(100)));

    // Another namespace's user gets a permission error on these keys.
    let other = kv_b.fcall(FN_GET, std::slice::from_ref(&t), &[]).await;
    println!("other namespace: {other:?}");
    assert_eq!(other, Err(KvErrorKind::NoPerm));
    let other_add = kv_b
        .fcall("spend_v2_add", &[t.clone(), p.clone()], &["1".to_string()])
        .await;
    assert_eq!(other_add, Err(KvErrorKind::NoPerm));
    assert_eq!(admin.get(&t).await.as_deref(), Some("100"));
    // Its own namespace works.
    let store_b = ValkeyTotalStore::new(kv_b, USER_B.to_string(), 600);
    assert_eq!(store_b.get_total(id).await, Ok(TotalRead::Missing));
    // A function that reads a key it was not given hits the ACL inside the
    // function; that is the same permission error.
    let b_key = keys::total_key(USER_B, id);
    let inside = probe_a
        .fcall(
            "spendtest_peek",
            std::slice::from_ref(&t),
            std::slice::from_ref(&b_key),
        )
        .await;
    println!("ACL failure inside a function: {inside:?}");
    assert_eq!(inside, Err(KvErrorKind::NoPerm));
    // Reading its own key inside a function is fine.
    assert_eq!(
        probe_a
            .fcall(
                "spendtest_peek",
                std::slice::from_ref(&t),
                std::slice::from_ref(&t)
            )
            .await,
        Ok(KvReply::Str("100".to_string()))
    );

    // The restricted user cannot send scripts or load functions.
    let mut restricted = Raw::connect(&addr).await;
    restricted.ok(&["AUTH", USER_A, "pw-a"]).await;
    for args in [
        vec!["EVAL", "return 1", "0"],
        vec!["SCRIPT", "LOAD", "return 1"],
        vec!["FUNCTION", "LOAD", PEEK_LIBRARY],
        vec!["FLUSHALL"],
    ] {
        match restricted.cmd(&args).await {
            Resp::Error(e) => assert!(e.starts_with("NOPERM"), "{:?}: {e}", args[0]),
            other => panic!("{:?} -> {other:?}", args[0]),
        }
    }

    // A library that is not loaded is its own error kind.
    admin.ok(&["FUNCTION", "DELETE", "spendtotal"]).await;
    assert_eq!(store.get_total(id).await, Err(StoreErrorKind::NoFunction));
    assert_eq!(store.add(id, 1).await, Err(StoreErrorKind::NoFunction));
    assert_eq!(
        admin.cmd(&["FUNCTION", "LOAD", LIBRARY]).await,
        Resp::Bulk(Some("spendtotal".to_string()))
    );
    assert_eq!(store.get_total(id).await, Ok(TotalRead::Value(100)));

    // Clean up.
    store.invalidate(id).await.unwrap();
    admin.int(&["DEL", &t, &p, &g]).await;
    admin.int(&["DEL", &b_key]).await;
    admin.int(&["ACL", "DELUSER", USER_A, USER_B]).await;
    admin.ok(&["FUNCTION", "DELETE", "spendtest"]).await;
}
