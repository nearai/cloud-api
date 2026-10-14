# Scheduled model pricing notices

Successful inference responses include `x-model-pricing-change-date` when the
canonical model has a pending scheduled pricing change:

```http
x-model-pricing-change-date: 2030-01-01T00:00:00Z
```

The header uses the earliest pending change's `effective_at`, formatted as a UTC
RFC 3339 timestamp. It uses the same middleware and routes as
`x-model-deprecation-date` and `x-model-successor`, including streaming responses
and aliases. Pricing notices are independent of deprecation notices. The body is
unchanged, non-2xx responses omit the notice, and the existing CORS configuration
exposes the header to browser clients.

## Catalog fields and units

`GET /v1/models` adds an optional `upcoming_pricing` next to the current `pricing`:

```json
{
  "upcoming_pricing": {
    "effective_at": "2030-01-01T00:00:00Z",
    "pricing": {
      "input": 3.0,
      "output": 2.0,
      "prompt": "0.000003",
      "completion": "0.000002",
      "image": "0",
      "request": "0",
      "input_cache_read": "0"
    }
  }
}
```

As with the existing current prices, `input` and `output` are USD per million
tokens; `prompt`, `completion` and `input_cache_read` are exact USD-per-token
strings; `image` is USD per image. When a tiered profile is configured,
`upcoming_pricing.textPricing` contains the complete versioned profile, in USD
per million tokens. The scalar fields retain the existing Standard-short
projection.

`GET /v1/model/list` (and the native model detail endpoint) instead adds
`upcomingPricing`, with `effectiveAt` and `pricing`. The latter uses the native
`inputCostPerToken`, `outputCostPerToken`, `cacheReadCostPerToken`, `costPerImage`
decimal objects (`amount` in nano-dollars, `scale: 9`, `currency: "USD"`) and
optional `textPricing`.

Both catalogs publish complete future prices: scheduled fields replace their
current counterparts, while unspecified fields retain the live model values.
Unspecified fields do not use the historical price snapshot taken at confirm.
An absent cache-read price remains absent; an explicitly free price remains
zero. The current pricing and billing continue to use the current model values
until the scheduler applies the change.

## Lifecycle and consistency

Only `pending` changes produce a notice. Cancelled, applying, applied and failed
changes do not. A pending change whose date has passed remains visible until its
status changes; elapsed time alone does not establish that pricing was applied.
When no change is pending, both the header and the optional catalog field are
omitted.

The pending metadata is selected in the same database query as the model and
stored in the existing model caches. The middleware reuses the shared resolve
cache, without a separate pricing lookup on each request. Confirm invalidates
both caches after persistence, before notification email delivery. Cancel and
scheduler transitions also invalidate both caches.

These are advisory notices with the existing cache consistency bounds. Cache
invalidation is local to the instance performing the write; other instances
refresh via the existing TTLs (30 seconds for resolution, 300 seconds for the
catalog). An in-flight load may also repopulate a locally invalidated cache.
Existing HTTP catalog cache directives are unchanged. As with deprecation
notices, a failed lookup or the middleware's 250 ms timeout leaves a successful
response unannotated.
