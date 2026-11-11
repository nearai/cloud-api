# approval-guard: setup

**Rule:** a PR into the default branch can merge only if someone who did **not** touch the PR approved its current head commit.

"Touched" means any of:
- the PR author;
- any commit author or committer;
- anyone who pushed to the PR branch, as recorded in an unbroken push record from branch creation to the current head (a push that changed nothing may be missing while GitHub's activity log lags);
- whoever created a commit GitHub signed on their behalf (web UI / API commits), even on another branch that was later merged in.

Approvals from those people are shown in the check but don't count.

## Files per repo

| File | What it does |
|---|---|
| `.github/workflows/approval-guard-trigger.yml` | Runs on PR and review events. It has no permissions and no secrets, and only wakes the handler. |
| `.github/workflows/approval-guard.yml` | `workflow_run` handler. It always runs the default branch's copy and never runs PR code. It gets an App token, evaluates the PR, and posts the `approval-guard` check. |
| `.github/approval-guard/guard.py` | The rule. Standard library only. |
| `.github/approval-guard/test_guard.py` | Unit tests: `python3 .github/approval-guard/test_guard.py`. |

## One-time org setup (org admin)

1. **Create the GitHub App `approval-guard`.**
   - Org-owned, private, no webhook.
   - Repository permissions: Checks read/write, Pull requests read, Contents read, Metadata read.
   - Install it on each repo that uses the check (*Only select repositories*). Without it the token step fails.
2. **Generate one private key** for the App.

## Per repo (repo admin)

1. **Settings → Environments → New `approval-guard`.**
   - Deployment branches and tags: *Selected branches* → the default branch only (`main`, or `master` on compose-manager).
   - Secret `APPROVAL_GUARD_APP_KEY`: the App's private key (PEM).
   - Variable `APPROVAL_GUARD_CLIENT_ID`: the App's Client ID.
   - Deployment-branch rule type must be **branch**, not tag.
   - Admin bypass off: uncheck *Allow administrators to bypass configured protection rules*.
   - Do this **before** merging step 2. A job that names a missing environment makes GitHub create it with no branch rule.
2. **Merge the PR that adds the files above.**
3. **Report-only period:** the check posts on every PR but is not required yet. Watch it for a few days; it should say `success` wherever a non-contributor approved.
4. **Restrict review dismissal:** branch protection → *Require a pull request before merging* → *Restrict who can dismiss pull request reviews* with nobody added (admins only). Otherwise a writer can dismiss a reviewer's "changes requested" while stopping the guard's re-check, and merge on an earlier success.
5. **Enforce:** branch ruleset on the default branch → *Require status checks* → `approval-guard`, with source set to the **approval-guard App** (not "any source"). Leave the bypass list empty.

## What it blocks / doesn't

| Case | Result |
|---|---|
| B pushes to A's PR, then B approves (even if A pushes again afterwards) | blocked: B is a pusher/committer |
| B commits through the API with A as the author | blocked: the activity log shows B pushed |
| B makes that API commit on another branch **in this repo** and A merges or fast-forwards to it | blocked: every push that ever pointed at that commit counts, including B's (assumes GitHub keeps activity for deleted branches; confirm during report-only) |
| B makes an API commit in a fork or another repo and A fast-forwards to it | **not covered**: this repo's activity log never saw B's push; A took the commit in |
| B renames the branch to hide their push | blocked: the push record no longer reaches branch creation, so the check fails |
| Activity log hasn't caught up with B's latest push | blocked: record must reach the current head |
| B has a workflow push for them (`github-actions[bot]`) | blocked: bot pushes/commits fail |
| B deletes the trigger workflow on the branch | blocked: no check is posted, so the required check is missing |
| B posts a fake `approval-guard` check | ignored: the required check is pinned to the App |
| B approves, then pushes again | blocked: the approval is on the old commit |
| Fork PRs | fail with "push the branch to this repo" |
| Dependabot PR (only Dependabot commits) | one human approval is enough |
| "Update branch" button | whoever clicked it counts as having touched the PR |
| A re-commits (cherry-pick / reset) B's change under their own name | not covered: A took ownership of the change |
| Author clicks "Commit suggestion" on B's suggestion | not covered: author accepted it |
| Approval withdrawn while a run is mid-evaluation | blocked: reviews are re-read before posting; if they changed, the run fails and the queued run decides |
| A PR changes approval-guard's own files | needs **two** untouched approvals |
| A reviewer approves, then requests changes; a writer stops the guard's re-check and dismisses that change request | blocked only with step 4 (review dismissal restricted to admins): the change request then keeps GitHub's own required review failing |
| Approval withdrawn after a success, and a writer stops the re-check (cancels it before it marks the check *in progress*, disables the workflow, exhausts the App's rate limit, or an approved head weakened the trigger) | **not covered by the check alone**: the earlier success stays on that commit. It was earned by an untouched approval of this exact commit; a reviewer who objects should request changes, which GitHub's required review enforces when dismissal is restricted (step 4) |
| An admin edits the rules | **not covered** (admins can change rulesets) |
| Two colluding / hacked accounts | **not covered** |

## Open choice

The App key here is an **environment secret** (simplest). To move it to AWS KMS later, replace the `create-github-app-token` step with an OIDC → `kms:Sign` JWT step. `guard.py` doesn't change.

## Always fails, or never gets a check

- **PRs from forks:** fail. Push the branch to this repo instead.
- **PRs whose commits or pushes come from bots other than Dependabot**, e.g. github-actions, renovate, release-please, Copilot: fail. A human re-pushes the change.
- **PRs into other branches:** no check. Stacked PRs that GitHub retargets to the default branch get evaluated on the next push or review, or a manual re-run.
- **Very old branches** whose push record has expired: fail. Re-create the branch.
- **Merge queue:** not supported yet. Don't enforce on repos that use one.
- **PRs with more than 250 commits:** fail.
- **Web/API commits GitHub can't attribute** fail: "Update branch" using *rebase* on a multi-commit PR, a rebase-merge of a nested PR, or several commits pushed in one Git Data API call. Use "Update branch" with a merge commit, or push from git.
- **Web/API commits dated in the future**, or a repo too busy to scan back to the commit's date (more than 3000 activity entries), fail.
- **Web/API commits whose creating push has aged out of GitHub's activity history** fail (the guard can't prove who made them). Re-push the change from git.
- **Bot activity on a commit**: if any bot other than Dependabot ever pushed a branch to a web/API commit in the PR, the check fails.
- **Activity log lag:** the guard retries for about 20 s; if it still lags, the check fails and you re-run it (below).

## Verify during report-only

- Order: after a *success*, a newer *in progress* `approval-guard` check on the same commit must block the merge (GitHub should use the latest check run of that name).
- Whether GitHub's native required review blocks the merge when a writer requests changes after approving.
- Dependabot-triggered runs get the environment secret.
- Activity entries survive branch deletion; a branch rename fails closed; "Update branch" appears as a push entry.

## Re-running

Actions → approval-guard → Run workflow (default branch) → PR number. This re-reads live state and posts a fresh check. Use it if a check is missing or stale.
