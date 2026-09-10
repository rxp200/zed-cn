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
    def test_remote_difference_blocks_before_mutation_and_network(self):
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
                Path('crates/remote_server').mkdir()
                Path('crates/remote_server/server.rs').write_text('changed\n')
                git('add', '.')
                git('commit', '-qm', 'Remote change')
                source = git('rev-parse', 'HEAD')
                git('checkout', '--detach', stable)
                before = git('write-tree')
                args = ['prepare', '--source-ref', source, '--stable-tag', 'v1.2.3', '--release-tag', 'zed-cn-v1.2.3-r1']
                for extra in ([], ['--remote-review-approved', source + ':' + '0' * 40]):
                    with patch('sys.argv', args + extra), patch.object(MODULE, 'resolve_previous') as network, contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                        self.assertEqual(MODULE.main(), 2)
                        network.assert_not_called()
                    self.assertEqual(git('write-tree'), before)
                    self.assertFalse(Path('crates/remote_server/server.rs').exists())
                with patch('sys.argv', args + ['--remote-review-approved', source + ':' + stable]), patch.object(MODULE, 'resolve_previous', side_effect=RuntimeError('approved gate reached')) as network:
                    with self.assertRaisesRegex(RuntimeError, 'approved gate reached'):
                        MODULE.main()
                    network.assert_called_once()
            finally:
                os.chdir(previous)


if __name__ == '__main__':
    unittest.main()
