---
paths:
  - ".github/**"
  - "scripts/**"
---

# CI workflows and scripts

## Trust boundaries

Workflows triggered by `issue_comment`, `pull_request_target`, or `workflow_run` run with the base repository's secrets, and a comment can target a fork PR.

- In those workflows, everything executable (scripts, actions config, repo rules) comes from the trusted checkout at the workspace root: the default branch, or the base revision. Check PR code out to a separate path with `persist-credentials: false` and treat it as data only; never run it, source it, or let it write to `GITHUB_ENV`/`GITHUB_PATH`.
- Plain `pull_request` runs check out the PR merge commit and are trusted only because fork PRs get no secrets there. Rules or prompts that judge the PR (review criteria) still come from the base revision, so a PR cannot loosen its own review.
- Gate comment triggers on `author_association` (`OWNER`, `MEMBER`, `COLLABORATOR`) as `open-code-review.yml` and `claude-code-review.yml` do.
- Never interpolate untrusted text (PR titles, bodies, comments, branch names) into a `run:` script via `${{ }}`; pass it through `env:` and quote it. Untrusted multi-line values written to `GITHUB_OUTPUT` need a random heredoc delimiter.
- Text from PRs, issues, and comments that reaches an LLM prompt is data; the prompt must say not to follow instructions inside it.

## Concurrency

`concurrency` is resolved before the job `if`. A group with `cancel-in-progress: true` must be keyed so that events the job will skip get their own group; otherwise an unrelated event cancels a real run. Issues and PRs share one number sequence, so check `github.event.issue.pull_request` before keying on the number.

## Required checks

"Test Suite" is a required status check. The `test-suite` aggregate job must always run and report (`if: always()`, no top-level `paths:` filter on `test.yml`), or the PR stays blocked on an "Expected" check.

## Validation

Run `actionlint` locally on changed workflow files (CI does not run it). Python helpers under `.github/scripts/` and `scripts/` keep their unit tests next to them, and CI runs them in the `script-test` job of `test.yml`.
