"""scripts/pm/smelt-api's `archive` and `unarchive` (SME-460, on SME-480's
routes), against scripts/pm/fake_smelt.py."""

from helpers import FakeCase

TREE = ("GET", "/api/conversations/tree")
COMMANDS = ("GET", "/api/pods/commands")
# archive reads the tree and the pods' running commands before it posts.
IDLE_CHECK = [TREE, COMMANDS]


class Archive(FakeCase):
    def setUp(self):
        super().setUp()
        self.add_conv(7, title="Driver")
        self.add_conv(8, 7, persona="smelt:developer")
        self.add_conv(9, 8, persona="smelt:code-reviewer")

    def test_archive_posts_then_reads_the_tree_back_archived(self):
        r = self.api("archive", "7")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.sent(), IDLE_CHECK + [("POST", "/api/conversations/7/archive"), TREE])
        self.assertIn("archived", r.stdout)
        self.assertIsNotNone(self.st.nodes[0]["archived_at"])
        self.assertIsNotNone(self.st.nodes[1]["archived_at"], "its agents follow it")

    def test_unarchive_posts_then_reads_the_tree_back_unarchived(self):
        self.api("archive", "7")
        self.fake.clear()
        r = self.api("unarchive", "7")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.sent(), [("POST", "/api/conversations/7/unarchive"), TREE])
        self.assertIn("unarchived", r.stdout)
        self.assertIsNone(self.st.nodes[0]["archived_at"])

    def test_a_server_refusal_exits_1_and_prints_it(self):
        # An agent (409, naming its root), a missing id, and a forced 500.
        r = self.api("archive", "8")
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertIn("409", r.stdout + r.stderr)
        self.assertIn("conversation 7", r.stdout + r.stderr)
        self.assertEqual(self.sent(), IDLE_CHECK + [("POST", "/api/conversations/8/archive")])
        for args, before in ((["archive", "99"], IDLE_CHECK), (["unarchive", "99"], [])):
            with self.subTest(args=args):
                self.fake.clear()
                r = self.api(*args)
                self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
                # src/api/chat.rs's conversation_not_found, the real 404's text.
                self.assertIn("HTTP 404", r.stderr)
                self.assertIn("conversation not found", r.stderr)
                self.assertEqual(self.sent(), before + [("POST", f"/api/conversations/99/{args[0]}")])
        self.st.forced[("POST", "/api/conversations/7/unarchive")] = (500, {"message": "refused 500"})
        r = self.api("unarchive", "7")
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertIn("refused 500", r.stdout + r.stderr)

    def test_a_read_back_that_disagrees_exits_1(self):
        # The POST answered 200 but the tree doesn't show the change, or
        # doesn't list the conversation at all.
        self.st.ignore_archive = True
        r = self.api("archive", "7")
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertEqual(self.sent(), IDLE_CHECK + [("POST", "/api/conversations/7/archive"), TREE])
        self.st.ignore_archive = False
        self.api("archive", "7")
        self.st.ignore_archive = True
        r = self.api("unarchive", "7")
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.st.forced[TREE] = (200, [])
        self.st.ignore_archive = False
        r = self.api("archive", "7")
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertIn("isn't in the tree", r.stdout + r.stderr)

    def test_a_tree_with_anything_live_is_refused_and_nothing_is_posted(self):
        # An archived tree drops out of the sidebar unless something is owed,
        # and a busy seat doesn't count (sidebar/archive.rs): so archive
        # refuses while any node in the tree is busy, waiting or owes a
        # report, or any of its conversations' pods runs a command.
        live = [
            ("busy", lambda: self.st.nodes[2].update(busy=True)),
            ("waiting", lambda: self.st.nodes[1].update(waiting_for_user=True)),
            ("owed", lambda: self.st.nodes[0].update(report_owed_at="2026-11-12T18:00:00Z")),
            ("running", lambda: self.st.pod_commands.append({"conversation_id": 9, "running_commands": 1})),
        ]
        for word, make_live in live:
            with self.subTest(word=word):
                self.setUp()
                make_live()
                r = self.api("archive", "7")
                self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
                self.assertIn(word, r.stderr)
                self.assertIn("nothing sent", r.stderr)
                self.assertEqual(self.sent(), IDLE_CHECK)
                self.assertIsNone(self.st.nodes[0]["archived_at"])

    def test_another_tree_s_live_seat_or_an_idle_pod_doesnt_refuse(self):
        self.add_conv(20, title="another Driver", busy=True)
        self.st.pod_commands += [{"conversation_id": 20, "running_commands": 2},
                                 {"conversation_id": 8, "running_commands": 0}]
        r = self.api("archive", "7")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIsNone(self.st.nodes[3]["archived_at"])

    def test_unarchive_doesnt_check(self):
        self.api("archive", "7")
        self.st.nodes[1]["busy"] = True
        self.fake.clear()
        r = self.api("unarchive", "7")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.sent(), [("POST", "/api/conversations/7/unarchive"), TREE])

    def test_a_non_integer_id_exits_2_and_sends_nothing(self):
        for args in (["archive", "7a"], ["unarchive", "../7"], ["archive", "-1"], ["unarchive"]):
            with self.subTest(args=args):
                self.fake.clear()
                r = self.api(*args)
                self.assertEqual(r.returncode, 2, r.stdout + r.stderr)
                self.assertEqual(self.sent(), [])
