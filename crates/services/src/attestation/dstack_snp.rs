//! Reference measurements and minimum firmware versions for dstack SNP guests.
use anyhow::{ensure, Context, Result};
use dstack_types::mr_config::MrConfigV3;
use serde::Deserialize;
use sev::firmware::{guest::AttestationReport, host::TcbVersion};

use super::{
    dstack::SnpEvidence,
    snp::{kds::AmdKds, Tcb},
};

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

impl MinimumTcb {
    fn cpu(&self) -> (u8, u8, u8) {
        let eax = u32::from_be_bytes(self.cpuid);
        (
            ((eax >> 8 & 0xf) + (eax >> 20 & 0xff)) as u8,
            ((eax >> 16 & 0xf) << 4 | eax >> 4 & 0xf) as u8,
            (eax & 0xf) as u8,
        )
    }

    fn satisfied_by(&self, version: TcbVersion) -> bool {
        Tcb::from(version).meets(&Tcb {
            bootloader: self.bootloader,
            tee: self.tee,
            snp: self.snp,
            microcode: self.microcode,
        }) && self
            .fmc
            .is_none_or(|min| version.fmc.is_some_and(|fmc| fmc >= min))
    }
}

pub(super) struct SnpVerifier {
    kds: AmdKds,
    policy: Policy,
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
        for min in &policy.minimum_tcbs {
            ensure!(
                min.cpu().0 != 0x1a || min.fmc.is_some(),
                "Turin requires a minimum FMC version"
            );
        }
        Ok(Self {
            kds: AmdKds::new()?,
            policy,
        })
    }

    pub async fn verify(
        &self,
        evidence: &SnpEvidence,
        report_data: &[u8; 64],
    ) -> Result<SnpClaims> {
        let report = self
            .kds
            .verify(&evidence.report, &evidence.cert_chain)
            .await?;
        ensure!(
            &report.report_data == report_data,
            "SNP report_data mismatch"
        );
        self.claims(&report, &evidence.mr_config)
    }

    fn claims(&self, report: &AttestationReport, document: &str) -> Result<SnpClaims> {
        ensure!(
            report.vmpl == 0
                && report.key_info.signing_key() == 0
                && !report.key_info.mask_chip_key(),
            "dstack requires a VMPL0 report signed by an unmasked VCEK"
        );
        ensure!(
            (report.policy.smt_allowed() || !report.plat_info.smt_enabled())
                && (!report.policy.rapl_dis() || report.plat_info.rapl_disabled())
                && (!report.policy.ciphertext_hiding()
                    || report.plat_info.ciphertext_hiding_enabled()),
            "SNP platform does not satisfy guest policy"
        );
        let mut floors = self
            .policy
            .minimum_tcbs
            .iter()
            .filter(|min| {
                let (family, model, stepping) = min.cpu();
                (report.cpuid_fam_id, report.cpuid_mod_id, report.cpuid_step)
                    == (Some(family), Some(model), Some(stepping))
            })
            .peekable();
        ensure!(floors.peek().is_some(), "no minimum TCB for this SNP CPU");
        ensure!(
            floors.all(
                |min| [report.reported_tcb, report.committed_tcb, report.launch_tcb]
                    .into_iter()
                    .all(|version| min.satisfied_by(version))
            ),
            "SNP firmware below configured minimum"
        );
        ensure!(
            MrConfigV3::snp_host_data_from_document(document) == report.host_data,
            "SNP HOST_DATA does not match mr_config"
        );
        let config = MrConfigV3::from_document(document)?;
        ensure!(config.version == 3, "unsupported SNP mr_config version");
        let launch = self
            .policy
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
            tcb_status: "UpToDate".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requires_current_tcb_and_binds_the_approved_launch_and_compose() {
        let verifier = SnpVerifier::new(serde_json::from_value(json!({
            "launches": [{"measurement":"01".repeat(48), "os_image_hash":"02".repeat(32), "compose_hash":"03".repeat(32)}],
            "minimum_tcbs": [{"cpuid":"00a00f11", "bootloader":1,"tee":1,"snp":1,"microcode":1}]
        })).unwrap()).unwrap();
        let document =
            json!({"version":3, "key_provider":"kms", "compose_hash":"03".repeat(32)}).to_string();
        let tcb = TcbVersion {
            fmc: None,
            bootloader: 1,
            tee: 1,
            snp: 1,
            microcode: 1,
        };
        let report = AttestationReport {
            cpuid_fam_id: Some(0x19),
            cpuid_mod_id: Some(1),
            cpuid_step: Some(1),
            reported_tcb: tcb,
            committed_tcb: tcb,
            launch_tcb: tcb,
            measurement: [1; 48],
            report_data: [4; 64],
            host_data: MrConfigV3::snp_host_data_from_document(&document),
            ..Default::default()
        };
        let claims = verifier.claims(&report, &document).unwrap();
        assert_eq!(claims.report_data, [4; 64]);
        assert_eq!(claims.os_image_hash, "02".repeat(32));
        assert_eq!(claims.compose_hash, "03".repeat(32));
        assert!(verifier.claims(&report, &(document.clone() + " ")).is_err());
        for changed in [
            AttestationReport {
                reported_tcb: TcbVersion::default(),
                ..report
            },
            AttestationReport {
                committed_tcb: TcbVersion::default(),
                ..report
            },
            AttestationReport {
                launch_tcb: TcbVersion::default(),
                ..report
            },
            AttestationReport {
                cpuid_step: Some(2),
                ..report
            },
            AttestationReport {
                measurement: [5; 48],
                ..report
            },
            AttestationReport { vmpl: 1, ..report },
        ] {
            assert!(verifier.claims(&changed, &document).is_err());
        }
        let other_compose = document.replace(&"03".repeat(32), &"04".repeat(32));
        assert!(verifier
            .claims(
                &AttestationReport {
                    host_data: MrConfigV3::snp_host_data_from_document(&other_compose),
                    ..report
                },
                &other_compose
            )
            .is_err());
    }
}
