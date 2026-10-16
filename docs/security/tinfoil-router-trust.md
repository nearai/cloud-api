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

## Model pins added by an admin

A model release that is not yet in the compiled pins can be trusted without a deploy. An admin adds its Sigstore release bundle to the `tinfoil_trust` setting:

```bash
curl -X PATCH "$CLOUD_API/v1/admin/settings/tinfoil_trust" \
  -H "Authorization: Bearer $ADMIN_TOKEN" -H 'Content-Type: application/json' \
  --data @tinfoil-admin-pins.json
```

- **The bundle file.** The daily Tinfoil sync job attaches `tinfoil-admin-pins.json` to every run (`{"model_pins": [...]}`), dry runs included. You can also run the job on demand with `workflow_dispatch` and `dry_run=true`.
- **The PATCH replaces the whole `model_pins` list.** Merge with the current value from `GET /v1/admin/settings/tinfoil_trust` first.
- **What the PATCH checks.** Cloud API verifies every bundle before storing it. It must be a valid Sigstore attestation, signed by that `tinfoilsh/*` repository's release workflow at that tag.
- **What admission checks.** A model is then admitted only when the router's attested proxy document lists it with the same repository, tag and measurement registers as the signed release.
- **Limits.** At most 32 pins, and at most 1 MiB in all.
- **Switch.** `{"admin_model_pins": false}` turns admin pins off without removing them.

The model's attestation report shows `pin_source: "admin"` and the release's `rekor_log_index`.

## Public pins listing

`GET /v1/attestation/tinfoil/pins` needs no authentication. It returns three things:

- every compiled router and model pin;
- both switches;
- each admin model pin with its verified provenance: tag, Rekor log index, integrated time and measurements.

Raw bundles are not served.
