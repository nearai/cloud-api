import importlib.util
import json
import pathlib
import unittest
from unittest import mock

_spec = importlib.util.spec_from_file_location(
    "guard", pathlib.Path(__file__).with_name("guard.py"))
guard = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(guard)

HEAD = "h" * 40
OLD = "o" * 40
Z = guard.ZERO_SHA


def user(login, kind="User"):
    return {"login": login, "type": kind}


def pr(author="alice", kind="User"):
    return {"user": user(author, kind), "head": {"sha": HEAD}}


def commit(author, committer=None, verified=True, sha="c" * 40, akind="User", ckind="User"):
    committer = committer or author
    return {
        "sha": sha,
        "author": user(author, akind) if author else None,
        "committer": user(committer, ckind) if committer else None,
        "commit": {"verification": {"verified": verified, "verified_at": "2026-11-11T10:00:00Z"},
                   "committer": {"date": "2026-11-11T10:00:00Z"}},
    }


def push(actor, kind="User", before="b" * 40, after=HEAD, typ="push", ts="2026-11-11T12:00:00Z"):
    return {"actor": user(actor, kind) if actor else None, "before": before, "after": after,
            "activity_type": typ, "timestamp": ts}


_rid = iter(range(1, 10_000))


def review(login, state="APPROVED", sha=HEAD, kind="User"):
    return {"id": next(_rid), "user": user(login, kind), "state": state, "commit_id": sha}


def writers(*logins):
    return lambda l: "write" if l in logins else "read"


ALL_WRITE = lambda l: "write"


class Decide(unittest.TestCase):
    def run_decide(self, p, commits, reviews, pushes, perms=ALL_WRITE):
        return guard.decide(p, commits, reviews, pushes, perms)[0]

    # --- the core rule ------------------------------------------------------

    def test_untouched_approver_passes(self):
        self.assertEqual("success", self.run_decide(
            pr(), [commit("alice")], [review("carol")], [push("alice")]))

    def test_takeover_attack_committer_approval_ignored(self):
        # alice opens, bob pushes a commit, alice pushes after, bob approves
        self.assertEqual("failure", self.run_decide(
            pr("alice"), [commit("alice"), commit("bob"), commit("alice")],
            [review("bob")], [push("alice"), push("bob"), push("alice")]))

    def test_pusher_of_web_commit_with_forged_author_is_touched(self):
        self.assertEqual("failure", self.run_decide(
            pr("alice"), [commit("alice", committer="web-flow")],
            [review("bob")], [push("bob")]))

    def test_pr_author_approval_never_counts(self):
        self.assertEqual("failure", self.run_decide(
            pr("alice"), [commit("bob")], [review("alice")], [push("bob")]))

    def test_realistic_dependabot_web_flow_commit_passes(self):
        # Dependabot commits are signed by GitHub (committer web-flow); the
        # branch_creation by dependabot[bot] attributes them.
        p = pr("dependabot[bot]", "Bot")
        c = commit("dependabot[bot]", committer="web-flow", sha=HEAD, akind="Bot")
        chain = [push("dependabot[bot]", "Bot", before=Z, after=HEAD, typ="branch_creation")]
        extra, problems = guard.web_flow_creators([c], chain, [])
        self.assertEqual([], problems)
        self.assertEqual("success", self.run_decide(p, [c], [review("carol")], chain + extra))

    def test_dependabot_takeover_example_passes(self):
        # dependabot PR, pierre replaced the commit, someone else approves
        p = pr("dependabot[bot]", "Bot")
        self.assertEqual("success", self.run_decide(
            p, [commit("pierre")], [review("think"), review("pierre")],
            [push("dependabot[bot]", "Bot"), push("pierre")]))

    def test_pure_dependabot_pr_one_human_approval(self):
        p = pr("dependabot[bot]", "Bot")
        self.assertEqual("success", self.run_decide(
            p, [commit("dependabot[bot]", akind="Bot", ckind="Bot")],
            [review("carol")], [push("dependabot[bot]", "Bot")]))

    # --- review semantics ---------------------------------------------------

    def test_stale_approval_on_old_commit_ignored(self):
        self.assertEqual("failure", self.run_decide(
            pr(), [commit("alice")], [review("carol", sha=OLD)], [push("alice")]))

    def test_dismissed_approval_no_longer_counts(self):
        # GitHub rewrites a dismissed review's state to DISMISSED in place.
        r = review("carol")
        r["state"] = "DISMISSED"
        self.assertEqual("failure", self.run_decide(pr(), [commit("alice")], [r], [push("alice")]))

    def test_comment_after_approval_keeps_approval(self):
        self.assertEqual("success", self.run_decide(
            pr(), [commit("alice")],
            [review("carol"), review("carol", "COMMENTED")], [push("alice")]))

    def test_writer_changes_requested_blocks(self):
        self.assertEqual("failure", self.run_decide(
            pr(), [commit("alice")],
            [review("carol"), review("dave", "CHANGES_REQUESTED")], [push("alice")]))

    def test_non_writer_or_bot_changes_requested_does_not_block(self):
        self.assertEqual("success", self.run_decide(
            pr(), [commit("alice")],
            [review("carol"), review("eve", "CHANGES_REQUESTED"),
             review("lint[bot]", "CHANGES_REQUESTED", kind="Bot")],
            [push("alice")], writers("carol")))

    def test_changes_requested_with_failed_lookup_blocks(self):
        def perms(l):
            if l == "dave":
                raise guard.GuardError("boom")
            return "write"
        self.assertEqual("failure", self.run_decide(
            pr(), [commit("alice")],
            [review("carol"), review("dave", "CHANGES_REQUESTED")], [push("alice")], perms))

    def test_dismissed_changes_requested_no_longer_blocks(self):
        r = review("dave", "CHANGES_REQUESTED")
        r["state"] = "DISMISSED"
        self.assertEqual("success", self.run_decide(
            pr(), [commit("alice")], [r, review("carol")], [push("alice")]))

    def test_bot_approval_ignored(self):
        self.assertEqual("failure", self.run_decide(
            pr(), [commit("alice")], [review("ironloopai[bot]", kind="Bot")], [push("alice")]))

    def test_approver_without_write_ignored(self):
        self.assertEqual("failure", self.run_decide(
            pr(), [commit("alice")], [review("carol")], [push("alice")], writers("dave")))

    def test_permission_lookup_failure_ignores_only_that_approval(self):
        def perms(l):
            if l == "carol":
                raise guard.GuardError("boom")
            return "write"
        self.assertEqual("success", self.run_decide(
            pr(), [commit("alice")], [review("carol"), review("dave")], [push("alice")], perms))

    def test_login_case_insensitive(self):
        self.assertEqual("failure", self.run_decide(
            pr("alice"), [commit("Bob")], [review("bob")], [push("BOB")]))

    # --- identities ---------------------------------------------------------

    def test_unverified_commit_fails(self):
        self.assertEqual("failure", self.run_decide(
            pr(), [commit("alice", verified=False)], [review("carol")], [push("alice")]))

    def test_unlinked_committer_fails(self):
        c = commit("alice")
        c["committer"] = None
        self.assertEqual("failure", self.run_decide(pr(), [c], [review("carol")], [push("alice")]))

    def test_unlinked_or_bot_author_does_not_block(self):
        c1 = commit("alice")
        c1["author"] = None
        c2 = commit("renovate[bot]", committer="alice", akind="Bot")
        self.assertEqual("success", self.run_decide(
            pr(), [c1, c2], [review("carol")], [push("alice")]))

    def test_actions_bot_push_fails(self):
        self.assertEqual("failure", self.run_decide(
            pr(), [commit("alice")], [review("carol")],
            [push("alice"), push("github-actions[bot]", "Bot")]))

    def test_bot_committer_fails(self):
        self.assertEqual("failure", self.run_decide(
            pr(), [commit("alice", committer="some-app[bot]", ckind="Bot")],
            [review("carol")], [push("alice")]))

    def test_push_without_actor_fails(self):
        self.assertEqual("failure", self.run_decide(
            pr(), [commit("alice")], [review("carol")], [push(None)]))


class Chain(unittest.TestCase):
    A, B, C = "a" * 40, "b" * 40, "c" * 40

    def entries(self):
        return [
            push("bob", before=self.B, after=HEAD, typ="force_push"),
            push("alice", before=self.A, after=self.B),
            push("alice", before=Z, after=self.A, typ="branch_creation"),
        ]

    def test_good_chain(self):
        self.assertEqual(3, len(guard.push_chain(self.entries(), HEAD)))

    def test_older_branch_with_same_name_dropped(self):
        e = self.entries() + [push("mallory", before=self.C, after=Z, typ="branch_deletion"),
                              push("mallory", before=Z, after=self.C, typ="branch_creation")]
        chain = guard.push_chain(e, HEAD)
        self.assertNotIn("mallory", [x["actor"]["login"] for x in chain])

    def test_stale_newest_entry_fails(self):
        # activity log lags: newest entry is not the current head
        with self.assertRaises(guard.GuardError):
            guard.push_chain(self.entries()[1:], HEAD)

    def test_gap_fails(self):
        e = self.entries()
        del e[1]
        with self.assertRaises(guard.GuardError):
            guard.push_chain(e, HEAD)

    def test_missing_branch_creation_fails(self):
        # e.g. renamed branch: entries under the old name are not returned
        with self.assertRaises(guard.GuardError):
            guard.push_chain(self.entries()[:2], HEAD)

    def test_exact_duplicate_entry_tolerated(self):
        e = self.entries()
        for i, x in enumerate(e):
            x["id"] = 100 - i
        e.insert(1, dict(e[0]))
        self.assertEqual(3, len(guard.push_chain(e, HEAD)))

    def test_stale_is_retryable_error(self):
        with self.assertRaises(guard.StaleRecord):
            guard.push_chain(self.entries()[1:], HEAD)

    def test_duplicate_entry_without_id_fails(self):
        e = self.entries()
        e.insert(1, dict(e[0]))
        with self.assertRaises(guard.GuardError):
            guard.push_chain(e, HEAD)

    def test_empty_fails(self):
        with self.assertRaises(guard.GuardError):
            guard.push_chain([], HEAD)


class WebFlow(unittest.TestCase):
    W = "w" * 40

    def test_web_commit_on_this_branch_credited_to_its_pusher(self):
        c = commit("alice", committer="web-flow", sha=HEAD)
        extra, problems = guard.web_flow_creators([c], [push("alice")], [])
        self.assertEqual([], problems)
        self.assertEqual(["alice"], [e["actor"]["login"] for e in extra])

    def test_fast_forward_to_api_commit_still_credits_creator(self):
        # bob creates API commit W (author alice) on fix-x; alice fast-forwards
        # her PR branch to W and pushes: W is now the chain tip, pushed by alice.
        c = commit("alice", committer="web-flow", sha=self.W)
        chain = [push("alice", after=self.W)]
        repo = [push("alice", after=self.W), push("bob", after=self.W, typ="branch_creation")]
        extra, problems = guard.web_flow_creators([c], chain, repo)
        self.assertEqual([], problems)
        self.assertIn("bob", [e["actor"]["login"] for e in extra])
        verdict = guard.decide(pr("alice"), [c], [review("bob")], chain + extra, ALL_WRITE)
        self.assertEqual("failure", verdict[0])

    def test_newer_fast_forward_elsewhere_does_not_hide_creator(self):
        c = commit("alice", committer="web-flow", sha=self.W)
        repo = [push("alice", after=self.W), push("bob", after=self.W)]  # newest first
        extra, _ = guard.web_flow_creators([c], [push("alice")], repo)
        self.assertIn("bob", [e["actor"]["login"] for e in extra])

    def test_web_commit_from_other_branch_attributed_to_its_creator(self):
        # bob made an API commit with author=alice on his branch; alice merged it
        c = commit("alice", committer="web-flow", sha=self.W)
        other = push("bob", after=self.W)
        extra, problems = guard.web_flow_creators([c], [push("alice")], [other])
        self.assertEqual([], problems)
        self.assertEqual("bob", extra[0]["actor"]["login"])
        verdict = guard.decide(pr("alice"), [c], [review("bob")], [push("alice")] + extra, ALL_WRITE)
        self.assertEqual("failure", verdict[0])

    def test_unattributable_web_commit_fails(self):
        c = commit("alice", committer="web-flow", sha=self.W)
        _, problems = guard.web_flow_creators([c], [push("alice")], [])
        self.assertTrue(problems)


class Plumbing(unittest.TestCase):
    def setUp(self):
        patcher = mock.patch.object(guard, "touches_guard", return_value=False)
        self.touches = patcher.start()
        self.addCleanup(patcher.stop)

    def test_get_pages_follows_link_and_stops(self):
        pages = [([1, 2], '<https://x/next>; rel="next"'), ([3], '<https://x/n2>; rel="next"'), ([4], "")]
        with mock.patch.object(guard, "_request", side_effect=pages) as req:
            self.assertEqual([1, 2, 3], guard.get_pages("/p", stop=lambda i: i == 3))
            self.assertEqual(2, req.call_count)

    def test_get_pages_cap_fails_closed(self):
        pages = [([1], '<https://x/n>; rel="next"')] * 5
        with mock.patch.object(guard, "_request", side_effect=pages):
            with self.assertRaises(guard.GuardError):
                guard.get_pages("/p", max_pages=2)

    def _gather_pr(self, head=HEAD):
        return {"head": {"sha": head, "ref": "feat", "repo": {"full_name": "o/r"}}, "commits": 1}

    def test_gather_retries_stale_record(self):
        good = [push("alice", before=Z, after=HEAD, typ="branch_creation")]
        stale = [push("alice", before=Z, after=OLD, typ="branch_creation")]
        calls = [[commit("alice")], [], stale, good]
        with mock.patch.object(guard, "get_pages", side_effect=calls), \
             mock.patch.object(guard.time, "sleep") as sleep:
            _, _, pushes = guard.gather("o/r", 1, self._gather_pr())
        self.assertEqual(1, sleep.call_count)
        self.assertEqual("alice", pushes[0]["actor"]["login"])

    def test_gather_rejects_future_dated_web_commit(self):
        c = commit("alice", committer="web-flow", sha=HEAD)
        c["commit"]["committer"]["date"] = "2999-01-01T00:00:00Z"
        chain = [push("alice", before=Z, after=HEAD, typ="branch_creation")]
        with mock.patch.object(guard, "get_pages", side_effect=[[c], [], chain]):
            with self.assertRaises(guard.GuardError):
                guard.gather("o/r", 1, self._gather_pr())

    def test_permission_404_means_no_access(self):
        p = self._pr()
        def fake_get(path):
            if "/permission" in path:
                raise guard.GuardError("404", 404)
            return p
        with mock.patch.object(guard, "get", side_effect=fake_get), \
             mock.patch.object(guard, "gather", return_value=(
                 [commit("alice")], [review("carol"), review("outsider", "CHANGES_REQUESTED")],
                 [push("alice")])), \
             mock.patch.object(guard, "start_check", return_value=7), \
             mock.patch.object(guard, "finish_check") as post:
            guard.evaluate_pr("o/r", "main", 1)
        # carol is also a 404 here -> no write -> failure, but NOT "Changes requested"
        self.assertEqual("failure", post.call_args.args[2])
        self.assertNotEqual("Changes requested", post.call_args.args[3])

    def test_get_pages_stop_on_last_allowed_page_ok(self):
        pages = [([1], '<https://x/n>; rel="next"'), ([2], '<https://x/n2>; rel="next"')]
        with mock.patch.object(guard, "_request", side_effect=pages):
            self.assertEqual([1, 2], guard.get_pages("/p", stop=lambda i: i == 2, max_pages=2))

    def test_get_pages_require_stop_returns_when_stop_fires(self):
        pages = [([1], '<https://x/n>; rel="next"'), ([2], '<https://x/n2>; rel="next"')]
        with mock.patch.object(guard, "_request", side_effect=pages):
            self.assertEqual([1, 2], guard.get_pages(
                "/p", stop=lambda i: i == 2, max_pages=5, require_stop=True))

    def test_get_pages_require_stop_fails_when_history_ends(self):
        with mock.patch.object(guard, "_request", return_value=([1], "")):
            with self.assertRaises(guard.GuardError):
                guard.get_pages("/p", stop=lambda i: False, require_stop=True)

    def test_get_pages_ignores_prev_before_next(self):
        link = '<https://x/p>; rel="prev", <https://x/n>; rel="next"'
        with mock.patch.object(guard, "_request", side_effect=[([1], link), ([2], "")]) as req:
            self.assertEqual([1, 2], guard.get_pages("/p"))
        self.assertEqual("https://x/n", req.call_args.args[0])

    def test_gather_retries_exhausted(self):
        stale = [push("alice", before=Z, after=OLD, typ="branch_creation")]
        with mock.patch.object(guard, "get_pages", side_effect=[[commit("alice")], [], stale, stale, stale]), \
             mock.patch.object(guard.time, "sleep"):
            with self.assertRaises(guard.StaleRecord):
                guard.gather("o/r", 1, self._gather_pr())

    def test_gather_gap_not_retried(self):
        gap = [push("alice", before="x" * 40, after=HEAD)]
        with mock.patch.object(guard, "get_pages", side_effect=[[commit("alice")], [], gap]), \
             mock.patch.object(guard.time, "sleep") as sleep:
            with self.assertRaises(guard.GuardError):
                guard.gather("o/r", 1, self._gather_pr())
        sleep.assert_not_called()

    def test_gather_web_commit_requires_signing_time(self):
        c = commit("alice", committer="web-flow", sha=HEAD)
        del c["commit"]["verification"]["verified_at"]
        chain = [push("alice", before=Z, after=HEAD, typ="branch_creation")]
        with mock.patch.object(guard, "get_pages", side_effect=[[c], [], chain]):
            with self.assertRaises(guard.GuardError):
                guard.gather("o/r", 1, self._gather_pr())

    def test_gather_web_commit_window_anchored_on_signing_time(self):
        # committer date pushed into the future-but-passed; verified_at earlier
        c = commit("alice", committer="web-flow", sha=HEAD)
        c["commit"]["committer"]["date"] = "2026-11-11T10:00:00Z"
        c["commit"]["verification"]["verified_at"] = "2026-10-01T00:00:00Z"
        chain = [push("alice", before=Z, after=HEAD, typ="branch_creation")]
        seen = {}
        def fake_pages(path, stop=None, max_pages=None, require_stop=False):
            if "activity?direction" in path:
                seen["stop_old"] = stop({"timestamp": "2026-09-29T00:00:00Z"})
                seen["stop_new"] = stop({"timestamp": "2026-10-15T00:00:00Z"})
                seen["require_stop"] = require_stop
                return []
            return {"/pulls/1/commits": [c], "/pulls/1/reviews": []}.get(path[path.index("/pulls"):], chain) \
                if "/pulls" in path else chain
        with mock.patch.object(guard, "get_pages", side_effect=fake_pages):
            guard.gather("o/r", 1, self._gather_pr())
        self.assertEqual((True, False, True), (seen["stop_old"], seen["stop_new"], seen["require_stop"]))

    def test_gather_too_many_commits(self):
        p = self._gather_pr()
        p["commits"] = 251
        with self.assertRaises(guard.GuardError):
            guard.gather("o/r", 1, p)

    def test_gather_commit_count_mismatch(self):
        p = self._gather_pr()
        p["commits"] = 2
        with mock.patch.object(guard, "get_pages", return_value=[commit("alice")]):
            with self.assertRaises(guard.GuardError):
                guard.gather("o/r", 1, p)

    def test_candidate_prs_falls_back_to_commit_lookup(self):
        with mock.patch.object(guard, "get", return_value=[{"number": 5}]) as g:
            self.assertEqual({5}, guard.candidate_prs("o/r", HEAD, "", "[]"))
        self.assertIn(f"/commits/{HEAD}/pulls", g.call_args.args[0])

    def test_get_pages_rejects_non_list(self):
        with mock.patch.object(guard, "_request", return_value=({"message": "x"}, "")):
            with self.assertRaises(guard.GuardError):
                guard.get_pages("/p")

    def test_candidate_prs(self):
        self.assertEqual({7, 9}, guard.candidate_prs("o/r", HEAD, "9", json.dumps([{"number": 7}])))

    def _pr(self, head=HEAD, fork=False):
        return {"state": "open", "base": {"ref": "main"}, "head": {
            "sha": head, "ref": "feat", "repo": {"full_name": "x/y" if fork else "o/r"}},
            "commits": 1, "user": user("alice")}

    def test_evaluate_posts_failure_on_unexpected_error(self):
        with mock.patch.object(guard, "get", return_value=self._pr()), \
             mock.patch.object(guard, "gather", side_effect=KeyError("boom")), \
             mock.patch.object(guard, "start_check", return_value=7), \
             mock.patch.object(guard, "finish_check") as post:
            guard.evaluate_pr("o/r", "main", 1)
        self.assertEqual("failure", post.call_args.args[2])

    def test_evaluate_fork_fails(self):
        with mock.patch.object(guard, "get", return_value=self._pr(fork=True)), \
             mock.patch.object(guard, "start_check", return_value=7), \
             mock.patch.object(guard, "finish_check") as post:
            guard.evaluate_pr("o/r", "main", 1)
        self.assertEqual("failure", post.call_args.args[2])

    def test_evaluate_fails_superseded_when_head_moved(self):
        with mock.patch.object(guard, "get", side_effect=[self._pr(), self._pr(head=OLD)]), \
             mock.patch.object(guard, "gather", return_value=([], [], [])), \
             mock.patch.object(guard, "start_check", return_value=7), \
             mock.patch.object(guard, "finish_check") as post:
            guard.evaluate_pr("o/r", "main", 1)
        self.assertEqual(("o/r", 7, "failure", "Superseded by a newer commit"), post.call_args.args[:4])

    def test_in_progress_posted_before_evaluating(self):
        calls = []
        with mock.patch.object(guard, "get", return_value=self._pr()), \
             mock.patch.object(guard, "start_check", side_effect=lambda *a: calls.append("start") or 7), \
             mock.patch.object(guard, "gather", side_effect=lambda *a: calls.append("gather") or ([], [], [])), \
             mock.patch.object(guard, "finish_check", side_effect=lambda *a: calls.append("finish")):
            guard.evaluate_pr("o/r", "main", 1)
        self.assertEqual(["start", "gather", "finish"], calls)

    def test_start_check_failure_raises_before_evaluating(self):
        with mock.patch.object(guard, "get", return_value=self._pr()), \
             mock.patch.object(guard, "start_check", side_effect=guard.GuardError("rate limited")), \
             mock.patch.object(guard, "gather") as g, \
             mock.patch.object(guard, "finish_check") as post:
            with self.assertRaises(guard.GuardError):
                guard.evaluate_pr("o/r", "main", 1)
        g.assert_not_called()
        post.assert_not_called()

    def test_review_reread_failure_finishes_as_failure(self):
        p = self._pr()
        with mock.patch.object(guard, "get", side_effect=[p, {"permission": "write"}, p]), \
             mock.patch.object(guard, "gather", return_value=([commit("alice")], [review("carol")], [push("alice")])), \
             mock.patch.object(guard, "get_pages", side_effect=guard.GuardError("HTTP 502")), \
             mock.patch.object(guard, "start_check", return_value=7), \
             mock.patch.object(guard, "finish_check") as post:
            guard.evaluate_pr("o/r", "main", 1)
        self.assertEqual(("failure", "approval-guard could not confirm its verdict"), post.call_args.args[2:4])

    def test_guard_change_needs_two_untouched_approvals(self):
        p = self._pr()
        self.touches.return_value = True
        one = [review("carol")]
        with mock.patch.object(guard, "get", side_effect=[p, {"permission": "write"}, p]), \
             mock.patch.object(guard, "gather", return_value=([commit("alice")], one, [push("alice")])), \
             mock.patch.object(guard, "get_pages", return_value=one), \
             mock.patch.object(guard, "start_check", return_value=7), \
             mock.patch.object(guard, "finish_check") as post:
            guard.evaluate_pr("o/r", "main", 1)
        self.assertEqual("failure", post.call_args.args[2])
        two = [review("carol"), review("dave")]
        with mock.patch.object(guard, "get", side_effect=[p, {"permission": "write"}, {"permission": "write"}, p]), \
             mock.patch.object(guard, "gather", return_value=([commit("alice")], two, [push("alice")])), \
             mock.patch.object(guard, "get_pages", return_value=[dict(r) for r in two]), \
             mock.patch.object(guard, "start_check", return_value=7), \
             mock.patch.object(guard, "finish_check") as post:
            guard.evaluate_pr("o/r", "main", 1)
        self.assertEqual("success", post.call_args.args[2])

    def test_check_request_bodies(self):
        with mock.patch.object(guard, "_request", return_value=({"id": 9}, "")) as req:
            self.assertEqual(9, guard.start_check("o/r", HEAD))
            guard.finish_check("o/r", 9, "success", "t", "s")
        start_url, start_method, start_body = req.call_args_list[0].args
        self.assertEqual(("POST", "approval-guard", HEAD, "in_progress"),
                         (start_method, start_body["name"], start_body["head_sha"], start_body["status"]))
        fin_url, fin_method, fin_body = req.call_args_list[1].args
        self.assertTrue(fin_url.endswith("/check-runs/9"))
        self.assertEqual(("PATCH", "completed", "success"),
                         (fin_method, fin_body["status"], fin_body["conclusion"]))

    def test_start_check_rejects_bad_response(self):
        with mock.patch.object(guard, "_request", return_value=({"message": "x"}, "")):
            with self.assertRaises(guard.GuardError):
                guard.start_check("o/r", HEAD)


    def test_cancelled_run_leaves_check_pending(self):
        # Simulates cancellation mid-run: nothing after start_check completes.
        with mock.patch.object(guard, "get", return_value=self._pr()), \
             mock.patch.object(guard, "start_check", return_value=7) as start, \
             mock.patch.object(guard, "gather", side_effect=KeyboardInterrupt), \
             mock.patch.object(guard, "finish_check") as post:
            with self.assertRaises(KeyboardInterrupt):
                guard.evaluate_pr("o/r", "main", 1)
        start.assert_called_once()
        post.assert_not_called()

    def test_success_downgraded_when_reviews_change_during_evaluation(self):
        p = self._pr()
        snap = [review("carol")]
        dismissed = [dict(snap[0], state="DISMISSED")]
        with mock.patch.object(guard, "get", side_effect=[p, {"permission": "write"}, p]), \
             mock.patch.object(guard, "gather", return_value=([commit("alice")], snap, [push("alice")])), \
             mock.patch.object(guard, "get_pages", return_value=dismissed), \
             mock.patch.object(guard, "start_check", return_value=7), \
             mock.patch.object(guard, "finish_check") as post:
            guard.evaluate_pr("o/r", "main", 1)
        self.assertEqual(("failure", "Reviews changed during evaluation"), post.call_args.args[2:4])

    def test_evaluate_success_path(self):
        p = self._pr()
        reviews = [review("carol")]
        with mock.patch.object(guard, "get", side_effect=[p, {"permission": "write"}, p]), \
             mock.patch.object(guard, "gather",
                               return_value=([commit("alice")], reviews, [push("alice")])), \
             mock.patch.object(guard, "get_pages", return_value=[dict(r) for r in reviews]), \
             mock.patch.object(guard, "start_check", return_value=7) as start, \
             mock.patch.object(guard, "finish_check") as post:
            guard.evaluate_pr("o/r", "main", 1)
        start.assert_called_once_with("o/r", HEAD)
        self.assertEqual(("o/r", 7, "success"), post.call_args.args[:3])

    def test_evaluate_skips_pr_into_other_branch(self):
        p = self._pr()
        p["base"]["ref"] = "release"
        with mock.patch.object(guard, "get", return_value=p), \
             mock.patch.object(guard, "start_check") as start, \
             mock.patch.object(guard, "finish_check") as post:
            guard.evaluate_pr("o/r", "main", 1)
        start.assert_not_called()
        post.assert_not_called()


class TouchesGuard(unittest.TestCase):
    def files(self, *names, prev=None):
        out = [{"filename": n} for n in names]
        if prev:
            out[0]["previous_filename"] = prev
        return out

    def check(self, files, changed=None):
        with mock.patch.object(guard, "get_pages", return_value=files):
            return guard.touches_guard("o/r", 1, {"changed_files": len(files) if changed is None else changed})

    def test_detects_workflow_and_script_changes(self):
        self.assertTrue(self.check(self.files("src/main.rs", ".github/workflows/approval-guard-trigger.yml")))
        self.assertTrue(self.check(self.files(".github/approval-guard/guard.py")))

    def test_detects_rename_away(self):
        self.assertTrue(self.check(self.files("x.yml", prev=".github/workflows/approval-guard.yml")))

    def test_ordinary_change(self):
        self.assertFalse(self.check(self.files("src/main.rs", ".github/workflows/ci.yml")))

    def test_incomplete_list_fails(self):
        with self.assertRaises(guard.GuardError):
            self.check(self.files("src/main.rs"), changed=5)


if __name__ == "__main__":
    unittest.main()
