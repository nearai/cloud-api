//! Keyless observation of Chutes instances for the daily measurement sync
//! (`crates/chutes_sync`). Kept apart from the serving verifier in
//! [`super::chutes`]; both share its quote and GPU stages.

use inference_providers::attested::chutes::attestation as transform;
use inference_providers::attested::chutes::evidence::PublicInstanceEvidence;
use inference_providers::attested::chutes::report_data::{PublicEvidenceVerifier, ReportDataError};

use super::chutes::{shared_stage_verifier, verify_gpu, verify_quote, ChutesVerifyError};
use super::chutes_pins::Registers;
use super::verification::AttestationVerifier;

/// Records which register sets genuine Chutes instances run, for the daily
/// measurement sync, from Chutes' **public** evidence (no API key).
///
/// It runs the same DCAP quote and NVIDIA NRAS checks as
/// [`super::chutes::ChutesBackendVerifier`], but the freshness proof differs:
/// the public evidence carries no E2EE key, so instead of `report_data[0:32]`
/// the observer checks that the quote binds the instance certificate and that
/// the certificate's key signed a body containing our nonce and this quote
/// ([`PublicEvidenceVerifier`]). The GPU evidence is checked against
/// `report_data[0:32]`, the nonce Chutes bound it to.
///
/// It has no allow-list and does not implement `ChutesInstanceVerifier`, so it
/// can never be handed to the provider pool to serve traffic.
pub struct ChutesObserver {
    inner: AttestationVerifier,
}

impl ChutesObserver {
    pub fn new(pccs_url: Option<String>) -> Self {
        Self {
            inner: shared_stage_verifier(pccs_url),
        }
    }

    /// Verify one instance's public evidence and return its five registers.
    pub async fn observe_instance(
        &self,
        evidence: &PublicInstanceEvidence,
        nonce: &str,
    ) -> Result<Registers, ChutesVerifyError> {
        // Without the signed body there is no freshness proof; refuse before
        // any network call.
        let (Some(body), Some(signature)) = (&evidence.attested_body, &evidence.signature) else {
            return Err(ReportDataError::SignedBody(
                "public evidence has no attested_body/signature".to_string(),
            )
            .into());
        };
        let instance = &evidence.evidence;
        let q = verify_quote(&self.inner, instance).await?;
        let cert_der = transform::certificate_der(instance)?;
        PublicEvidenceVerifier.verify(
            &q.report_data,
            nonce,
            &cert_der,
            &instance.quote,
            body,
            signature,
        )?;
        verify_gpu(&self.inner, instance, &hex::encode(&q.report_data[..32])).await?;
        Ok(Registers {
            mrtd: hex::encode(q.mrtd),
            rtmr0: hex::encode(q.rtmr0),
            rtmr1: hex::encode(q.rtmr1),
            rtmr2: hex::encode(q.rtmr2),
            rtmr3: hex::encode(q.rtmr3),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inference_providers::attested::chutes::evidence::InstanceEvidence;

    fn public_evidence(quote: &str, signed: bool) -> PublicInstanceEvidence {
        PublicInstanceEvidence {
            evidence: InstanceEvidence {
                quote: quote.to_string(),
                gpu_evidence: vec![],
                instance_id: "inst-1".to_string(),
                certificate: "Y2VydA==".to_string(),
            },
            signature: signed.then(|| "c2ln".to_string()),
            attested_body: signed.then(|| "Ym9keQ==".to_string()),
        }
    }

    #[tokio::test]
    async fn observer_reaches_transform_without_any_measurement_policy() {
        // The observer has no allow-list at all: on a malformed quote it fails
        // in the transform stage, before any network call, and labels it so.
        let err = ChutesObserver::new(None)
            .observe_instance(
                &public_evidence("!!! not base64 !!!", true),
                &"a".repeat(64),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ChutesVerifyError::Transform(_)));
        assert_eq!(err.stage(), "transform");
    }

    #[tokio::test]
    async fn observer_requires_the_signed_body_before_any_network_call() {
        // Without `attested_body` and `signature` there is no freshness proof,
        // so the observer refuses before fetching DCAP collateral.
        let err = ChutesObserver::new(None)
            .observe_instance(&public_evidence("BAACAIE=", false), &"a".repeat(64))
            .await
            .unwrap_err();
        assert!(matches!(err, ChutesVerifyError::ReportData(_)), "{err}");
        assert_eq!(err.stage(), "report_data");
    }
}
