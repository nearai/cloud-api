//! Attested inference providers: backends that present verifiable TEE
//! attestation. Today: NEAR AI's own fleet (`nearai`) and Chutes (`chutes`,
//! data-path skeleton behind a hard-off gate — verifier wired in a later PR).

pub mod chutes;
pub mod nearai;
