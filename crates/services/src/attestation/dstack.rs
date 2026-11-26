//! CPU evidence from the dstack Attest RPC.
use std::io::Cursor;

use anyhow::{bail, ensure, Context, Result};
use cc_eventlog::{RuntimeEvent, TdxEvent};
use parity_scale_codec::Decode;
use serde::Deserialize;

const MAX_ATTESTATION_BYTES: usize = 10 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(tag = "kind", content = "data")]
pub(super) enum Platform {
    #[serde(rename = "tdx")]
    Tdx(TdxEvidence),
    #[serde(rename = "sev-snp")]
    Snp(SnpEvidence),
}

#[derive(Decode, Deserialize)]
pub(super) struct TdxEvidence {
    pub quote: Vec<u8>,
    pub event_log: Vec<TdxEvent>,
}

#[derive(Decode, Deserialize)]
pub(super) struct SnpEvidence {
    pub report: Vec<u8>,
    pub cert_chain: Vec<Vec<u8>>,
    pub mr_config: String,
}

pub(super) struct Evidence {
    pub platform: Platform,
    pub report_data: [u8; 64],
    pub runtime_events: Vec<RuntimeEvent>,
}

#[derive(Deserialize)]
struct Envelope {
    version: u64,
    platform: Platform,
    stack: Stack,
}

#[derive(Deserialize)]
#[serde(tag = "kind", content = "data")]
enum Stack {
    #[serde(rename = "dstack")]
    Dstack {
        report_data: Vec<u8>,
        runtime_events: Vec<RuntimeEvent>,
        #[serde(rename = "config")]
        _config: String,
    },
}

impl Evidence {
    pub fn decode(encoded: &str) -> Result<Self> {
        ensure!(
            encoded.len() <= MAX_ATTESTATION_BYTES * 2,
            "attestation exceeds size limit"
        );
        let bytes = hex::decode(encoded).context("invalid attestation hex")?;
        match bytes.as_slice() {
            [0, kind, tail @ ..] => {
                let mut rest = tail;
                let platform = match kind {
                    0 => Platform::Tdx(TdxEvidence::decode(&mut rest)?),
                    3 => Platform::Snp(SnpEvidence::decode(&mut rest)?),
                    _ => bail!("unsupported dstack SCALE platform"),
                };
                let runtime_events = Vec::<RuntimeEvent>::decode(&mut rest)?;
                let report_data = <[u8; 64]>::decode(&mut rest)?;
                String::decode(&mut rest)?;
                ensure!(rest.is_empty(), "trailing SCALE attestation data");
                Ok(Self {
                    platform,
                    report_data,
                    runtime_events,
                })
            }
            [0x80..=0x8f | 0xde | 0xdf, ..] => {
                let mut reader = Cursor::new(&bytes);
                let envelope: Envelope = rmp_serde::from_read(&mut reader)?;
                ensure!(
                    reader.position() == bytes.len() as u64,
                    "trailing MessagePack attestation data"
                );
                ensure!(
                    envelope.version == 1,
                    "unsupported dstack attestation version"
                );
                let Stack::Dstack {
                    report_data,
                    runtime_events,
                    ..
                } = envelope.stack;
                Ok(Self {
                    platform: envelope.platform,
                    report_data: report_data
                        .try_into()
                        .ok()
                        .context("report_data must be 64 bytes")?,
                    runtime_events,
                })
            }
            _ => bail!("unknown dstack attestation format"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parity_scale_codec::Encode;
    use serde_json::json;

    #[test]
    fn decodes_both_encodings_and_rejects_trailing_bytes() {
        let mut scale = vec![0, 0];
        (
            vec![1u8, 2],
            Vec::<TdxEvent>::new(),
            Vec::<RuntimeEvent>::new(),
            [3u8; 64],
            "",
        )
            .encode_to(&mut scale);
        let stack = json!({"kind":"dstack","data":{"report_data":vec![3;64],"runtime_events":[], "config":""}});
        let mut v1 = json!({"version":1, "platform":{"kind":"tdx","data":{"quote":[1,2],"event_log":[]}}, "stack":stack});
        for mut bytes in [scale, rmp_serde::to_vec_named(&v1).unwrap()] {
            let evidence = Evidence::decode(&hex::encode(&bytes)).unwrap();
            assert!(matches!(evidence.platform, Platform::Tdx(tdx) if tdx.quote == [1,2]));
            assert_eq!(evidence.report_data, [3; 64]);
            bytes.push(0);
            assert!(Evidence::decode(&hex::encode(bytes)).is_err());
        }
        for (field, value) in [
            ("stack", json!({"kind":"dstack-pod","data":stack["data"]})),
            ("version", json!(2)),
        ] {
            let mut unsupported = v1.clone();
            unsupported[field] = value;
            assert!(
                Evidence::decode(&hex::encode(rmp_serde::to_vec_named(&unsupported).unwrap()))
                    .is_err()
            );
        }
        v1["stack"]["data"]["report_data"] = json!([1]);
        assert!(Evidence::decode(&hex::encode(rmp_serde::to_vec_named(&v1).unwrap())).is_err());
    }
}
