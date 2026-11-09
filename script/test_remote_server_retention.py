"""Source guards for removed startup pruning; not an end-to-end SSH test."""

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


class RemoteServerRetentionTests(unittest.TestCase):
    def test_ssh_setup_ends_after_install_or_reuse(self):
        source = (ROOT / "crates/remote/src/transport/ssh.rs").read_text()
        tail = source.split("this.remote_binary_path = Some(", 1)[1].split(
            "    async fn ensure_server_binary(", 1
        )[0]
        code = "\n".join(line.split("//", 1)[0] for line in tail.splitlines())
        self.assertEqual(
            "".join(code.split()),
            "this.ensure_server_binary(&delegate,release_channel,version,cx)"
            ".await?,);Ok(this)}",
        )
        for suffix in ("py", "ps1"):
            self.assertFalse(
                (ROOT / f"crates/remote/src/transport/cleanup_remote_servers.{suffix}").exists()
            )

    def test_server_startup_has_no_legacy_binary_pruning(self):
        source = (ROOT / "crates/remote_server/src/server.rs").read_text()
        for obsolete in (
            "cleanup_old_binaries", "is_older_server_version", "is_file_in_use",
            "remote_wsl_server_dir_relative", "removing old remote server binary",
        ):
            self.assertNotIn(obsolete, source)
        tail = source.split("handle_crash_files_requests(&project, &session);", 1)[1].split(
            "mem::forget(project);", 1
        )[0]
        code = "\n".join(line.split("//", 1)[0] for line in tail.splitlines())
        self.assertEqual(code.strip(), "")


if __name__ == "__main__":
    unittest.main()
