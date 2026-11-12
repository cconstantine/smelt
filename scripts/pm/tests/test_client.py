"""scripts/pm/smelt-api's tests (SME-460): the allowlist, the address, exit
codes and each subcommand's pre-checks, against scripts/pm/fake_smelt.py."""

import ast
import json
import os
import socket

from helpers import CLIENT, PM, WAITER, FakeCase, base_env, fake_smelt

import smelt_api

QUESTION = {
    "tool_use_id": "tu1",
    "questions": [
        {"question": "Which?", "header": "Pick", "multi_select": False,
         "options": [{"label": "A"}, {"label": "B", "description": "the other"}]},
        {"question": "Which ones?", "header": "Many", "multi_select": True,
         "options": [{"label": "X"}, {"label": "Y"}, {"label": "Z"}]},
    ],
}


class Allowlist(FakeCase):
    def setUp(self):
        super().setUp()
        self.add_conv(7)
        self.st.questions[7] = json.loads(json.dumps(QUESTION))
        self.st.models[7] = {"state": "Chosen", "choice": fake_smelt.choice(1, "m")}
        self.brief = self.tmpfile("brief.md", "the brief\n")

    def test_each_subcommand_sends_exactly_its_rows_requests(self):
        c = "/api/conversations/7"
        table = [
            (["create"], [("POST", "/api/conversations")]),
            (["attach", "7", "https://github.com/o/r.git"], [("POST", c + "/repos")]),
            (["model-set", "7", "1", "m"], [("POST", c + "/model"), ("GET", c + "/model")]),
            (["send", "7", self.brief], [("POST", c + "/messages")]),
            (["send", "7", self.brief, "--expect-model", "1:m"], [("GET", c + "/model"), ("POST", c + "/messages")]),
            (["answer", "7", "tu1", "A", "X"], [("GET", c + "/question"), ("POST", c + "/question/answer")]),
            (["tree", "7"], [("GET", "/api/conversations/tree")]),
            (["messages", "7"], [("GET", c + "/messages")]),
            (["question", "7"], [("GET", c + "/question")]),
            (["model", "7"], [("GET", c + "/model")]),
            (["persona", "7"], [("GET", c + "/persona")]),
            (["persona-set", "7", "smelt:developer"], [("POST", c + "/persona"), ("GET", c + "/persona")]),
            (["pods"], [("GET", "/api/pods"), ("GET", "/api/pods/commands")]),
            (["providers"], [("GET", "/api/providers")]),
            (["where"], [("GET", "/api/build-id")]),
        ]
        for args, want in table:
            with self.subTest(args=args[0]):
                self.st.questions[7] = json.loads(json.dumps(QUESTION))
                self.fake.clear()
                r = self.api(*args)
                self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
                self.assertEqual(self.sent(), want)

    def test_refused_subcommands_exit_2_and_send_nothing(self):
        for args in (["delete", "7"], ["stop-pod", "1"], ["remove-storage", "1"], ["raw", "GET", "/api/secrets"],
                     ["providers-set", "1"], ["stop", "7"], ["unknown"], []):
            with self.subTest(args=args):
                r = self.api(*args)
                self.assertEqual(r.returncode, 2, r.stdout + r.stderr)
                self.assertEqual(self.sent(), [])

    def test_not_yet_subcommands_exit_3_naming_their_ticket(self):
        for args, ticket in ((["archive", "7"], "SME-480"), (["unarchive", "7"], "SME-480")):
            with self.subTest(args=args):
                r = self.api(*args)
                self.assertEqual(r.returncode, 3, r.stdout + r.stderr)
                self.assertIn(ticket, r.stdout + r.stderr)
                self.assertIn("not on this server yet", r.stdout + r.stderr)
                self.assertEqual(self.sent(), [])

    def test_compact_posts_its_note_and_says_what_the_server_refused(self):
        # SME-482's route, the PM's way to compact a Driver (pm-in-smelt.md).
        self.add_conv(7)
        r = self.api("compact", "7", "--note", "keep the PR number")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.sent(), [("POST", "/api/conversations/7/compact")])
        self.assertEqual(self.st.compacted, [(7, "keep the PR number")])
        self.fake.clear()
        r = self.api("compact", "7")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.st.compacted[-1], (7, None), "no --note sends a null note")
        self.st.compact_refusals[7] = "A turn is running in this conversation"
        r = self.api("compact", "7")
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertIn("A turn is running", r.stdout + r.stderr)

    def test_a_non_integer_id_exits_2_and_sends_nothing(self):
        for args in (["messages", "1/../2"], ["attach", "../7", "https://x/y"], ["model", "-1"], ["tree", "7a"]):
            with self.subTest(args=args):
                r = self.api(*args)
                self.assertEqual(r.returncode, 2, r.stdout + r.stderr)
                self.assertEqual(self.sent(), [])

    def test_the_sources_name_no_delete_and_no_path_outside_the_tables(self):
        # Every script that talks to smelt (L2: smelt-wait too); only
        # smelt_api.py may import urllib, so every request goes through call().
        for path in (CLIENT, WAITER, os.path.join(PM, "smelt_api.py")):
            with open(path, encoding="utf-8") as f:
                tree = ast.parse(f.read())
            if not path.endswith("smelt_api.py"):
                for n in ast.walk(tree):
                    names = ([a.name for a in n.names] if isinstance(n, ast.Import)
                             else [n.module or ""] if isinstance(n, ast.ImportFrom) else [])
                    for name in names:
                        with self.subTest(path=os.path.basename(path), imports=name):
                            self.assertFalse(name.split(".")[0] in ("urllib", "http", "requests", "socket"),
                                             "a request outside smelt_api.call")
            tables = set()
            for top in tree.body:
                if isinstance(top, ast.Assign) and any(getattr(t, "id", "") in ("ROUTES", "NOT_YET") for t in top.targets):
                    tables.update(id(n) for n in ast.walk(top.value))
            for n in ast.walk(tree):
                if isinstance(n, ast.Constant) and isinstance(n.value, str):
                    with self.subTest(path=os.path.basename(path), text=n.value[:40]):
                        self.assertNotEqual(n.value.strip().upper(), "DELETE")
                        if "/api/" in n.value:
                            self.assertIn(id(n), tables, "an /api/ path outside the route table")
        self.assertTrue(all(m in ("GET", "POST") for m, _ in smelt_api.ROUTES.values()))


class Address(FakeCase):
    def test_the_default_is_the_in_cluster_url_with_the_public_host(self):
        self.assertEqual(smelt_api.address({}),
                         ("http://smelt.smelt-park.svc.cluster.local:8080", "smelt.constantlee.us"))
        self.assertEqual(smelt_api.address({"SMELT_API_HOST": ""})[1], None, "empty: the URL's own host")
        self.assertEqual(smelt_api.address({"SMELT_API_URL": "https://smelt.example/"}), ("https://smelt.example", None))
        self.assertEqual(smelt_api.address({"SMELT_API_URL": "http://x:1", "SMELT_API_HOST": "h.example"}),
                         ("http://x:1", "h.example"))

    def test_a_url_with_more_than_scheme_host_and_port_exits_2_unprinted(self):
        # L1: a path, query or fragment would swallow the route table's
        # path, and userinfo would be printed (or crash urllib).
        base = self.fake.url
        for url in (base + "/api/pods/7/stop?", base + "/api/conversations/7/stop#", base + "/x",
                    base + "?q=1", base + "#f", f"http://user:hunter2@127.0.0.1:{self.fake.port}",
                    "http://hunter2@127.0.0.1", "ftp://127.0.0.1", "http://", "http://h:99999", "127.0.0.1:8080"):
            for script, args in ((CLIENT, ["where"]), (CLIENT, ["create"]), (WAITER, ["1", "45", "0.01", "--no-github"])):
                with self.subTest(url=url, script=os.path.basename(script), args=args[0]):
                    self.fake.clear()
                    r = self.run_script(script, *args, env=base_env(SMELT_API_URL=url))
                    self.assertEqual(r.returncode, 2, r.stdout + r.stderr)
                    self.assertNotIn("hunter2", r.stdout + r.stderr)
                    self.assertNotIn("Traceback", r.stderr)
                    self.assertIn("SMELT_API_URL", r.stderr)
                    self.assertEqual(self.sent(), [])

    def test_a_url_of_scheme_host_and_port_is_kept(self):
        for url, want in (("https://smelt.example/", "https://smelt.example"), ("http://x:1", "http://x:1"),
                          ("HTTP://Smelt.Example:8080", "HTTP://Smelt.Example:8080")):
            with self.subTest(url=url):
                self.assertEqual(smelt_api.address({"SMELT_API_URL": url})[0], want)

    def host_sent(self, **env):
        self.fake.clear()
        r = self.api("where", env=self.env(**env))
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        return self.fake.requests()[-1]["headers"]["host"], r.stdout

    def test_the_url_s_host_is_sent_unless_smelt_api_host_names_one(self):
        host, out = self.host_sent()
        self.assertEqual(host, f"127.0.0.1:{self.fake.port}")
        host, out = self.host_sent(SMELT_API_HOST="smelt.constantlee.us")
        self.assertEqual(host, "smelt.constantlee.us")
        self.assertIn(self.fake.url, out)
        self.assertIn("smelt.constantlee.us", out)
        self.assertIn("fakebuild0001", out)


class ExitCodes(FakeCase):
    def test_a_403_or_500_exits_1_and_prints_the_body(self):
        for status in (403, 500):
            with self.subTest(status=status):
                self.st.forced[("GET", "/api/conversations/tree")] = (status, {"message": f"refused {status}"})
                r = self.api("tree", "1")
                self.assertEqual(r.returncode, 1)
                self.assertIn(f"refused {status}", r.stdout + r.stderr)
                self.assertIn(str(status), r.stdout + r.stderr)

    def test_a_refused_connection_exits_2(self):
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.close()
        r = self.api("where", env=base_env(SMELT_API_URL=f"http://127.0.0.1:{port}"))
        self.assertEqual(r.returncode, 2, r.stdout + r.stderr)

    def test_create_prints_the_new_id_alone_first(self):
        r = self.api("create")
        self.assertEqual(r.returncode, 0)
        self.assertEqual(r.stdout.splitlines()[0], "100")


class ModelSet(FakeCase):
    def setUp(self):
        super().setUp()
        self.add_conv(7)

    def test_a_model_the_server_kept_exits_1(self):
        self.st.ignore_model_set = True
        r = self.api("model-set", "7", "2", "strata-model")
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertIn("Default", r.stdout + r.stderr)

    def test_a_model_read_back_as_chosen_exits_0(self):
        r = self.api("model-set", "7", "2", "strata-model")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("Chosen", r.stdout)

    def test_a_non_integer_provider_exits_2(self):
        r = self.api("model-set", "7", "two", "m")
        self.assertEqual(r.returncode, 2)
        self.assertEqual(self.sent(), [])


class Send(FakeCase):
    def setUp(self):
        super().setUp()
        self.add_conv(7)
        self.brief = self.tmpfile("brief.md", "the brief, caf\u00e9\n")

    def test_the_wrong_model_sends_nothing(self):
        models = [
            self.st.default_model(),
            {"state": "NoDefault"},
            {"state": "NoProviders"},
            {"state": "Chosen", "choice": fake_smelt.choice(1, "other-model")},
            {"state": "Chosen", "choice": fake_smelt.choice(2, "claude-opus-5-5")},
        ]
        for model in models:
            with self.subTest(model=model):
                self.st.models[7] = model
                self.fake.clear()
                r = self.api("send", "7", self.brief, "--expect-model", "1:claude-opus-5-5")
                self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
                self.assertEqual(self.sent("POST"), [])

    def test_the_expected_model_sends_once(self):
        self.st.models[7] = {"state": "Chosen", "choice": fake_smelt.choice(1, "claude-opus-5-5")}
        r = self.api("send", "7", self.brief, "--expect-model", "1:claude-opus-5-5")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        posts = [q for q in self.fake.requests() if q["method"] == "POST"]
        self.assertEqual(len(posts), 1)
        self.assertEqual(posts[0]["body"], {"content": "the brief, caf\u00e9\n"})

    def test_stdin_is_read_and_an_empty_brief_is_refused(self):
        r = self.api("send", "7", "-", stdin="from stdin")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.fake.requests()[-1]["body"], {"content": "from stdin"})
        self.fake.clear()
        r = self.api("send", "7", self.tmpfile("empty.md", "  \n"))
        self.assertEqual(r.returncode, 2)
        self.assertEqual(self.sent(), [])

    def test_a_bad_expect_model_is_usage(self):
        r = self.api("send", "7", self.brief, "--expect-model", "claude-opus-5-5")
        self.assertEqual(r.returncode, 2)
        self.assertEqual(self.sent(), [])


class Answer(FakeCase):
    def setUp(self):
        super().setUp()
        self.add_conv(7)
        self.st.questions[7] = json.loads(json.dumps(QUESTION))

    def refused(self, *answers, tool_use_id="tu1"):
        self.fake.clear()
        r = self.api("answer", "7", tool_use_id, *answers)
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertEqual(self.sent("POST"), [])
        return r

    def test_each_bad_answer_sends_nothing(self):
        self.refused("A", "X", tool_use_id="tu2")
        self.refused("A")
        self.refused("A", "X", "Y")
        self.refused("C", "X")
        self.refused("A|B", "X")
        self.refused("A", "X|W")
        self.refused("other:", "X")
        self.st.questions[7] = None
        r = self.refused("A", "X")
        self.assertIn("no question", r.stdout + r.stderr)

    def test_labels_multi_select_and_other_make_the_body(self):
        r = self.api("answer", "7", "tu1", "other:my own words", "X|Z")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.fake.requests()[-1]["body"], {
            "tool_use_id": "tu1",
            "answers": [{"selected": [], "other": "my own words"}, {"selected": ["X", "Z"]}],
        })
        self.st.questions[7] = json.loads(json.dumps(QUESTION))
        r = self.api("answer", "7", "tu1", "B", "Y")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.fake.requests()[-1]["body"]["answers"], [{"selected": ["B"]}, {"selected": ["Y"]}])


class Reads(FakeCase):
    def test_tree_shows_descendants_at_any_depth_and_nothing_else(self):
        self.add_conv(1, persona="smelt:ticket-driver", title="Driver")
        self.add_conv(2, 1, persona="smelt:developer", status="running", busy=True)
        self.add_conv(3, 2, waiting_for_user=True)
        self.add_conv(4, 3, report_owed_at="2026-11-11T12:30:00")
        self.add_conv(5, title="Someone else")
        self.add_conv(6, 5)
        r = self.api("tree", "1")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        lines = r.stdout.splitlines()
        ids = [int(line.split()[0]) for line in lines]
        self.assertEqual(ids, [1, 2, 3, 4])
        indents = [len(line) - len(line.lstrip()) for line in lines]
        self.assertEqual(indents, sorted(indents))
        self.assertLess(indents[0], indents[3])
        self.assertIn("smelt:developer", lines[1])
        self.assertIn("busy", lines[1])
        self.assertIn("waiting", lines[2])
        self.assertIn("2026-11-11T12:30:00", lines[3])
        r = self.api("tree", "1", "--json")
        self.assertEqual(sorted(n["id"] for n in json.loads(r.stdout)), [1, 2, 3, 4])
        r = self.api("tree", "99")
        self.assertEqual(r.returncode, 1)

    def test_messages_keeps_the_last_k_and_cuts_long_blocks(self):
        self.add_conv(7)
        long = "x" * 2000
        self.st.messages[7] = [
            fake_smelt.message(1, 7, "user", [{"type": "text", "text": "first"}]),
            fake_smelt.message(2, 7, "assistant", [{"type": "text", "text": long},
                                                   {"type": "tool_use", "id": "t", "name": "notify_user",
                                                    "input": {"message": "done"}}]),
            fake_smelt.message(3, 7, "user", [{"type": "tool_result", "tool_use_id": "t", "content": "ok"}]),
            {"id": 4, "conversation_id": 7, "role": "assistant", "content": "plain old text",
             "created_at": fake_smelt.STAMP},
        ]
        r = self.api("messages", "7", "--last", "3")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertNotIn("first", r.stdout)
        self.assertIn("notify_user", r.stdout)
        self.assertIn("tool_result", r.stdout)
        self.assertIn("plain old text", r.stdout)
        self.assertNotIn("x" * 501, r.stdout)
        self.assertIn("x" * 400, r.stdout)
        r = self.api("messages", "7", "--json", "--last", "1")
        self.assertEqual([m["id"] for m in json.loads(r.stdout)], [4])

    def test_question_prints_none_when_nothing_waits(self):
        self.add_conv(7)
        r = self.api("question", "7")
        self.assertEqual((r.returncode, r.stdout.strip()), (0, "none"))
        self.st.questions[7] = QUESTION
        r = self.api("question", "7")
        self.assertEqual(json.loads(r.stdout)["tool_use_id"], "tu1")

    def test_pods_shows_each_pod_with_its_running_commands(self):
        self.st.pods = [{"pod_id": 3, "conversation_id": 7, "conversation_title": "Dev", "status": "Running"}]
        self.st.pod_commands = [{"conversation_id": 7, "running_commands": 2}]
        r = self.api("pods")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        line = r.stdout.splitlines()[0].split()
        self.assertEqual(line[:3], ["7", "Running", "2"])

    def test_providers_shows_id_kind_name_and_never_the_url_or_hint(self):
        r = self.api("providers")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(r.stdout.splitlines(), ["1 claude_cli ClaudeCode", "2 llama_cpp strata"])
        self.assertNotIn("strata:8080", r.stdout)
        self.assertNotIn("abcd", r.stdout)
