import contextlib
import importlib.machinery
import importlib.util
import io
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

LOADER = importlib.machinery.SourceFileLoader('prepare_stable', str(Path(__file__).with_name('prepare-public-stable-release')))
MODULE = importlib.util.module_from_spec(importlib.util.spec_from_loader(LOADER.name, LOADER))
LOADER.exec_module(MODULE)


class RemoteCheckpointTests(unittest.TestCase):
    def check_security_gate(self, remote_difference):
        previous = Path.cwd()
        with tempfile.TemporaryDirectory() as directory:
            try:
                os.chdir(directory)
                def git(*args):
                    return subprocess.check_output(['git', *args], stderr=subprocess.DEVNULL, text=True).strip()
                git('init', '-q')
                git('config', 'user.name', 'Test')
                git('config', 'user.email', 'test@example.com')
                Path('crates/zed').mkdir(parents=True)
                Path('crates/zed/Cargo.toml').write_text('[package]\nname = "zed"\nversion = "1.2.3"\n')
                Path('crates/zed/RELEASE_CHANNEL').write_text('stable\n')
                git('add', '.')
                git('commit', '-qm', 'Stable')
                stable = git('rev-parse', 'HEAD')
                git('tag', 'v1.2.3')
                if remote_difference:
                    Path('crates/remote_server').mkdir()
                    Path('crates/remote_server/server.rs').write_text('changed\n')
                    git('add', '.')
                    git('commit', '-qm', 'Remote change')
                source = git('rev-parse', 'HEAD')
                git('checkout', '--detach', stable)
                before = git('write-tree')
                args = ['prepare', '--source-ref', source, '--stable-tag', 'v1.2.3', '--release-tag', 'zed-cn-v1.2.3-r1']
                for extra in ([], ['--remote-security-reviewed', source + ':' + '0' * 40], ['--remote-security-reviewed', '0' * 40 + ':' + stable]):
                    with patch('sys.argv', args + extra), patch.object(MODULE, 'resolve_previous') as network, contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                        self.assertEqual(MODULE.main(), 2)
                        network.assert_not_called()
                    self.assertEqual(git('write-tree'), before)
                    self.assertFalse(Path('crates/remote_server/server.rs').exists())
                with patch('sys.argv', args + ['--remote-security-reviewed', source + ':' + stable]), patch.object(MODULE, 'resolve_previous', side_effect=RuntimeError('approved gate reached')) as network:
                    with self.assertRaisesRegex(RuntimeError, 'approved gate reached'):
                        MODULE.main()
                    network.assert_called_once()
                with patch('sys.argv', args + ['--remote-review-approved', source + ':' + stable]), contextlib.redirect_stderr(io.StringIO()):
                    with self.assertRaises(SystemExit) as error:
                        MODULE.main()
                    self.assertEqual(error.exception.code, 2)
            finally:
                os.chdir(previous)

    def test_remote_security_gate_before_mutation_and_network(self):
        self.check_security_gate(remote_difference=True)

    def test_security_review_required_without_remote_directory_diff(self):
        self.check_security_gate(remote_difference=False)


class ProtectedBaselineTests(unittest.TestCase):
    def generate(self, baseline_has_rules):
        previous = Path.cwd()
        with tempfile.TemporaryDirectory() as directory:
            try:
                os.chdir(directory)

                def git(*args):
                    return subprocess.check_output(['git', *args], stderr=subprocess.DEVNULL, text=True).strip()

                git('init', '-q')
                git('config', 'user.name', 'Test')
                git('config', 'user.email', 'test@example.com')
                Path('crates/zed').mkdir(parents=True)
                Path('crates/zed/Cargo.toml').write_text('[package]\nname = "zed"\nversion = "1.2.3"\n')
                Path('crates/zed/RELEASE_CHANNEL').write_text('stable\n')
                Path('Cargo.lock').write_text('version = 3\n\n[[package]]\nname = "zed"\nversion = "1.2.3"\n')
                Path('AGENTS.md').write_text('official agents\n')
                if baseline_has_rules:
                    Path('.rules').write_text('official rules\n')
                git('add', '.')
                git('commit', '-qm', 'Official Stable')
                stable = git('rev-parse', 'HEAD')
                git('tag', 'v1.2.3')

                Path('crates/foo.rs').write_text('implementation\n')
                Path('AGENTS.md').write_text('projection agents\n')
                if baseline_has_rules:
                    Path('.rules').write_text('projection rules\n')
                git('add', '.')
                git('commit', '-qm', 'Public projection')
                source = git('rev-parse', 'HEAD')
                git('checkout', '-q', '--detach', stable)

                args = [
                    'prepare',
                    '--source-ref', source,
                    '--stable-tag', 'v1.2.3',
                    '--release-tag', 'zed-cn-v1.2.3-r2',
                    '--remote-security-reviewed', source + ':' + stable,
                ]
                with patch('sys.argv', args), patch.object(
                    MODULE,
                    'resolve_previous',
                    return_value=({'tag_name': 'zed-cn-v1.2.2-r9', 'body': ''}, stable),
                ) as network, contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(MODULE.main(), 0)
                network.assert_called_once()

                staged = git('ls-files').splitlines()
                self.assertIn('crates/foo.rs', staged)
                self.assertEqual(git('cat-file', '-p', ':AGENTS.md'), 'official agents')
                self.assertEqual(git('cat-file', '-p', ':crates/zed/RELEASE_CHANNEL'), 'stable')
                self.assertIn('version = "1.2.3"', git('cat-file', '-p', ':crates/zed/Cargo.toml'))
                if baseline_has_rules:
                    self.assertEqual(git('cat-file', '-p', ':.rules'), 'official rules')
                else:
                    self.assertNotIn('.rules', staged)
            finally:
                os.chdir(previous)

    def test_protected_rules_are_overlaid_from_the_baseline(self):
        self.generate(baseline_has_rules=True)

    def test_baseline_without_rules_removes_it_from_the_generated_tree(self):
        self.generate(baseline_has_rules=False)


if __name__ == '__main__':
    unittest.main()
