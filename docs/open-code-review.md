# OpenCodeReview dogfooding

The OpenCodeReview workflow builds the NEAR AI provider from the immutable
`OCR_REF` commit in `nearai/open-code-review` while the integration awaits an
upstream release. The current pin includes the provider changes in
[alibaba/open-code-review#1758](https://github.com/alibaba/open-code-review/pull/1758).

## Configuration

- `OCR_LLM_AUTH_TOKEN` remains the API-key secret. It must contain a production
  NEAR AI API key; the workflow passes it as `NEARAI_API_KEY` only to the review
  step, without writing it to OCR's configuration file.
- Requests use `https://cloud-api.near.ai/v1`.
- The model defaults to `z-ai/glm-5.3-flash`. Set the repository variable
  `OCR_NEARAI_MODEL` to choose another canonical model ID with E2EE and model-TEE
  support. Unsupported models fail rather than weakening the policy.
- The legacy `OCR_LLM_URL` and `OCR_LLM_MODEL` secrets are no longer used by this
  workflow. E2EE and `model-tee` verification are explicitly required.

The existing triggers remain: same-repository PRs opened or marked ready, and
`/ocr` comments from owners, members, or collaborators. Comment-triggered reviews
can include fork PRs. The workflow executes only the trusted workflow revision
and pinned OCR source; it fetches the PR commits as data without checking out or
executing PR code. Review rules come from the trusted checkout.

## Results and rollout

Publication requires a zero OCR exit code, complete review coverage, at least one
verified completion, and no failed, pending, or cancelled requests. A no-op review
is explicitly reported as skipped, without claiming inference verification. The
workflow checks the PR head again before posting to avoid publishing a review
against a newer commit.

The Actions step summary records the verification count and policy. Result JSON
and stderr are retained as workflow artifacts for seven days; these can contain
source code and review content and are not encrypted by inference E2EE.

Because these triggers use the base/default-branch workflow, this PR's new flow
must be merged before `/ocr` can exercise it. After merging, trigger `/ocr` on a
small PR and confirm the verification summary and posted review. No live
inference is performed by the local workflow tests.

To update the dogfood build, change `OCR_REF` to a reviewed commit and rerun
`actionlint` plus the Python script tests. Once the NEAR AI provider is released
upstream, replace the source build with a pinned release that retains the same
verification and publication checks.
