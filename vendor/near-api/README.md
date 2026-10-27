# near-api zstd dependency patch

Source: crates.io `near-api` 0.8.6, upstream commit
`14df71e8e0ecf1f1f0afb4385dea6e7ebd57aea1` (`api/`).
Download: <https://crates.io/api/v1/crates/near-api/0.8.6/download>.
The original crate archive SHA-256 is
`7d40379fdb1c7b3fec53512150c0dc97ee2fbcb1ecf2710158edb1e157bafb36`.

All Rust source and examples are unchanged. The only change to the normalized
upstream `Cargo.toml` is the `zstd` requirement, from `0.13` to `0.14.3`.
`Cargo.toml.orig` and `CHANGELOG.md` are preserved from the published crate;
LICENSE-MIT and LICENSE-APACHE come from the upstream commit above.
The upstream crate's standalone lockfile and Cargo registry metadata are omitted.
The workspace lockfile controls the resolved dependencies.

The fixed dependency chain is:

- `zstd` 0.14.3: RUSTSEC-2026-0391 (data leakage when reusing an encoder).
- `zstd-safe` 8.2.0: RUSTSEC-2026-0398 (seekable decompression bounds) and
  RUSTSEC-2026-0403 (uninitialized memory in legacy-frame decompression).
- `zstd-sys` 2.1.2: RUSTSEC-2026-0410 (WebAssembly allocation failure).

`near-api` uses `zstd::decode_all` to read compressed contract ABIs. That API is
retained by the updated dependency. Keeping the library source unchanged also
preserves the signing, account, and staking APIs used by cloud-api.

Remove this patch and workspace exclusion when an upstream `near-api` release
uses the fixed dependency chain. Regenerate Cargo.lock and run both `cargo audit`
and `cargo deny --all-features check advisories` before removing it.
