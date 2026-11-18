"""Isolated imports for tests of the review-arms brief writer."""
import importlib
import os
from pathlib import Path
import sys
import unittest
from unittest import mock


SCRIPTS = Path(__file__).resolve().parent
MODULES = ('brief_writer_terra', 'brief_ablation', 'brief_driver', 'brief_oracle',
           'brief_score', 'driver')


class WriterTestCase(unittest.TestCase):
    def load_writer(self, root):
        env = {name: value for name, value in os.environ.items() if not name.startswith('REVIEW_ARMS_')}
        env.update(REVIEW_ARMS_DIR=str(root), REVIEW_ARMS_REPO=str(SCRIPTS.parents[2]))
        self.enterContext(mock.patch.dict(os.environ, env, clear=True))

        # Hard-won: fcffcf032: discovery reused the first writer's import-time data root.
        saved = {name: sys.modules.pop(name) for name in MODULES if name in sys.modules}
        try:
            return importlib.import_module('brief_writer_terra')
        finally:
            for name in MODULES:
                sys.modules.pop(name, None)
            sys.modules.update(saved)
