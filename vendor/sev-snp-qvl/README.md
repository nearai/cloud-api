# sev-snp-qvl

This crate contains dstack v0.6.6's AMD SEV-SNP quote verifier and its tests, from revision `c87303818753e7abb3233bd1f5a4b611cb981154`. The standalone manifest selects the OpenSSL backend of `sev` 8 so the verifier can share the workspace's vendored OpenSSL and build on both x86_64 and arm64.

The source changes import the `ByteParser` trait, use the array representation of `chip_id` and build the Linux certificate-table encoder test on Linux. One test borrows the TCB requirement for its assertion. The public roots, certificate and CRL checks, TCB assessment and quote verification come from upstream. [Upstream source](https://github.com/Dstack-TEE/dstack/tree/c87303818753e7abb3233bd1f5a4b611cb981154/dstack/sev-snp-qvl)

Run `cargo test --locked -p sev-snp-qvl` after updating the crate.
