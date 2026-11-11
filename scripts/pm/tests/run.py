"""Runs scripts/pm/tests/test_*.py (SME-460); scripts/test-pm calls it.

Arguments are passed to unittest (`-k pattern`, `-v`)."""

import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

if __name__ == "__main__":
    argv = [sys.argv[0], "discover", "-s", HERE, "-t", HERE, "-p", "test_*.py", *sys.argv[1:]]
    unittest.main(module=None, argv=argv)
