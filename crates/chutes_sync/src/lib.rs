//! Daily Chutes measurement sync, run by
//! `.github/workflows/chutes-measurements-sync.yml`.
//!
//! The probe ([`probe`]) asks every Chutes TEE chute for fresh evidence and
//! records the registers of each instance whose quote passes the Intel
//! signature chain, TCB floor, debug-bit check, report_data bindings and
//! NVIDIA NRAS. The classifier ([`classify`]) then adds a row to the compiled
//! pins file (`services::attestation::chutes_pins`) only if all five registers
//! equal a row Chutes publishes. Rows are never removed, and a row whose
//! runtime RTMR3 is all zeros is never added. A person reviews and merges the
//! resulting PR; this crate is tooling and is not part of the API image.

pub mod classify;
pub mod report;
