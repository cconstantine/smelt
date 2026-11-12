"""scripts/pm/smelt-wait's tests (SME-460): one case per wake, and each false
match it was fixed for, against scripts/pm/fake_smelt.py and a fake
smelt-mcp."""

import datetime
import json
import subprocess
import sys
import time

from helpers import WAITER, FakeCase, fake_smelt

SINCE = "2026-11-11T12:00:00Z"
# Fractional minutes: 0.03 min is 1.8 s at a 0.2 s poll.
CAP = "0.03"


def stamp(minutes_ago):
    t = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(minutes=minutes_ago)
    return t.replace(tzinfo=None).isoformat(timespec="seconds")


def pr(number, title, ref, merged_at):
    return {"number": number, "title": title, "merged": merged_at is not None, "merged_at": merged_at,
            "head": {"ref": ref}}


def run_(id, name, conclusion, status="completed"):
    return {"id": id, "name": name, "status": status, "conclusion": conclusion, "head_sha": "abc123",
            "created_at": SINCE, "updated_at": SINCE}


class Wait(FakeCase):
    def setUp(self):
        super().setUp()
        self.add_conv(1, persona="smelt:ticket-driver")
        self.add_conv(2, 1, persona="smelt:developer")
        self.add_conv(3, 2)
        self.add_conv(9)
        self.mcp_bin = None

    def mcp(self, prs=None, runs=None):
        script = {"Github/list_pull_requests": [json.dumps(p) for p in (prs or [[]])],
                  "Github/actions_list": [json.dumps({"total_count": len(r), "workflow_runs": r}) if r is not None
                                          else None for r in (runs or [[]])]}
        if prs and prs[0] is None:
            script["Github/list_pull_requests"] = [None]
        bindir, self.mcp_data = self.fake_mcp(script)
        self.mcp_bin = bindir + "/smelt-mcp"

    def start(self, *args, **env):
        if self.mcp_bin is None and "--no-github" not in args:
            self.mcp()
        env.setdefault("SMELT_WAIT_POLL_SECS", "0.2")
        env.setdefault("SMELT_WAIT_GH_EVERY", "1")
        env.setdefault("SMELT_MCP_BIN", self.mcp_bin)
        self.proc = subprocess.Popen([sys.executable, "-I", "-B", WAITER, *args], env=self.env(**env),
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(self.stop_proc)
        return self.proc

    def stop_proc(self):
        if self.proc.poll() is None:
            self.proc.kill()
        self.proc.wait(5)
        self.proc.stdout.close()
        self.proc.stderr.close()

    def baseline(self):
        """Wait for the watcher's baseline line on stderr."""
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            line = self.proc.stderr.readline()
            if not line:
                self.fail("smelt-wait ended before its baseline")
            if "baseline taken" in line:
                return
        self.fail("no baseline in 15 s")

    def woke(self, timeout=20):
        """The wake line without its time (`KIND TIME rest`), which is kept
        in self.at after checking `--since` takes it."""
        try:
            out, err = self.proc.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.fail("smelt-wait didn't wake in time")
        lines = out.splitlines()
        self.assertEqual(self.proc.returncode, 0, out + err)
        self.assertEqual(len(lines), 1, f"exactly one wake line: {out!r} {err!r}")
        kind, _, rest = lines[0].partition(" ")
        at, _, rest = rest.partition(" ")
        self.assertRegex(at, r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$", f"no wake time: {lines[0]!r}")
        self.at = at
        return f"{kind} {rest}" if rest else kind

    def node(self, cid):
        return next(n for n in self.st.nodes if n["id"] == cid)

    def say(self, cid, role, blocks):
        with self.st.lock:
            msgs = self.st.messages.setdefault(cid, [])
            msgs.append(fake_smelt.message(self.st.next_message_id, cid, role, blocks))
            self.st.next_message_id += 1

    # Waiting for the user.

    def test_a_node_already_waiting_wakes_at_once(self):
        self.node(3)["waiting_for_user"] = True
        self.start("1", "45", CAP)
        self.assertEqual(self.woke(), "ALREADY_WAITING 3")

    def test_a_known_waiting_node_is_quiet_and_a_new_one_wakes(self):
        self.node(3)["waiting_for_user"] = True
        self.start("1", "45", "1", "--known-waiting", "3")
        self.baseline()
        time.sleep(0.6)
        self.node(2)["waiting_for_user"] = True
        self.assertEqual(self.woke(), "WAITING_FOR_USER 2")

    def test_a_known_waiting_node_that_asks_again_wakes(self):
        # M2: --known-waiting covers the question waiting at start, not the
        # next one: once a poll shows the node not waiting, it's dropped.
        self.node(3)["waiting_for_user"] = True
        self.start("1", "45", "1", "--known-waiting", "3")
        self.baseline()
        time.sleep(0.6)
        self.node(3)["waiting_for_user"] = False
        time.sleep(0.6)
        self.node(3)["waiting_for_user"] = True
        self.assertEqual(self.woke(), "WAITING_FOR_USER 3")

    def test_a_known_waiting_node_not_waiting_at_start_wakes_when_it_asks(self):
        self.start("1", "45", "1", "--known-waiting", "3")
        self.baseline()
        time.sleep(0.4)
        self.node(3)["waiting_for_user"] = True
        self.assertEqual(self.woke(), "WAITING_FOR_USER 3")

    def test_a_waiting_node_outside_the_tree_is_quiet(self):
        self.node(9)["waiting_for_user"] = True
        self.start("1", "45", CAP)
        self.assertEqual(self.woke(), "CAP")

    # ROOT's own messages.

    def test_a_new_notify_user_call_wakes(self):
        self.start("1", "45", "1")
        self.baseline()
        self.say(1, "assistant", [{"type": "text", "text": "done"},
                                  {"type": "tool_use", "id": "t1", "name": "notify_user", "input": {"message": "PR #5 open"}}])
        line = self.woke()
        self.assertTrue(line.startswith("TOOL notify_user"), line)
        self.assertIn("PR #5 open", line)

    def test_old_calls_and_quoted_tool_names_are_quiet(self):
        self.say(1, "assistant", [{"type": "tool_use", "id": "t0", "name": "ask_user", "input": {}}])
        self.start("1", "45", CAP)
        self.baseline()
        self.say(1, "user", [{"type": "tool_result", "tool_use_id": "t0", "content": "called notify_user"}])
        self.say(1, "user", [{"type": "text", "text": "please notify_user me"}])
        self.say(2, "assistant", [{"type": "tool_use", "id": "t2", "name": "notify_user", "input": {}}])
        self.assertEqual(self.woke(), "CAP")

    def test_the_key_matches_only_assistant_text_after_the_baseline(self):
        self.say(1, "assistant", [{"type": "text", "text": "PR opened #1 (old)"}])
        self.start("1", "45", "1", SMELT_KEY=r"PR opened #\d+")
        self.baseline()
        self.say(1, "user", [{"type": "text", "text": "PR opened #2"}])
        self.say(1, "assistant", [{"type": "tool_use", "id": "t", "name": "message_agent",
                                   "input": {"text": "PR opened #3"}}])
        self.say(1, "user", [{"type": "tool_result", "tool_use_id": "t", "content": "PR opened #4"}])
        time.sleep(0.8)
        self.assertIsNone(self.proc.poll(), "woke on a false match")
        self.say(1, "assistant", [{"type": "text", "text": "Driver: PR opened #5, head abc"}])
        line = self.woke()
        self.assertTrue(line.startswith("KEY "), line)
        self.assertIn("PR opened #5", line)

    # A seat owing a report (SME-455).

    def test_a_report_owed_past_the_grace_at_start_wakes_at_once(self):
        # M3: the baseline doesn't swallow a stamp already past the grace.
        owed = stamp(30)
        self.node(2)["report_owed_at"] = owed
        self.start("1", "45", "1")
        self.assertEqual(self.woke(), f"REPORT_OWED 2 {owed}")

    def test_a_report_owed_inside_the_grace_at_start_wakes_once_past_it(self):
        owed = stamp(0)
        self.node(3)["report_owed_at"] = owed
        self.start("1", "45", "1", SMELT_OWED_GRACE_MIN="0.03")
        self.assertEqual(self.woke(), f"REPORT_OWED 3 {owed}")

    def test_a_known_owed_stamp_is_quiet_and_a_new_one_from_it_wakes(self):
        self.node(2)["report_owed_at"] = stamp(30)
        self.start("1", "45", "1", "--known-owed", "2")
        self.baseline()
        time.sleep(0.6)
        self.assertIsNone(self.proc.poll(), "woke on a known owed stamp")
        owed = stamp(20)
        self.node(2)["report_owed_at"] = owed
        self.assertEqual(self.woke(), f"REPORT_OWED 2 {owed}")

    def test_a_new_old_enough_owed_report_wakes(self):
        self.node(2)["report_owed_at"] = stamp(30)
        self.start("1", "45", "1", "--known-owed", "2")
        self.baseline()
        self.node(3)["report_owed_at"] = stamp(1)
        time.sleep(0.8)
        self.assertIsNone(self.proc.poll(), "woke inside the grace period, or on the baseline's stamp")
        owed = stamp(11)
        self.node(3)["report_owed_at"] = owed
        self.assertEqual(self.woke(), f"REPORT_OWED 3 {owed}")

    # GitHub, through smelt-mcp.

    def test_this_ticket_s_merge_after_since_wakes_and_others_dont(self):
        quiet = [
            pr(1, "SME-4600: another", "cconstan/sme-4600-x", "2026-11-11T13:00:00Z"),
            pr(2, "SME-46: shorter", "cconstan/sme-46-y", "2026-11-11T13:00:00Z"),
            pr(3, "SME-460: before since", "cconstan/sme-460-z", "2026-11-11T11:00:00Z"),
            pr(4, "SME-460: closed unmerged", "cconstan/sme-460-z", None),
        ]
        self.mcp(prs=[quiet, quiet, quiet, quiet + [pr(5, "SME-460 test: by title", "fix-things", "2026-11-11T13:05:00Z")]])
        self.start("1", "45", "1", "--since", SINCE, SME="460")
        line = self.woke()
        self.assertTrue(line.startswith("MERGED pr#5"), line)

    def test_a_merge_found_by_its_branch_wakes(self):
        self.mcp(prs=[[pr(7, "Run the PM", "cconstan/SME-460-run-the-pm", "2026-11-11T13:00:00Z")]])
        self.start("1", "45", "1", "--since", SINCE, SME="460")
        self.assertTrue(self.woke().startswith("MERGED pr#7"))

    def test_a_run_going_red_after_the_baseline_wakes_and_an_old_red_doesnt(self):
        old = run_(1, "CI", "failure")
        going = run_(2, "CI", None, status="in_progress")
        other = run_(3, "Docs", "failure")
        red = run_(2, "CI", "timed_out")
        self.mcp(runs=[[old, going], [old, going, other], [old, going, other], [old, red, other]])
        self.start("1", "45", "1")
        line = self.woke()
        self.assertTrue(line.startswith("MAIN_RED run#2"), line)

    def test_an_old_red_alone_is_quiet(self):
        self.mcp(runs=[[run_(1, "Publish images", "startup_failure")]])
        self.start("1", "45", CAP)
        self.assertEqual(self.woke(), "CAP")

    # The wake line's time, for the next run's --since (N1).

    def test_a_wake_s_time_is_now_without_github(self):
        before = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0)
        self.start("1", "45", CAP, "--no-github")
        self.assertEqual(self.woke(), "CAP")
        at = datetime.datetime.fromisoformat(self.at.replace("Z", "+00:00"))
        self.assertTrue(before <= at <= datetime.datetime.now(datetime.timezone.utc), self.at)

    def test_a_merge_s_time_is_its_merged_at_and_restarting_there_is_quiet(self):
        merged = pr(7, "SME-460: x", "cconstan/sme-460-x", "2026-11-11T13:00:00Z")
        self.mcp(prs=[[merged]])
        self.start("1", "45", "1", "--since", SINCE, SME="460")
        self.assertTrue(self.woke().startswith("MERGED pr#7"))
        self.assertEqual(self.at, "2026-11-11T13:00:00Z")
        self.start("1", "45", CAP, "--since", self.at, SME="460")
        self.assertEqual(self.woke(), "CAP")

    def test_a_wake_s_time_stays_at_since_until_github_answers(self):
        self.mcp(prs=[None])
        self.start("1", "45", CAP, "--since", SINCE, SME="460", SMELT_WAIT_MAX_FAILURES="1000")
        self.assertEqual(self.woke(), "CAP")
        self.assertEqual(self.at, SINCE, "a merge GitHub never showed would be skipped by the next run")

    def test_a_wake_s_time_after_a_github_check_is_just_before_it(self):
        self.start("1", "45", CAP, "--since", SINCE, SME="460")
        self.assertEqual(self.woke(), "CAP")
        at = datetime.datetime.fromisoformat(self.at.replace("Z", "+00:00"))
        self.assertGreater(at, datetime.datetime.fromisoformat(SINCE.replace("Z", "+00:00")))

    def test_no_smelt_mcp_exits_2_unless_no_github(self):
        r = self.run_script(WAITER, "1", "45", CAP, env=self.env(SMELT_MCP_BIN="/nonexistent/smelt-mcp",
                                                               SMELT_WAIT_POLL_SECS="0.2"))
        self.assertEqual(r.returncode, 2, r.stdout + r.stderr)
        self.assertIn("smelt-mcp", r.stderr)
        self.assertEqual(r.stdout, "")
        r = self.run_script(WAITER, "1", "45", CAP, "--no-github",
                            env=self.env(SMELT_MCP_BIN="/nonexistent/smelt-mcp", SMELT_WAIT_POLL_SECS="0.2"))
        self.assertEqual((r.returncode, r.stdout.split()[0]), (0, "CAP"), r.stderr)

    # Time and failures.

    def test_nothing_moving_wakes_with_stall_naming_the_busy(self):
        self.node(2)["busy"] = True
        self.start("1", "0.02", "1", "--no-github")
        line = self.woke()
        self.assertTrue(line.startswith("STALL 0.02"), line)
        self.assertIn("busy:[2]", line)

    def test_an_updated_node_holds_off_the_stall(self):
        self.start("1", "0.02", CAP, "--no-github")
        self.baseline()
        for i in range(8):
            time.sleep(0.2)
            self.node(3)["updated_at"] = f"2026-11-11T12:00:{i + 1:02d}"
        self.assertEqual(self.woke(), "CAP")

    def test_failed_polls_in_a_row_wake_unreachable(self):
        self.start("1", "45", "1", "--no-github", SMELT_WAIT_MAX_FAILURES="3")
        self.baseline()
        self.st.fail_next = 1000
        self.assertEqual(self.woke(), "UNREACHABLE 3")

    def test_failed_smelt_mcp_calls_count_toward_unreachable(self):
        self.mcp(prs=[None])
        self.start("1", "45", "1", SMELT_WAIT_MAX_FAILURES="3", SME="460")
        self.assertEqual(self.woke(), "UNREACHABLE github 3")

    def test_smelt_mcp_failing_at_the_default_gh_every_still_wakes_unreachable(self):
        # M1: at SMELT_WAIT_GH_EVERY=2 a tree-only poll falls between each
        # GitHub call; its success mustn't reset GitHub's count.
        self.mcp(prs=[None])
        self.start("1", "45", "1", SMELT_WAIT_MAX_FAILURES="3", SMELT_WAIT_GH_EVERY="2", SME="460")
        self.assertEqual(self.woke(), "UNREACHABLE github 3")

    def test_smelt_mcp_failing_at_the_baseline_counts_as_github(self):
        self.mcp(runs=[None])
        self.start("1", "45", "1", SMELT_WAIT_MAX_FAILURES="3", SMELT_WAIT_GH_EVERY="2")
        self.assertEqual(self.woke(), "UNREACHABLE github 3")

    def test_a_root_not_in_the_tree_exits_2(self):
        r = self.run_script(WAITER, "55", "45", CAP, "--no-github", env=self.env(SMELT_WAIT_POLL_SECS="0.2"))
        self.assertEqual(r.returncode, 2, r.stdout + r.stderr)
        self.assertEqual(r.stdout, "")

    def test_usage_errors_exit_2(self):
        for args in (["x1"], ["1", "soon"], []):
            with self.subTest(args=args):
                r = self.run_script(WAITER, *args, "--no-github")
                self.assertEqual(r.returncode, 2, r.stdout + r.stderr)
