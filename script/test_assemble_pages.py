import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('assemble_pages', Path(__file__).with_name('assemble-pages.py'))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class AssemblePagesTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.website = self.root / 'dist'
        self.output = self.root / 'pages'
        self.website.mkdir()
        self.output.mkdir()
        (self.website / 'index.html').write_text('<h1>Zed CN</h1>')
        self.feed = b'{"schema_version":1,"releases":[]}\n'
        (self.output / 'updates.json').write_bytes(self.feed)

    def test_preserves_both_feeds_byte_for_byte(self):
        (self.output / 'updates-dev.json').write_bytes(self.feed)
        (self.website / 'assets').mkdir()
        (self.website / 'assets' / 'main.js').write_text('console.log("hello")')
        module.assemble(self.website, self.output)
        for name in ('updates.json', 'updates-dev.json'):
            self.assertEqual((self.output / name).read_bytes(), self.feed)
        self.assertTrue((self.output / 'index.html').exists())
        self.assertTrue((self.output / 'assets/main.js').exists())
        self.assertTrue((self.output / '.nojekyll').exists())

    def test_rejects_feed_overwrite_before_copy(self):
        (self.website / 'updates.json').write_text('{}')
        with self.assertRaises(ValueError):
            module.assemble(self.website, self.output)
        self.assertEqual((self.output / 'updates.json').read_bytes(), self.feed)
        self.assertFalse((self.output / 'index.html').exists())

    def test_missing_stable_feed(self):
        (self.output / 'updates.json').unlink()
        with self.assertRaises(ValueError):
            module.assemble(self.website, self.output)

    def test_invalid_feed(self):
        (self.output / 'updates.json').write_text('{}')
        with self.assertRaises(ValueError):
            module.assemble(self.website, self.output)

    def test_symlink_is_rejected(self):
        (self.website / 'leak').symlink_to(self.root)
        with self.assertRaises(ValueError):
            module.assemble(self.website, self.output)

    def test_output_symlink_is_rejected_without_external_write(self):
        external = self.root / 'external'
        external.mkdir()
        (self.output / 'assets').symlink_to(external)
        (self.website / 'assets').mkdir()
        (self.website / 'assets/main.js').write_text('hello')
        with self.assertRaises(ValueError):
            module.assemble(self.website, self.output)
        self.assertFalse((external / 'main.js').exists())

    def test_workflow_keeps_single_pages_artifact_and_existing_triggers(self):
        workflow = (Path(__file__).resolve().parents[1] / '.github/workflows/publish-update-manifest.yml').read_text()
        self.assertEqual(workflow.count('uses: actions/deploy-pages@'), 1)
        self.assertIn('workflows: [Build multiplatform release]', workflow)
        self.assertIn('workflow_dispatch:', workflow)
        self.assertIn('branches: [main]', workflow)
        self.assertIn('cancel-in-progress: false', workflow)
        self.assertIn('python3 script/generate-update-manifest.py --output update-site/updates.json', workflow)
        self.assertIn('--channel dev --output update-site/updates-dev.json', workflow)
        self.assertIn('python3 script/assemble-pages.py', workflow)
        self.assertIn('path: update-site', workflow)
        self.assertIn('cmp update-site/index.html deployed-index.html', workflow)
        self.assertLess(workflow.index('npm test'), workflow.index('uses: actions/upload-pages-artifact@'))

    def test_overlapping_directories(self):
        with self.assertRaises(ValueError):
            module.assemble(self.website, self.website)


if __name__ == '__main__':
    unittest.main()
