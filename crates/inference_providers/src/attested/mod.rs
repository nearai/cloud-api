//! Attested inference providers: backends that present verifiable TEE
//! attestation. Today: NEAR AI's own fleet (`nearai`) and Chutes (`chutes`,
//! data-path skeleton behind a hard-off gate — verifier wired in a later PR) and
//! Tinfoil (`tinfoil`). `openai_wire` holds the OpenAI wire handling they share.

pub mod chutes;
pub mod nearai;
pub(crate) mod openai_wire;
pub mod tinfoil;
