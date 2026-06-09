//! Attested inference providers: backends that present verifiable TEE
//! attestation. Today: NEAR AI's own fleet (`nearai`). Future: attested 3p
//! (e.g. Chutes).

pub mod nearai;
