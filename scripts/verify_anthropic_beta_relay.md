# Native Anthropic beta relay rollout verification

`verify_anthropic_beta_relay.py` sends small synthetic requests. It checks both
Messages routes, complete streaming responses, usage, and router versus upstream
errors. It prints only fixed diagnostics and HTTP status codes. Credentials come
from `--api-key-file` or `API_KEY`; response bodies are never printed or saved.
Exit status is 0 only when every probe passes (1 for failed probes, 2 for invalid
configuration). Python 3.10+ is sufficient; no packages are needed.

The default compatibility header is the same as `test_anthropic_cache_walk.py`:
the 12-token client set plus its conditional thirteenth token. `ANTHROPIC_BETA`
can override that header. Use a model supported by the environment's native
Anthropic route.

## Staging sequence

1. Deploy the relaying image and the Compose configuration that forwards
   `ANTHROPIC_DENIED_BETAS`. Retain `ANTHROPIC_ALLOWED_BETAS` until the rollback
   image also relays beta tokens. Record the actual deployed digest and replica
   inventory; a green probe against a load balancer does not prove every replica.
2. With the temporary test token absent from the denylist, run:

   ```sh
   python3 scripts/verify_anthropic_beta_relay.py \
     --api-key-file /path/to/staging-key --model anthropic/claude-sonnet-4-6
   ```

3. In staging only, temporarily add `not-a-real-beta-2099-01-01` to
   `ANTHROPIC_DENIED_BETAS` in the deployment environment, preserving any existing
   entries. Apply it through the normal rolling update. Check the rendered
   cloud-api service environment contains the forwarding entry, and confirm the
   intended configuration reached each replica. The verifier never changes
   configuration or deploys an image.
4. Run the same command with `--phase denylist`. It requires router-origin policy
   errors for both lowercase and uppercase spellings on both routes, while the
   regular compatibility requests and body restrictions still pass.
5. Restore the previous denylist and roll staging again. Run `--phase relay`
   again. The synthetic unknown token must once more reach upstream and return
   an upstream-origin error. Perform restoration even if the denylist phase
   fails; keep baseline, enabled, and restored results separately.

For relay checks against individual replicas, `--api-url` accepts an HTTPS
origin. Record which replica each run reaches using the deployment's supported
routing. A run against the default public URL qualifies only that request path.
The temporary denylist phase deliberately accepts only the staging public URL;
configuration readback must establish the setting on every replica.

## Production and interpretation

Only after the staging sequence and a separately authorized rollout, repeat the
relay check with `--api-url https://cloud-api.near.ai --allow-production` and the
production key file. Do not enable the temporary denylist in production.

Success requires JSON Messages with text, a successful stop reason and valid
usage; SSE `message_start`, text content, final `message_delta` usage and
`message_stop` with no error or unfinished content block; and positive token
counting. Error checks require the expected JSON category and reason, plus the
presence of `request-id` for an upstream rejection and its absence for a router
rejection. Messages must also expose `inference-id`.

These checks validate response semantics and usage fields, not persisted billing
records. Run a real client smoke test as an additional rollout check. The old
allowlisting image is expected to fail the relay suite; that result is a baseline,
not qualification of the new image.

Offline regression tests, also required by CI:

```sh
python3 -B -m unittest discover -s scripts/tests -v
```
