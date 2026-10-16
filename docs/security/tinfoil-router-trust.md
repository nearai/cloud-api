# Tinfoil router trust

Cloud API sends Tinfoil-backed requests only to a router whose SEV-SNP attestation verifies and whose measurement Cloud API trusts. A router measurement is trusted in one of two ways:

| `pin_source` | Meaning |
|---|---|
| `compiled` | The measurement is in the compiled pins (`crates/services/src/attestation/tinfoil_golden_measurements.json`). |
| `sigstore_atc` | The measurement is not compiled. The ATC bundle for the router carried a Sigstore release attestation that met three conditions. (1) It verifies offline against the Sigstore public-good root. (2) It was signed by GitHub Actions running `.github/workflows/tinfoil-release-publish.yml` in `tinfoilsh/confidential-model-router` at a tag. (3) Its signed `snp_measurement` equals the router's live, AMD-verified measurement, and its subject digest equals the ATC `digest`. |

In both cases the SNP report chain, the debug policy, the minimum TCB, the binding of the report to the router's TLS key, and the router's domain checks run first.

## Kill switch

Operators can turn the Sigstore path off without a deploy. Routers are then trusted only through the compiled pins:

```bash
curl -X PATCH "$CLOUD_API/v1/admin/settings/tinfoil_trust" \
  -H "Authorization: Bearer $ADMIN_TOKEN" -H 'Content-Type: application/json' \
  --data '{"router_sigstore": false}'
```

The change takes effect immediately on the instance that handles the PATCH, and within 10 minutes on the others. Each Tinfoil session then re-verifies on its next 60-second tick, and a router admitted via `sigstore_atc` is dropped. To return to the default (on), send `{"router_sigstore": null}`.

## Verifying a `sigstore_atc` router yourself

The model attestation report (`GET /v1/attestation/report?model=<tinfoil-backed model>`) lists the router under the Tinfoil section, with `measurement`, `pin_source`, `tag` and `rekor_log_index`.

1. **Find the signed release in Rekor.** Look up the log entry: `rekor-cli get --log-index <rekor_log_index>` or `https://search.sigstore.dev/?logIndex=<rekor_log_index>`. Its signer identity must be `https://github.com/tinfoilsh/confidential-model-router/.github/workflows/tinfoil-release-publish.yml@refs/tags/<tag>`, with the issuer `https://token.actions.githubusercontent.com`.
2. **Check the release attestation.** Fetch it from `https://github.com/tinfoilsh/confidential-model-router/releases/tag/<tag>`, or with `gh attestation verify --repo tinfoilsh/confidential-model-router`. Its `snp_measurement` predicate field must equal the report's router `measurement`.
3. **Check the router's attestation.** Verify the router's SEV-SNP report independently, either with Tinfoil's verifier or by checking the AMD VCEK chain. Its measurement must be the same value.
