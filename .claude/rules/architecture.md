---
paths:
  - "crates/**/*.rs"
---

# Architecture and design

## Layers (ports and adapters)

`crates/api/src/routes/` (HTTP adapters) call service traits; services in `crates/services/src/<domain>/` hold domain logic and depend only on traits declared in that domain's `ports.rs`; `crates/database/src/repositories/` implements those traits. This keeps services testable with fakes (SQL, cache, or mock implementations of the same trait) and free of Axum and SQL types.

- Services take `Arc<dyn SomeRepository>`, never a concrete database type. `crates/services/` does not import the `database` crate.
- New routes call a service trait, not a repository. Some existing route modules (admin, auth, services, feature requests) still hold concrete repositories in their state; do not add more, and move logic into a service when you change one of them substantially.
- New persistence goes behind a trait in the owning domain's `ports.rs`, implemented in `crates/database/src/repositories/`.
- New service: `crates/services/src/<name>/` with `ports.rs` plus `mod.rs`/`service.rs`, exported from `crates/services/src/lib.rs`.
- New endpoint: handler in `crates/api/src/routes/`, wired into the matching `build_*_routes` function in `crates/api/src/lib.rs`, with `utoipa` annotations so it appears in the OpenAPI spec.

## Extend the existing seam

Most changes fit something the repo already does. Extend it rather than adding a parallel path:

| Need | Existing seam |
|---|---|
| New inference backend | implement `InferenceProvider` (`crates/inference_providers/src/lib.rs`), validated and built by `create_external_provider` in `crates/services/src/inference_provider_pool/mod.rs`; `ProviderSource` is the closed list of backends |
| New per-chunk streaming side effect (metrics, billing) | the `InterceptStream` wrapper in `crates/services/src/completions/mod.rs` |
| New routing or placement policy | its own module beside `crates/services/src/inference_provider_pool/context_routing.rs`, not a flag inside the pool's `mod.rs` |
| New state with a closed set of values | a Rust enum with a strict parser; see `.claude/rules/database.md` |
| Many related parameters | a context struct, as `ProcessStreamContext` does in `crates/services/src/responses/service.rs` |

A new mode flag, a `match` arm on a provider name inside shared code, or a second function that does what an existing one does usually means the change belongs in the existing seam. The pattern vocabulary (Adapter, Decorator, Strategy, Parameter Object) and code-smell names follow https://refactoring.guru/design-patterns and https://refactoring.guru/refactoring/smells.

## Keep abstractions earned

Add a trait, generic, or wrapper only when it has two real implementations or a test seam that needs it now. Avoid one-call pass-through functions and parameters nothing reads, and prefer deleting an unused abstraction to keeping it "for later". Closed enums such as `ProviderSource` are a deliberate closed set, not something to generalize.

## Errors

API errors use `ErrorResponse` (`crates/api/src/models.rs`) with the standard types `unauthorized`, `forbidden`, `not_found`, `conflict`, `validation_error`. Services return typed errors; routes convert them (see `crates/api/src/conversions.rs` and `crates/api/src/routes/common.rs`). Keep the cause when converting: `map_err(|_| ...)` that discards the source error hides the real failure.

## Runtime configuration

Runtime-changeable config goes into `admin_settings` as an owner-registered key implementing `SettingKey` (`crates/services/src/admin_settings/registry.rs`), registered via `AdminSettingsService::builder(..).register::<K>(handle)`. Do not add a table, migration, or admin API for it.

- The owning module defines the key's type, defaults, parsing, and size cap; `admin_settings` knows no key's type.
- Read values through the published typed handle, never raw JSON.
- The table is plaintext: no secrets, credentials, or customer data. Signed public trust inputs are fine because the owner verifies them.
- A key with `deny_unknown_fields` may only gain a field once every running instance has a build that knows it.
- Consumers that need settings at startup register after the settings service loads.
- Runtime trust pins for attested backup providers follow `docs/security/attested-provider-runtime-trust.md`.

## File size

Keep new files under about 1,000 lines. Many existing files are already far larger (for example `crates/services/src/inference_provider_pool/mod.rs`); when a change adds a sizable new concern to one of them, put it in a new submodule along an existing seam instead of growing the file further.
