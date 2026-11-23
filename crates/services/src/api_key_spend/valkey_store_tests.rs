//! The adapter with the real Lua scripts against a real Valkey.

use inference_providers::placement_io::ValkeyEndpoint;
use inference_providers::valkey_kv::{KvErrorKind, ValkeyKv};
use uuid::Uuid;

use super::lua;
use super::ports::{SpendTotalStore, TotalRead};
use super::valkey_store::ValkeyTotalStore;

const ENVIRONMENT: &str = "itest";

fn connect(addr: &str, password: &str) -> ValkeyKv {
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    ValkeyKv::start(
        &ValkeyEndpoint {
            host: host.to_string(),
            port: port.parse().expect("port"),
            tls: false,
            ca_pem: None,
        },
        password.to_string(),
    )
    .expect("client")
}

async fn ready(kv: &ValkeyKv) {
    for _ in 0..200 {
        if kv.get("spend:probe").await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("Valkey did not become reachable");
}

/// Real Valkey 9.x, the ACL user `router` limited to
/// `~spend:* -@all +get +set +incrby +exists +del +eval` there:
///
/// ```text
/// SPEND_TOTAL_TEST_REDIS_ADDR=127.0.0.1:PORT \
/// SPEND_TOTAL_TEST_REDIS_PASSWORD=<router password> \
/// cargo test -p services --lib real_valkey -- --ignored
/// ```
///
/// Use a disposable server: the test creates (and removes) keys under
/// `spend:v2:itest:`.
#[tokio::test]
#[ignore = "needs SPEND_TOTAL_TEST_REDIS_ADDR and SPEND_TOTAL_TEST_REDIS_PASSWORD"]
async fn real_valkey_scripts_round_trip() {
    let addr = std::env::var("SPEND_TOTAL_TEST_REDIS_ADDR")
        .expect("SPEND_TOTAL_TEST_REDIS_ADDR must point at a disposable Valkey");
    let password = std::env::var("SPEND_TOTAL_TEST_REDIS_PASSWORD")
        .expect("SPEND_TOTAL_TEST_REDIS_PASSWORD must be the router password");
    // A second connection reads the keys the adapter wrote.
    let probe = connect(&addr, &password);
    ready(&probe).await;
    let store = ValkeyTotalStore::new(connect(&addr, &password), ENVIRONMENT.to_string(), 600);
    // Wait for the adapter's own connection too.
    for _ in 0..200 {
        if store.get_total(Uuid::new_v4()).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let id = Uuid::new_v4();
    let t = lua::total_key(ENVIRONMENT, id);
    let p = lua::pend_key(ENVIRONMENT, id);
    let g = lua::gen_key(ENVIRONMENT, id);
    let read = |key: String| {
        let probe = &probe;
        async move { probe.get(&key).await.unwrap() }
    };

    // ADD with no keys returns 0 and creates nothing.
    assert_eq!(store.add(id, 5).await, Ok(0));
    assert_eq!(read(t.clone()).await, None);
    assert_eq!(read(p.clone()).await, None);
    assert_eq!(store.get_total(id).await, Ok(TotalRead::Missing));

    // BEGIN, then ADD 7: pend 7, no total.
    let generation = store.begin(id).await.unwrap();
    assert_eq!(generation, "0");
    assert_eq!(store.add(id, 7).await, Ok(2));
    assert_eq!(read(p.clone()).await.as_deref(), Some("7"));
    assert_eq!(read(t.clone()).await, None);

    // FINISH(sum 100) = 107, removes pend, bumps the generation.
    assert_eq!(store.finish(id, 100, &generation).await, Ok(Some(107)));
    assert_eq!(read(t.clone()).await.as_deref(), Some("107"));
    assert_eq!(read(p.clone()).await, None);
    assert_eq!(read(g.clone()).await.as_deref(), Some("1"));
    assert_eq!(store.get_total(id).await, Ok(TotalRead::Value(107)));

    // ADD 5 -> 112 (only the total exists now).
    assert_eq!(store.add(id, 5).await, Ok(1));
    assert_eq!(read(t.clone()).await.as_deref(), Some("112"));

    // A second FINISH with the old generation aborts.
    assert_eq!(store.finish(id, 1, &generation).await, Ok(None));
    assert_eq!(read(t.clone()).await.as_deref(), Some("112"));

    // INVALIDATE removes total and pend and bumps the generation.
    store.invalidate(id).await.unwrap();
    assert_eq!(read(t.clone()).await, None);
    assert_eq!(read(p.clone()).await, None);
    assert_eq!(read(g.clone()).await.as_deref(), Some("2"));

    // Totals above 2^53 are exact: the server adds, not Lua.
    let generation = store.begin(id).await.unwrap();
    assert_eq!(generation, "2");
    assert_eq!(store.add(id, 2).await, Ok(2));
    assert_eq!(
        store.finish(id, 9_007_199_254_740_993, &generation).await,
        Ok(Some(9_007_199_254_740_995))
    );
    assert_eq!(read(t.clone()).await.as_deref(), Some("9007199254740995"));

    // A malformed total is reported as such.
    probe
        .eval(
            "return redis.call('SET', KEYS[1], 'abc')",
            std::slice::from_ref(&t),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(store.get_total(id).await, Ok(TotalRead::Malformed));

    // A script cannot write a key outside `spend:*`, declared or not.
    let outside = format!("other:{}", Uuid::new_v4());
    let declared = probe
        .eval(
            "return redis.call('SET', KEYS[1], 'x')",
            std::slice::from_ref(&outside),
            &[],
        )
        .await;
    let undeclared = probe
        .eval(
            "return redis.call('SET', ARGV[1], 'x')",
            std::slice::from_ref(&t),
            std::slice::from_ref(&outside),
        )
        .await;
    println!("outside key, declared: {declared:?}; undeclared: {undeclared:?}");
    assert!(declared.is_err());
    assert!(undeclared.is_err());
    // Both are ACL refusals, so both pause the shared level for a minute.
    assert_eq!(declared.unwrap_err(), KvErrorKind::NoPerm);
    assert_eq!(undeclared.unwrap_err(), KvErrorKind::NoPerm);

    // Clean up; only the generation key remains (it expires by itself).
    store.invalidate(id).await.unwrap();
    assert_eq!(store.add(id, 1).await, Ok(0));
}
