"""scripts/pm/smelt-api's `archive` and `unarchive` (SME-460, on SME-480's
routes), against scripts/pm/fake_smelt.py."""

from helpers import FakeCase

TREE = ("GET", "/api/conversations/tree")


class Archive(FakeCase):
    def setUp(self):
        super().setUp()
        self.add_conv(7, title="Driver")
        self.add_conv(8, 7, persona="smelt:developer")

    def test_archive_posts_then_reads_the_tree_back_archived(self):
        r = self.api("archive", "7")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.sent(), [("POST", "/api/conversations/7/archive"), TREE])
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
        self.assertEqual(self.sent(), [("POST", "/api/conversations/8/archive")])
        for args in (["archive", "99"], ["unarchive", "99"]):
            with self.subTest(args=args):
                self.fake.clear()
                r = self.api(*args)
                self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
                self.assertEqual(self.sent(), [("POST", f"/api/conversations/99/{args[0]}")])
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
        self.assertEqual(self.sent(), [("POST", "/api/conversations/7/archive"), TREE])
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

    def test_a_non_integer_id_exits_2_and_sends_nothing(self):
        for args in (["archive", "7a"], ["unarchive", "../7"], ["archive", "-1"], ["unarchive"]):
            with self.subTest(args=args):
                self.fake.clear()
                r = self.api(*args)
                self.assertEqual(r.returncode, 2, r.stdout + r.stderr)
                self.assertEqual(self.sent(), [])
