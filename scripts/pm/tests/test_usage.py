"""scripts/pm/smelt-api's `usage` (SME-486): the Claude subscription's
usage windows and the PM's hold/ok verdict, against
scripts/pm/fake_smelt.py."""

import json

from fake_smelt import usage, window
from helpers import FakeCase

USAGE_7 = ("GET", "/api/providers/7/usage")


class Usage(FakeCase):
    def verdict(self, *windows):
        """The verdict line `usage 7` prints for these windows."""
        self.st.usage[7] = usage(7, list(windows))
        r = self.api("usage", "7")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        lines = [line for line in r.stdout.splitlines() if line.startswith("verdict")]
        self.assertEqual(len(lines), 1, r.stdout)
        return lines[0].split(None, 1)[1]

    # P1
    def test_usage_prints_the_windows_and_the_verdict_and_sends_one_get(self):
        self.st.usage[7] = usage(7, [
            window("five_hour", 0.45),
            window("seven_day", 0.32, resets_at="2026-11-17T09:00:00Z", elapsed_fraction=0.5, projected=0.64),
        ])
        r = self.api("usage", "7")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.sent(), [USAGE_7])
        lines = r.stdout.splitlines()
        self.assertEqual(len(lines), 3, r.stdout)
        self.assertEqual(lines[0].split(), ["five_hour", "45%", "allowed", "resets", "2026-11-13T14:20Z",
                                            "seen", "2026-11-13T12:02Z"])
        self.assertEqual(lines[1].split(), ["seven_day", "32%", "allowed", "resets", "2026-11-17T09:00Z",
                                            "seen", "2026-11-13T12:02Z", "pace", "64%"])
        self.assertEqual(lines[2].split(), ["verdict", "ok"])

    def test_json_prints_the_same_fields_and_the_verdict(self):
        self.st.usage[7] = usage(7, [window("five_hour", 0.75)])
        r = self.api("usage", "7", "--json")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        out = json.loads(r.stdout)
        self.assertEqual(out["provider_id"], 7)
        self.assertEqual(out["windows"][0]["name"], "five_hour")
        self.assertEqual(out["windows"][0]["utilization"], 0.75)
        self.assertTrue(out["verdict"].startswith("hold: "), out)

    def test_a_server_refusal_exits_1(self):
        r = self.api("usage", "99")
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertIn("That provider no longer exists.", r.stderr)

    # Round 1 B-L1: before the deploy, smelt's catch-all NotFound page
    # answers the route 200 with HTML; that, or any answer without a
    # windows list, is no usage route, not "no reading".
    def test_an_answer_that_isnt_a_usage_answer_is_no_usage_route(self):
        for body in (b"<!DOCTYPE html><html><body>Not found</body></html>", b'"a string"', b'{"windows": "none"}', b"[]"):
            for args in (["usage", "7"], ["usage", "7", "--json"]):
                with self.subTest(body=body, args=args):
                    self.st.forced[USAGE_7] = (200, body)
                    r = self.api(*args)
                    self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
                    self.assertIn("unknown: no usage route", r.stdout)
                    self.assertNotIn("html", (r.stdout + r.stderr).lower())

    # P2
    def test_the_verdicts(self):
        cases = [
            ("five_hour at 70%", [window("five_hour", 0.70)], "hold: five_hour at 70%"),
            ("five_hour at 69%", [window("five_hour", 0.69)], "ok"),
            ("seven_day at 90%", [window("seven_day", 0.90)], "hold: seven_day at 90%"),
            ("seven_day at 89%", [window("seven_day", 0.89)], "ok"),
            ("seven_day pace 105%", [window("seven_day", 0.60, elapsed_fraction=0.57, projected=1.05)],
             "hold: seven_day on pace for 105% by reset"),
            ("seven_day pace 99%", [window("seven_day", 0.60, elapsed_fraction=0.6, projected=0.99)], "ok"),
            ("allowed_warning", [window("five_hour", 0.10, status="allowed_warning")],
             "hold: five_hour allowed_warning"),
            ("rejected", [window("seven_day_opus", 0.10, status="rejected")], "hold: seven_day_opus rejected"),
            ("a reset-passed window never holds",
             [window("five_hour", 0.10), window("seven_day", 0.95, reset_passed=True, status="rejected")], "ok"),
            ("no windows", [], "unknown: no reading"),
            ("every window reset-passed", [window("five_hour", 0.95, reset_passed=True)], "unknown: no reading"),
            ("two reasons", [window("five_hour", 0.80), window("seven_day", 0.95)],
             "hold: five_hour at 80%; seven_day at 95%"),
        ]
        for name, windows, expected in cases:
            with self.subTest(name):
                self.assertEqual(self.verdict(*windows), expected)

    # Round 1 B-L3: the verdict compares the whole percent it prints, so a
    # window printed at a threshold holds; a half rounds up, as Rust's
    # f64::round does on the provider page and the context bar.
    def test_the_verdict_compares_the_percent_it_prints(self):
        cases = [
            ("five_hour 69.51%", [window("five_hour", 0.6951)], "hold: five_hour at 70%"),
            ("five_hour 69.49%", [window("five_hour", 0.6949)], "ok"),
            ("seven_day 89.51%", [window("seven_day", 0.8951)], "hold: seven_day at 90%"),
            ("seven_day pace 99.51%", [window("seven_day", 0.60, elapsed_fraction=0.6, projected=0.9951)],
             "hold: seven_day on pace for 100% by reset"),
        ]
        for name, windows, expected in cases:
            with self.subTest(name):
                self.assertEqual(self.verdict(*windows), expected)
        self.st.usage[7] = usage(7, [window("five_hour", 0.125), window("seven_day", 0.625)])
        lines = self.api("usage", "7").stdout.splitlines()
        self.assertEqual(lines[0].split()[1], "13%", "a half rounds up")
        self.assertEqual(lines[1].split()[1], "63%", "a half rounds up")

    # Round 1 B-L4: only the seven-day pace counts (the verdict, the UI), so
    # only it prints.
    def test_only_the_seven_day_pace_prints(self):
        self.st.usage[7] = usage(7, [
            window("five_hour", 0.30, elapsed_fraction=0.15, projected=2.1),
            window("seven_day", 0.32, elapsed_fraction=0.5, projected=0.64),
        ])
        r = self.api("usage", "7")
        lines = r.stdout.splitlines()
        self.assertNotIn("pace", lines[0])
        self.assertIn("pace 64%", lines[1])
        self.assertEqual(lines[2].split(), ["verdict", "ok"])
        out = json.loads(self.api("usage", "7", "--json").stdout)
        self.assertEqual([w["projected"] for w in out["windows"]], [None, 0.64])

    def test_a_reset_passed_window_prints_reset_since(self):
        self.st.usage[7] = usage(7, [window("five_hour", 0.95, reset_passed=True)])
        r = self.api("usage", "7")
        self.assertIn("reset since", r.stdout.splitlines()[0])

    # P3
    def test_nothing_but_the_whitelisted_fields_prints(self):
        marker = "MARKER-sess-0123"
        answer = usage(7, [window("five_hour", 0.45), window(marker, 0.10)])
        answer["session_id"] = marker
        answer["windows"][0]["session_id"] = marker
        answer["windows"][0]["token"] = marker
        self.st.usage[7] = answer
        for args in (["usage", "7"], ["usage", "7", "--json"]):
            with self.subTest(args=args):
                r = self.api(*args)
                self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
                self.assertNotIn("MARKER", r.stdout + r.stderr)
        r = self.api("usage", "7")
        self.assertIn("(unnamed)", r.stdout, "a window name that isn't a plain word prints as (unnamed)")

    def test_a_non_digit_id_exits_2_and_sends_nothing(self):
        for args in (["usage", "7a"], ["usage", "../7"], ["usage", "-1"], ["usage"]):
            with self.subTest(args=args):
                self.fake.clear()
                r = self.api(*args)
                self.assertEqual(r.returncode, 2, r.stdout + r.stderr)
                self.assertEqual(self.sent(), [])
