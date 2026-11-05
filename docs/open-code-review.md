# OpenCodeReview dogfooding

**On hold:** keep this PR unmerged while the private-repository pilot in [nearai-cloud-ui#465](https://github.com/nearai/nearai-cloud-ui/pull/465) is the priority.

Add a production NEAR AI API key as the repository Actions secret `PRIVATE_OCR_API_KEY`. Once installed on the base/default branch, the workflow automatically reviews same-repository, non-draft PRs when opened or marked ready for review (`pull_request_target`). An owner, member, or collaborator can also trigger a review with a PR comment starting with `/ocr`. New commits do not automatically rerun reviews; use `/ocr` to rerun.

The workflow fixes the API URL to `https://cloud-api.near.ai/v1` and the model to `deepseek-ai/DeepSeek-V4.1-Flash-1017-abliterated`. It builds OpenCodeReview from commit `bc197a163ac59cab372ab0ba0daa8fc7fc180b45` of `nearai/open-code-review`.

E2EE and `model-tee` verification are mandatory. OCR's NEAR AI provider verifies attestation before each completion request and verifies response signatures before returning results. There is no separate synthetic preflight or custom publication gate. A nonzero OCR exit stops publication; successful exits, including partial reviews, use OCR's standard comment publisher.

Only the review inference step receives the dedicated API key. The workflow checks out trusted base/default-branch scripts and rules, fetches PR commits as data, and does not execute PR code or build the application. Publication checks that the PR head has not changed.

Source is plaintext on the GitHub-hosted runner before encryption, and findings are posted to the private PR. E2EE protects inference transport to the verified model environment; it does not hide source from GitHub or the runner. Raw result and stderr files are not uploaded as artifacts and are removed after the run. OCR may also keep local session data on the ephemeral runner.

Validate the workflow with `actionlint .github/workflows/open-code-review.yml`. A live smoke test requires the secret and a merged workflow; post `/ocr` on a small PR and inspect the run and review results.
