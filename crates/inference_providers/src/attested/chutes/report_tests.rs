use super::*;
use evidence::InstanceEvidence;
use verifier_port::VerifiedInstanceInfo;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Default)]
struct ReportVerifier {
    calls: std::sync::Mutex<Vec<(String, String, String)>>,
}

#[async_trait]
impl ChutesInstanceVerifier for ReportVerifier {
    async fn attest_instance(
        &self,
        evidence: &InstanceEvidence,
        boot_nonce: &str,
        e2e_pubkey: &str,
    ) -> Result<VerifiedInstanceInfo, String> {
        self.calls.lock().unwrap().push((
            evidence.instance_id.clone(),
            boot_nonce.to_string(),
            e2e_pubkey.to_string(),
        ));
        if evidence.instance_id != "selected" {
            return Err("instance failed verification".to_string());
        }
        Ok(VerifiedInstanceInfo {
            instance_id: evidence.instance_id.clone(),
            e2e_pubkey: e2e_pubkey.to_string(),
            measurement_config: "8xh200 v1.3.0".to_string(),
            tcb_status: "UpToDate".to_string(),
            gpu_verdict: "PASS".to_string(),
        })
    }
}

async fn assert_report_preserves_gpu_evidence(rejected_first: bool) {
    let server = MockServer::start().await;
    let verifier = Arc::new(ReportVerifier::default());
    let nonce = "ab".repeat(32);
    let pubkey =
        base64::engine::general_purpose::STANDARD.encode([7u8; report_data::ML_KEM_768_PUBKEY_LEN]);
    let gpu_evidence: Vec<Value> = (0..8)
        .map(|i| {
            json!({
                "arch": "HOPPER",
                "certificate": base64::engine::general_purpose::STANDARD
                    .encode(format!("selected GPU {i} certificate")),
                "evidence": base64::engine::general_purpose::STANDARD
                    .encode(format!("selected GPU {i} evidence")),
            })
        })
        .collect();
    let selected_evidence = json!({
        "instance_id": "selected",
        "quote": "c2VsZWN0ZWQgcXVvdGU=",
        "certificate": "c2VsZWN0ZWQgY2VydGlmaWNhdGU=",
        "gpu_evidence": gpu_evidence,
    });
    let mut instances = vec![json!({
        "instance_id": "selected",
        "e2e_pubkey": format!("  {pubkey}  "),
    })];
    if rejected_first {
        instances.insert(
            0,
            json!({"instance_id": "rejected", "e2e_pubkey": "cmVqZWN0ZWQ="}),
        );
    }

    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "test-model-TEE", "chute_id": "test-chute"}],
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/e2e/instances/test-chute"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"instances": instances})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/chutes/test-chute/evidence"))
        .and(query_param("nonce", nonce.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            // Keep another instance first to catch accidental array-index selection.
            "evidence": [{
                "instance_id": "rejected",
                "quote": "cmVqZWN0ZWQgcXVvdGU=",
                "certificate": "cmVqZWN0ZWQgY2VydGlmaWNhdGU=",
                "gpu_evidence": [{
                    "arch": "BLACKWELL",
                    "certificate": "b3RoZXIgY2VydGlmaWNhdGU=",
                    "evidence": "b3RoZXIgZXZpZGVuY2U=",
                }],
            }, selected_evidence],
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = Provider::new(
        Config::new("test-key".to_string(), "test-model-TEE".to_string(), 5)
            .with_hosts(server.uri(), server.uri())
            .with_canonical_id("test-model"),
        verifier.clone(),
    )
    .unwrap();
    let report = provider
        .get_attestation_report(
            "test-model".to_string(),
            None,
            Some(nonce.to_ascii_uppercase()),
            None,
            false,
        )
        .await
        .unwrap();

    assert_eq!(report["provider"], "chutes");
    assert_eq!(report["model"], "test-model");
    assert_eq!(report["verified"], true);
    assert_eq!(report["gpu_verdict"], "PASS");
    assert_eq!(report["instance_id"], selected_evidence["instance_id"]);
    assert_eq!(report["quote_b64"], selected_evidence["quote"]);
    assert_eq!(report["certificate_b64"], selected_evidence["certificate"]);
    assert_eq!(report["nonce"], nonce);
    assert_eq!(report["e2e_pubkey"], pubkey);
    assert_eq!(
        report.get("gpu_evidence"),
        Some(&selected_evidence["gpu_evidence"])
    );

    let mut expected_calls = Vec::new();
    if rejected_first {
        expected_calls.push((
            "rejected".to_string(),
            nonce.clone(),
            "cmVqZWN0ZWQ=".to_string(),
        ));
    }
    expected_calls.push(("selected".to_string(), nonce, pubkey));
    assert_eq!(*verifier.calls.lock().unwrap(), expected_calls);
    server.verify().await;
}

#[tokio::test]
async fn report_preserves_all_gpu_entries_and_instance_bindings() {
    assert_report_preserves_gpu_evidence(false).await;
}

#[tokio::test]
async fn report_preserves_gpu_evidence_after_rejected_candidate() {
    assert_report_preserves_gpu_evidence(true).await;
}
