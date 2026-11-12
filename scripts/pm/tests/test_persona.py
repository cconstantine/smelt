"""scripts/pm/smelt-api's `persona` and `persona-set` (SME-460, on SME-481's
top-level persona), against scripts/pm/fake_smelt.py."""

from helpers import FakeCase, fake_smelt

C = "/api/conversations/7/persona"


class PersonaSet(FakeCase):
    def setUp(self):
        super().setUp()
        self.add_conv(7)
        self.st.models[7] = {"state": "Chosen", "choice": fake_smelt.choice(1, "claude-opus-5-5", "ClaudeCode")}

    def test_a_set_applied_and_read_back_exits_0_naming_its_model(self):
        r = self.api("persona-set", "7", "smelt:project-manager")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.sent(), [("POST", C), ("GET", C)])
        self.assertEqual(self.fake.requests()[0]["body"], {"persona": "smelt:project-manager"})
        self.assertIn("smelt:project-manager", r.stdout)
        self.assertIn("claude-opus-5-5", r.stdout)
        self.assertIn("ClaudeCode", r.stdout)

    def test_the_model_it_ends_on_is_the_one_the_persona_pinned(self):
        # top-level.md "Model": a persona's smelt.model pins the conversation.
        self.st.persona_models["smelt:ticket-driver"] = (2, "strata-model")
        r = self.api("persona-set", "7", "smelt:ticket-driver")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("strata-model", r.stdout)
        self.assertIn("strata", r.stdout)

    def test_a_refusal_from_the_server_exits_1_and_prints_it(self):
        self.st.persona_refusals[7] = "smelt.model claude-x isn't listed on ClaudeCode; strata lists it"
        r = self.api("persona-set", "7", "smelt:ticket-driver")
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertIn("isn't listed on ClaudeCode", r.stdout + r.stderr)
        self.assertEqual(self.sent(), [("POST", C)])
        self.fake.clear()
        self.st.forced[("POST", C)] = (403, {"message": "refused 403"})
        r = self.api("persona-set", "7", "smelt:ticket-driver")
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertIn("refused 403", r.stdout + r.stderr)

    def test_an_unscoped_label_exits_2_and_sends_nothing(self):
        for label in ("ticket-driver", "smelt:", ":ticket-driver", "a:b:c", "smelt:Ticket-Driver",
                      "smelt:ticket driver", "../x:developer", "smelt/x:developer", "..:developer",
                      "smelt:" + "x" * 65, ""):
            with self.subTest(label=label):
                self.fake.clear()
                r = self.api("persona-set", "7", label)
                self.assertEqual(r.returncode, 2, r.stdout + r.stderr)
                self.assertEqual(self.sent(), [])

    def test_an_unapplied_read_back_exits_1(self):
        self.st.persona_unapplied = True
        r = self.api("persona-set", "7", "smelt:ticket-driver")
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertEqual(self.sent(), [("POST", C), ("GET", C)])
        self.assertIn("applied", r.stdout + r.stderr)

    def test_a_read_back_naming_another_persona_or_none_exits_1(self):
        other = {"label": "smelt:developer", "path": "/x", "commit": None, "applied": True,
                 "model": "m", "provider": "p"}
        for read in ({"set": other, "can_set": "set", "options": [], "notes": []},
                     {"set": None, "can_set": None, "options": [], "notes": []}):
            with self.subTest(read=read["set"]):
                self.st.forced[("GET", C)] = (200, read)
                self.st.personas.pop(7, None)
                r = self.api("persona-set", "7", "smelt:ticket-driver")
                self.assertEqual(r.returncode, 1, r.stdout + r.stderr)

    def test_a_non_integer_id_exits_2(self):
        r = self.api("persona-set", "7a", "smelt:ticket-driver")
        self.assertEqual(r.returncode, 2)
        self.assertEqual(self.sent(), [])


class PersonaRead(FakeCase):
    def setUp(self):
        super().setUp()
        self.add_conv(7)

    def test_an_unset_conversation_shows_can_set_and_the_option_labels(self):
        self.st.personas[7] = {"set": None, "can_set": None, "notes": ["other: its remote is untrusted"],
                               "options": [{"label": "smelt:developer", "description": "d", "error": None},
                                           {"label": "smelt:broken", "description": None, "error": "bad yaml"}]}
        r = self.api("persona", "7")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.sent(), [("GET", C)])
        self.assertIn("can_set", r.stdout)
        self.assertIn("smelt:developer", r.stdout)
        self.assertIn("smelt:broken", r.stdout)
        self.assertIn("bad yaml", r.stdout)
        self.assertIn("untrusted", r.stdout)

    def test_a_conversation_that_can_t_be_set_says_why(self):
        self.st.personas[7] = {"set": None, "can_set": "this conversation has started", "options": [], "notes": []}
        r = self.api("persona", "7")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("this conversation has started", r.stdout)

    def test_a_set_conversation_shows_label_commit_applied_model_and_provider(self):
        self.api("persona-set", "7", "smelt:developer")
        self.fake.clear()
        r = self.api("persona", "7")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.sent(), [("GET", C)])
        for want in ("smelt:developer", "d687835b", "applied", "True", "claude-opus-5-5", "ClaudeCode"):
            self.assertIn(want, r.stdout)
