# Model routes

A model route adds another provider to a model. The route names a second
catalog entry, the target, and the target's own provider then also serves
requests for the model. The route itself carries no provider configuration:
the target keeps its upstream, model name and request policy.

```
model  vendor/model          -> its own provider (always tried first)
route  vendor/model -> vendor/model-via-b   priority 10
route  vendor/model -> vendor/model-via-c   priority 20
```

## Behavior

- The model's own provider serves while it can. A route target is tried only
  after the providers ahead of it fail before any response byte is sent, with
  an error another provider may not share: a 5xx, 408 or 429, a connection
  failure, or an upstream answer that it does not serve the model. Targets
  are tried in priority order, then by target name.
- A provider with repeated consecutive failures is tried after healthy ones,
  so a route target leads while the model's own provider is down. Speed never
  reorders them: a slower own provider still leads a faster target.
- Any other 4xx is returned as is and reaches no route target.
- Routes apply only between third-party models: the model and the target must
  both be served by non-attested providers. A route that involves an attested
  provider on either side is ignored.
- One hop: a target's own routes are not followed.
- Chat requests use routes, streaming or not: `/v1/chat/completions` and the
  endpoints that are served through chat completions. Image, audio and the
  provider-native request paths use the model's own provider only.
- A request that pins a signing key, or whose organization turned provider
  fallback off, gets no route targets.
- Usage is recorded and priced on the requested model.

## What a route does not check

A route sends the model's requests to the target's upstream under the
target's configuration. Before enabling one, make sure that:

- the target's request policy (`enforced_request_body`) is at least as strict
  as the model's;
- the target serves the same upstream model, with the capabilities the
  model's catalog entry advertises;
- reaching the target through the model is intended for every organization
  that can use the model. Access is checked on the requested model only, so a
  target that is not listed is still reachable through its route.

## Response header

`/v1/chat/completions` sets `x-serving-route: <route id>` when a route target
served the response. The header is absent when the model's own provider
served.

## Configuration

`MODEL_ROUTES_ENABLED` (`true`/`1`, default off) loads the enabled routes at
startup and on every provider refresh (`EXTERNAL_PROVIDER_REFRESH_INTERVAL`).
With the setting off the routes are never read and provider selection is
unchanged. A failed reload keeps the routes loaded last.
