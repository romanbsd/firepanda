"""Offline tests for all-or-nothing FetchContent tag updates."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('bump', ROOT / 'scripts/bump_fetchcontent_tags.py')
bump = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bump)


class Updates(unittest.TestCase):
    def test_both_tags_and_idempotence(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'CMakeLists.txt'
            original = (ROOT / 'CMakeLists.txt').read_text()
            path.write_text(original)
            with patch.object(bump, 'latest_tag', return_value='v99.0.0'):
                self.assertEqual(bump.main(str(path)), 0)
                updated = path.read_text()
                self.assertEqual(updated.count('GIT_TAG v99.0.0'), 2)
                self.assertEqual(bump.main(str(path)), 0)
                self.assertEqual(path.read_text(), updated)
            self.assertIn('GIT_SHALLOW TRUE', updated)

    def test_failure_never_partially_writes(self):
        for tags in (['v99.0.0', None], ['v99.0.0', 'v1.0.0)\nmessage(INJECTED)']):
            with tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / 'CMakeLists.txt'
                original = (ROOT / 'CMakeLists.txt').read_text()
                path.write_text(original)
                with patch.object(bump, 'latest_tag', side_effect=tags):
                    self.assertEqual(bump.main(str(path)), 1)
                self.assertEqual(path.read_text(), original)

    def test_release_and_fallback(self):
        with patch.object(bump, 'api_json', return_value={'tag_name': 'v2.0.0'}):
            self.assertEqual(bump.latest_tag('owner/repo'), 'v2.0.0')
        with patch.object(bump, 'api_json', side_effect=[None, [{'name': 'nightly'}, {'name': 'v1.2.3'}]]):
            self.assertEqual(bump.latest_tag('owner/repo'), 'v1.2.3')


if __name__ == '__main__':
    unittest.main()
