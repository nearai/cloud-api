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

These settings apply to extra JSON body fields, such as `provider`, rather than
typed request fields such as `model` or `messages`. Fields outside the mandatory
configuration retain their existing semantics. Credentials belong in the
provider's secret configuration, never in either extra-body map.

Deploy support to every serving replica before enabling a model that requires
this policy: older binaries ignore unknown configuration fields. Verify both
streaming and non-streaming requests with conflicting caller preferences.

OpenRouter's [`zdr` filter](https://openrouter.ai/docs/guides/features/zdr)
restricts inference routing to eligible endpoints and fails if none are available.
It does not describe Cloud API conversation storage or separate plugin/tool
retention policies.
