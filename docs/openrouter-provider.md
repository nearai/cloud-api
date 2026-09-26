# OpenRouter provider discovery

Use `GET /v1/openrouter/models` as the provider-monitor discovery URL. Chat
completions remain at `POST /v1/chat/completions` with the normal Bearer API key.
The discovery route is public and returns OpenRouter schema version 2.4. It lists
only the active canonical model `z-ai/glm-5.3-flash`; adding other models to the
Cloud catalog does not add them to this feed. `/v1/models` retains its existing
OpenAI-compatible response for Cloud clients.

Prices, limits, capabilities, model identity, and datacenter country codes come
from the Cloud model catalog. Per-token USD prices remain decimal strings.
Text and image prompt tokens share the existing prompt/cache tariff; the legacy
zero per-image surcharge is not represented as free image inference. The route
rejects external deployments, tiered pricing, and unsupported modalities rather
than advertising a misleading flat-price contract. This initial adapter does not
advertise throughput, numeric sampling bounds, media formats, or certifications
that the catalog cannot substantiate.

The model stays hidden (`is_ready: false`) by default. Launch requires all of:

- `OPENROUTER_GLM53_FLASH_READY=true` in the API server environment.
- `OPENROUTER_GLM53_FLASH_ZDR=true` after verifying the complete serving path.
- The model's catalog `isReady` explicitly set to true.
- Nonempty, verified `datacenters` metadata on the model.

Only the literal environment values `true` and `false` declare ZDR; missing or
unrecognized values omit `compliance` and keep the model hidden. This setting is
a declaration, not a control that disables storage. No HIPAA claim is inferred.
Environment changes require a server restart/redeployment. Catalog changes have
the existing model-cache and HTTP-cache propagation delay.

Before launch, verify the staged endpoint against OpenRouter's current
[provider schema](https://openrouter.ai/docs/assets/provider-monitor-schema-v2.openapi.json),
exercise representative completions, streaming usage, tool calls, JSON output,
vision, cache pricing, and canceled-stream billing, then repeat after the approved
production deployment. Confirm invoicing and OpenRouter's provider-side baseline
tests separately. Setting `is_ready: false` skips baseline tests on newly staged
OpenRouter endpoints; coordinate test/launch timing before enabling readiness.

```sh
cargo test -p api --lib routes::openrouter
```

The test `owns_pricing_by_modality_and_preserves_cache_price` can emit its actual
serialized document to `OPENROUTER_TEST_DOCUMENT_PATH` for an independent JSON
Schema validator. The implementation follows the
[provider integration contract](https://openrouter.ai/docs/guides/community/for-providers).
