# Production releases

`Promote Image` is scheduled for **Thursday at 00:00 UTC** (`0 0 * * 4`).
GitHub may delay scheduled runs. It promotes the frozen `:staging` image to
`:prod`, records its digest, and publishes a `prod-YYYYMMDD-<shortsha>` tag and
GitHub release. After promotion succeeds, it dispatches `Update Cloud API Prod`
in `nearai/cvm-ansible-playbooks` and waits for its result.

Deployment resolves the latest `:prod` in the infrastructure repository; no
image digest is passed by the caller. An external tag change before resolution
can change the deployed image. Promotion and rollback share the
`prod-image-mutation` concurrency group, held while waiting for deployment.
The infrastructure workflow serializes production CVM updates separately and
retains its daily 02:00 UTC reconciliation schedule.

## Setup and verification

Deploy the infrastructure workflow's `release_id` input and run-name support
before enabling this caller. `REPOSITORY_DISPATCH_TOKEN` must have access to
`nearai/cvm-ansible-playbooks`, including Actions write permission for dispatch
and read permission for polling. Existing production environment protection
rules still apply; required approval can delay an otherwise automatic release.

Verifier pin validation remains enabled. The image commit must be present in
the verifier's accepted staging or production configuration before deployment.
The playbook performs per-instance health checks; the workflow additionally
checks `https://cloud-api.near.ai/v1/health` after rollout. Changed-release
notifications and the infrastructure summary record the deployed digest.

The caller summary links the exact deployment run. Failed, cancelled, or timed
out deployments fail the caller, exposing the failure in both repositories'
Actions runs. Operators should subscribe to workflow failure notifications.
The caller waits up to five minutes for run discovery and two hours for completion.
A timeout or cancellation of the caller does not cancel the infrastructure run:
inspect it before retrying or rolling back. A failed deployment does not
undo the image promotion or automatically roll back production.

## Manual promotion and deployment

Promote staging and deploy:

```sh
gh workflow run promote.yml --repo nearai/cloud-api -f target=prod
```

To promote a specific image, add `-f digest=sha256:<64 hex characters>`.
To retry only deployment of the current `:prod`:

```sh
gh workflow run update_cloud_api_prod.yml --repo nearai/cvm-ansible-playbooks --ref main
```

Follow the infrastructure Actions run through rollout and health verification.

## Rollback

Select a stamped production tag from the release history:

```sh
gh workflow run rollback.yml --repo nearai/cloud-api \
  -f confirm=ROLLBACK -f target_tag=prod-YYYYMMDD-abcdef0
```

Omit `target_tag` to select the most recently pushed production tag with a
different digest. Rollback moves `:prod`, writes an audit tag and release, then
uses the same dispatch-and-wait path. Check the linked deployment result and
health checks before considering rollback complete. If the tag already moved
but deployment failed, retry deployment directly instead of re-running rollback.
