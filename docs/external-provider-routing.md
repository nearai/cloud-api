# External provider routing policy

OpenAI-compatible models can configure extra request fields in `provider_config`.
`extra_request_body` supplies defaults that callers can override.
`enforced_request_body` applies mandatory fields after those defaults and caller
parameters. Objects merge recursively; configured leaves and arrays replace
caller values. A caller's null, scalar, or array cannot replace a configured object.
Models without this optional field retain existing behavior.

For example, this OpenRouter configuration restricts inference to a selected
provider's ZDR endpoints, without fallback:

```json
{
  "backend": "openai_compatible",
  "base_url": "https://openrouter.ai/api/v1",
  "enforced_request_body": {
    "provider": {
      "only": ["fireworks"],
      "allow_fallbacks": false,
      "zdr": true,
      "data_collection": "deny"
    }
  }
}
```

The initial policy surface supports only the extra JSON `provider` object.
The admin write path and provider loader reject policies on other backends,
non-object policies, and other top-level keys (including typed fields such as
`model` or `messages`, which could otherwise create duplicate JSON keys).
Audio transcription and image editing fail before dispatch when a mandatory
policy is configured, because multipart transport cannot carry these JSON fields.
Credentials belong in the provider's secret configuration, never in either map.

Caller fields outside the mandatory configuration retain their existing
semantics. For example, `provider.ignore` can exclude all allowed providers and
cause a request to fail; it cannot widen the permitted set or disable ZDR.

Deploy support to every serving replica before enabling a model that requires
this policy: older binaries ignore unknown configuration fields. Verify both
streaming and non-streaming requests with conflicting caller preferences. Apply
the policy to every provider configuration serving the model.

OpenRouter's [`zdr` filter](https://openrouter.ai/docs/guides/features/zdr)
restricts inference routing to eligible endpoints and fails if none are available.
It does not describe Cloud API conversation storage or separate plugin/tool
retention policies.
