# Model routes

A model route gives a model a named provider. The route links the model to a
second catalog entry, the target, under a provider name. A chat request for
the model is served by the model's own provider unless it names that
provider; it is then served as a request for the target entry.

```
model  vendor/model                  -> its own provider (the default)
route  vendor/model, name "alpha"    -> vendor/model-via-alpha
route  vendor/model, name "beta"     -> vendor/model-via-beta
```

## Naming a provider

```json
{
  "model": "vendor/model",
  "messages": [{"role": "user", "content": "Hi"}],
  "provider": {"only": ["alpha"]}
}
```

- Without `provider.only` the request is served by the model's own provider,
  exactly as for a model without routes. `null` and an empty list count as
  not set.
- With `provider.only` holding exactly one name the model has, the request
  becomes a request for that route's target entry. The target's upstream,
  request policy, price, access check and signature rules apply. The name is
  removed from the request before it is sent on; other `provider` fields are
  kept.
- Anything else in `provider.only` (a name the model does not have, several
  names, not a list) is refused with a 400 that lists the model's provider
  names.
- A route is never a fallback. If the named provider fails, its error is the
  answer; the model's own provider is not tried. If the model's own provider
  fails, no route is tried.

A model without routes is not affected: its `provider` object is passed to
its provider untouched. On a model with routes, `provider.only` means the
model's own provider names, so it can no longer carry names meant for the
model's upstream.

## What follows from running as the target

- Usage is recorded and priced on the target entry, under the target's name.
- The caller needs access to the model and to the target. A target the
  caller could not request directly cannot be reached through a route, and
  its provider name is neither offered nor acknowledged. A caller who can
  use none of a model's routes gets a 400 for any name, with none listed.
- The response is what the target would return for a direct request,
  including its `model` field, and it can be verified only as the target can.
  A target without attestation gives an unattested answer, also when the
  model itself is attested.
- The model must be requested by its canonical name to name a provider. An
  alias with `provider.only` is refused with a 400 that names the canonical
  model.

## Response header

`/v1/chat/completions` sets `x-model-provider: <name>` on a successful
response that the named provider served. It is absent on error responses and
whenever the model's own provider served.

Check the header to confirm the provider. A name is passed through without
effect, and the model's own provider serves, when the model has no enabled
route: routes are off, the route is disabled, or it was enabled less than
one refresh interval ago.

## Listing a model's providers

`GET /v1/model/{model_name}` lists the named providers of the model in
`providers`, ordered by name:

```json
"providers": [
  {
    "name": "alpha",
    "verifiable": false,
    "attestationSupported": false,
    "contextLength": 128000,
    "inputCostPerToken": {"amount": 300, "scale": 9, "currency": "USD"},
    "outputCostPerToken": {"amount": 1200, "scale": 9, "currency": "USD"}
  }
]
```

The provider that serves a request without a name is not in the list: the
model's own fields describe it. `cacheReadCostPerToken` is present when the
target has cache pricing. The endpoint is public, so a route whose target is
not a public model is left out. The field is absent when there is no provider
to list, and `GET /v1/model/list` does not carry it.

## Scope

Only `/v1/chat/completions` reads `provider.only` this way. On other
endpoints the field keeps its existing meaning.

## Configuration

`MODEL_ROUTES_ENABLED` (`true`/`1`, default off) loads the enabled routes at
startup and on every provider refresh (`EXTERNAL_PROVIDER_REFRESH_INTERVAL`).
With the setting off the routes are never read and no request is changed. A
failed reload keeps the routes loaded last. Until the routes have loaded
once, a request that names a provider in `provider.only` is answered with a
500, for every model.
