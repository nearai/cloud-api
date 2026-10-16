# Attested provider runtime trust

Attested third-party backup providers (today Chutes and Tinfoil) are trusted through pins compiled into the binary. A provider may also accept runtime pins, added without a deploy, when Cloud API can prove them itself.

## The rule

Runtime pins are allowed only when Cloud API verifies signed release provenance in code, both at PATCH time and at admission.

- **Tinfoil qualifies.** Its releases carry Sigstore attestations, which Cloud API checks offline (see [Tinfoil router trust](tinfoil-router-trust.md)).
- **Chutes stays compiled-only.** Its golden-measurement feed is unsigned, so there is nothing to verify and no runtime pins.

## What is generic

- **One `admin_settings` key per provider** (Tinfoil: `tinfoil_trust`). The owning module defines the type, defaults and size cap. `SettingKey::validate_patch` is the proof hook: it verifies every pin and rejects the PATCH if one fails, so nothing unverified is stored.
- **An admission branch in the provider's verifier.** It consults the key's current value only after the provider's normal attestation checks, and re-verifies the pin's provenance.
- **A `ProviderPinsSource`** (`crates/services/src/attestation/pins_view.rs`) feeds the public `GET /v1/attestation/pins`. The response is an object keyed by `ProviderSource::as_str()`. Each section holds the compiled pins and the runtime trust (`null` if the provider has none). Raw bundles are never served. The body is cached for 30 seconds and rebuilt only when a source's `version` changes.
- **A provider-tagged metric**, `cloud_api.attested_3p.time_to_trust_seconds{provider, kind}`: seconds from a measurement first being rejected to its first admission.

## Adding a provider

1. Decide whether the rule above holds. If the provider has no signed release provenance that Cloud API can verify in code, stop: keep it compiled-only and go to step 5.
2. Define a `SettingKey` in the provider's module with the value type, defaults and size cap. Register it where the other keys are registered, before the provider's consumers.
3. Implement `validate_patch` to verify all provenance and reject on any failure. Add tests for a valid pin, a tampered pin and a pin for the wrong release.
4. Add an admission branch in the provider's verifier that uses the same verification function as `validate_patch`.
5. Implement `ProviderPinsSource` (`provider()` is the `ProviderSource` name; `version()` is `settings.version::<Key>()`, or 0 when compiled-only) and add it to the sources list in `build_attestation_routes` in `crates/api/src/lib.rs`.
6. Have the provider's session record rejection and admission with a `TrustTimer` and emit `cloud_api.attested_3p.time_to_trust_seconds` with its `provider:` tag.
7. Extend the pins e2e test with the new provider's section.
