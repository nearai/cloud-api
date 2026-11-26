//! Reference measurements and minimum firmware versions for dstack SNP guests.
use anyhow::{ensure, Context, Result};
use dstack_types::mr_config::MrConfigV3;
use serde::Deserialize;
use sev_snp_qvl::{
    AmdKdsClient, AmdSnpCpuid, AmdSnpTcbRequirement, AmdSnpTcbVersion, QuoteVerifier,
    VerifiedAmdSnpReport,
};

use super::dstack::SnpEvidence;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    launches: Vec<Launch>,
    minimum_tcbs: Vec<MinimumTcb>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Launch {
    #[serde(with = "hex::serde")]
    measurement: [u8; 48],
    #[serde(with = "hex::serde")]
    os_image_hash: [u8; 32],
    #[serde(with = "hex::serde")]
    compose_hash: [u8; 32],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MinimumTcb {
    #[serde(with = "hex::serde")]
    cpuid: [u8; 4],
    bootloader: u8,
    tee: u8,
    snp: u8,
    microcode: u8,
    // Only processors with an FMC security version expose this component.
    fmc: Option<u8>,
}

pub(super) struct SnpVerifier {
    quotes: QuoteVerifier,
    kds: AmdKdsClient,
    launches: Vec<Launch>,
}

pub(super) struct SnpClaims {
    pub report_data: [u8; 64],
    pub os_image_hash: String,
    pub compose_hash: String,
    pub tcb_status: String,
}

impl SnpVerifier {
    pub fn from_env() -> Result<Self> {
        let path = std::env::var_os("DSTACK_SNP_POLICY_FILE")
            .context("DSTACK_SNP_POLICY_FILE is required for AMD model attestations")?;
        let file = std::fs::File::open(path).context("cannot open DSTACK_SNP_POLICY_FILE")?;
        Self::new(serde_json::from_reader(file).context("invalid DSTACK_SNP_POLICY_FILE")?)
    }

    fn new(policy: Policy) -> Result<Self> {
        ensure!(
            !policy.launches.is_empty(),
            "no trusted SNP launch measurements configured"
        );
        ensure!(
            !policy.minimum_tcbs.is_empty(),
            "no minimum AMD TCBs configured"
        );
        let mut requirements = Vec::new();
        for min in policy.minimum_tcbs {
            let cpuid = AmdSnpCpuid::from_eax(u32::from_be_bytes(min.cpuid));
            ensure!(
                cpuid.family != 0x1a || min.fmc.is_some(),
                "Turin requires a minimum FMC version"
            );
            requirements.push(AmdSnpTcbRequirement {
                cpuid,
                min_tcb: AmdSnpTcbVersion {
                    fmc: min.fmc,
                    bootloader: min.bootloader,
                    tee: min.tee,
                    snp: min.snp,
                    microcode: min.microcode,
                },
            });
        }
        Ok(Self {
            quotes: QuoteVerifier::new_prod().with_tcb_requirements(requirements),
            kds: AmdKdsClient::new()?,
            launches: policy.launches,
        })
    }

    pub async fn verify(
        &self,
        evidence: &SnpEvidence,
        report_data: &[u8; 64],
    ) -> Result<SnpClaims> {
        let report = self
            .quotes
            .fetch_and_verify(
                &self.kds,
                &evidence.report,
                &evidence.cert_chain,
                report_data,
            )
            .await?;
        self.claims(report, &evidence.mr_config)
    }

    fn claims(&self, report: VerifiedAmdSnpReport, document: &str) -> Result<SnpClaims> {
        ensure!(
            report.tcb_status == "UpToDate",
            "SNP CPU has no matching minimum TCB or is below it"
        );
        ensure!(
            MrConfigV3::snp_host_data_from_document(document) == report.host_data,
            "SNP HOST_DATA does not match mr_config"
        );
        let config = MrConfigV3::from_document(document)?;
        ensure!(config.version == 3, "unsupported SNP mr_config version");
        let launch = self
            .launches
            .iter()
            .find(|launch| {
                launch.measurement == report.measurement
                    && launch.compose_hash.as_slice() == config.compose_hash
            })
            .context("SNP launch measurement and compose hash do not match a trusted launch")?;
        Ok(SnpClaims {
            report_data: report.report_data,
            os_image_hash: hex::encode(launch.os_image_hash),
            compose_hash: hex::encode(launch.compose_hash),
            tcb_status: report.tcb_status,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sev_snp_qvl::AmdSnpTcbInfo;

    #[test]
    fn requires_current_tcb_and_binds_the_approved_launch_and_compose() {
        let verifier = SnpVerifier::new(serde_json::from_value(json!({
            "launches": [{"measurement":"01".repeat(48), "os_image_hash":"02".repeat(32), "compose_hash":"03".repeat(32)}],
            "minimum_tcbs": [{"cpuid":"00a00f11", "bootloader":1,"tee":1,"snp":1,"microcode":1}]
        })).unwrap()).unwrap();
        let document =
            json!({"version":3, "key_provider":"kms", "compose_hash":"03".repeat(32)}).to_string();
        let report = VerifiedAmdSnpReport {
            cpuid: None,
            measurement: [1; 48],
            report_data: [4; 64],
            host_data: MrConfigV3::snp_host_data_from_document(&document),
            chip_id: [0; 64],
            tcb_info: AmdSnpTcbInfo::default(),
            tcb_status: "UpToDate".into(),
        };
        let claims = verifier.claims(report.clone(), &document).unwrap();
        assert_eq!(claims.report_data, [4; 64]);
        assert_eq!(claims.os_image_hash, "02".repeat(32));
        assert_eq!(claims.compose_hash, "03".repeat(32));
        assert!(verifier
            .claims(report.clone(), &(document.clone() + " "))
            .is_err());
        for status in ["", "OutOfDate"] {
            assert!(verifier
                .claims(
                    VerifiedAmdSnpReport {
                        tcb_status: status.into(),
                        ..report.clone()
                    },
                    &document
                )
                .is_err());
        }
        assert!(verifier
            .claims(
                VerifiedAmdSnpReport {
                    measurement: [5; 48],
                    ..report.clone()
                },
                &document
            )
            .is_err());
        let other_compose = document.replace(&"03".repeat(32), &"04".repeat(32));
        assert!(verifier
            .claims(
                VerifiedAmdSnpReport {
                    host_data: MrConfigV3::snp_host_data_from_document(&other_compose),
                    ..report
                },
                &other_compose
            )
            .is_err());
    }
}
