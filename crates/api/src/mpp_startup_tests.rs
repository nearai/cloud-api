//! Session configuration errors propagate before a router or background task is built.

use config::{MppConfig, MppModelPrice, MppSessionSettings, MppSettings, MppTempoSettings};
use services::mpp::MppError;
use std::collections::BTreeMap;

fn config(signer: &str, rpc: &str) -> MppConfig {
    MppConfig::Enabled(Box::new(MppSettings {
        realm: "session-startup.test".into(),
        secret_key: "startup-test-hmac-0123456789abcdef".into(),
        service_api_key: "sk-live-startup-test-0001".into(),
        challenge_ttl_secs: 600,
        tempo: MppTempoSettings {
            rpc_url: rpc.into(),
            rpc_timeout_secs: 5,
            chain_id: 42431,
            currency: format!("0x{}", hex::encode([4; 20])),
            recipient: format!("0x{}", hex::encode([5; 20])),
        },
        model_prices: BTreeMap::from([(
            "startup-model".into(),
            MppModelPrice {
                amount: 0,
                token_price: Some(2),
                max_tokens: 256,
                max_request_bytes: 8192,
            },
        )]),
        session: Some(MppSessionSettings {
            signer_key: Some(signer.into()),
            min_voucher_delta: Some(1),
            suggested_deposit: 1_000_000,
            settle_interval_secs: 3600,
            settle_amount: 1_000_000,
            close_supervision_secs: 60,
        }),
    }))
}

#[test]
fn mpp_session_startup_rejects_invalid_signer_scalar() {
    let config = config(&hex::encode([0; 32]), "http://127.0.0.1:1");
    assert!(matches!(
        super::init_mpp_session(database::DbPool::uninitialized(), &config),
        Err(MppError::Config)
    ));
}

#[test]
fn mpp_session_startup_rejects_invalid_rpc_url() {
    let config = config(&hex::encode([1; 32]), "invalid-rpc-url");
    assert!(matches!(
        super::init_mpp_session(database::DbPool::uninitialized(), &config),
        Err(MppError::Config)
    ));
}
