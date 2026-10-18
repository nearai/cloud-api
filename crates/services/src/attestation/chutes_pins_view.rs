//! Chutes' section of the public pins view served by
//! `GET /v1/attestation/pins`. Chutes accepts compiled pins only (its golden
//! feed is unsigned, so cloud-api cannot verify a runtime pin), hence
//! `runtime` is always `null`.

use serde::Serialize;

use super::chutes_pins::PinsFile;
use super::pins_view::ProviderPinsSource;

/// One compiled row: a software release's registers on one hardware sizing.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChutesCompiledRowView {
    pub name: String,
    pub version: String,
    pub mrtd: String,
    pub rtmr0: String,
    pub rtmr1: String,
    pub rtmr2: String,
    pub rtmr3: String,
}

pub fn chutes_compiled_view(pins: &PinsFile) -> Vec<ChutesCompiledRowView> {
    pins.rows()
        .map(|(f, row)| {
            let r = f.registers_for(row);
            ChutesCompiledRowView {
                name: row.name.clone(),
                version: f.version.clone(),
                mrtd: r.mrtd,
                rtmr0: r.rtmr0,
                rtmr1: r.rtmr1,
                rtmr2: r.rtmr2,
                rtmr3: r.rtmr3,
            }
        })
        .collect()
}

pub struct ChutesPinsSource;

impl ProviderPinsSource for ChutesPinsSource {
    fn provider(&self) -> &'static str {
        "chutes"
    }

    fn version(&self) -> u64 {
        0
    }

    fn public_view(&self) -> serde_json::Value {
        let compiled = match PinsFile::compiled() {
            Ok(pins) => chutes_compiled_view(&pins),
            Err(_) => {
                tracing::error!("chutes compiled pins unreadable for the public pins view");
                Vec::new()
            }
        };
        serde_json::json!({"compiled": compiled, "runtime": null})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_every_compiled_row_and_has_no_runtime_pins() {
        let pins = PinsFile::compiled().unwrap();
        let v = ChutesPinsSource.public_view();
        assert_eq!(v["compiled"].as_array().unwrap().len(), pins.rows().count());
        assert!(v["runtime"].is_null());
        let row = &v["compiled"][0];
        for k in [
            "name", "version", "mrtd", "rtmr0", "rtmr1", "rtmr2", "rtmr3",
        ] {
            assert!(row[k].is_string(), "{k}");
        }
        assert_eq!(ChutesPinsSource.version(), 0);
    }
}
