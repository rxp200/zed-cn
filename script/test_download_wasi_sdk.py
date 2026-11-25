#!/usr/bin/env python3
"""Regressions for script/download-wasi-sdk retry and integrity handling.

The script talks to a local HTTP server that replays failure plans, so the
checks cover transient HTTP errors, truncated transfers, exhausted attempts
and an already-installed SDK without touching the real WASI SDK release.
"""
import io
import os
import platform
import subprocess
import tarfile
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "download-wasi-sdk"
VERSION = "34"


def archive_layout():
    architecture = {"x86_64": "x86_64", "amd64": "x86_64"}.get(platform.machine().lower())
    if architecture is None:
        if platform.machine().lower() in {"arm64", "aarch64"}:
            architecture = "arm64"
        else:
            raise unittest.SkipTest(f"unsupported architecture: {platform.machine()}")
    system = {"darwin": "macos", "linux": "linux"}.get(platform.system().lower())
    if system is None:
        raise unittest.SkipTest(f"unsupported platform: {platform.system()}")
    return f"wasi-sdk-{VERSION}.0-{architecture}-{system}"


def build_archive(directory_name, version=f"{VERSION}.0"):
    buffer = io.BytesIO()
    payload = b"fake compiler\n"
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        for name, data in (
            (f"{directory_name}/VERSION", f"{version}\n".encode()),
            (f"{directory_name}/bin/wasm-ld", payload),
        ):
            info = tarfile.TarInfo(name)
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
    return buffer.getvalue()


class Server:
    """Serves one archive and plays a per-request plan of ok/503/truncate modes."""

    def __init__(self, body, plan):
        self.body = body
        self.plan = list(plan)
        self.requests = 0
        self.paths = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def do_GET(self):
                owner.paths.append(self.path)
                index = min(owner.requests, len(owner.plan) - 1)
                mode = owner.plan[index]
                owner.requests += 1
                if mode == "503":
                    self.send_response(503)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                self.send_response(200)
                self.send_header("Content-Type", "application/gzip")
                self.send_header("Content-Length", str(len(owner.body)))
                self.end_headers()
                if mode == "truncate":
                    self.wfile.write(owner.body[: len(owner.body) // 2])
                    self.wfile.flush()
                    self.close_connection = True
                    return
                self.wfile.write(owner.body)

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)

    @property
    def base_url(self):
        host, port = self.httpd.server_address[:2]
        return f"http://{host}:{port}/download"

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_exc):
        self.httpd.shutdown()
        self.httpd.server_close()
        self.thread.join()


class DownloadWasiSdkTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="download-wasi-sdk-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.target = self.root / "target"
        self.directory_name = archive_layout()

    def run_script(self, server, attempts=3):
        environment = dict(
            os.environ,
            WASI_SDK_BASE_URL=server.base_url,
            WASI_SDK_ATTEMPTS=str(attempts),
            WASI_SDK_RETRY_DELAY="0",
        )
        return subprocess.run(
            ["bash", str(SCRIPT)],
            cwd=self.root,
            env=environment,
            capture_output=True,
            text=True,
        )

    def leftovers(self):
        if not self.target.exists():
            return []
        return sorted(path.name for path in self.target.iterdir())

    def assert_installed(self):
        version_file = self.target / "wasi-sdk" / "VERSION"
        self.assertTrue(version_file.is_file(), self.leftovers())
        self.assertEqual(version_file.read_text().strip(), f"{VERSION}.0")
        self.assertEqual(self.leftovers(), ["wasi-sdk"])

    def test_retries_after_transient_server_errors(self):
        with Server(build_archive(self.directory_name), ["503", "503", "ok"]) as server:
            result = self.run_script(server)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(server.requests, 3)
        self.assertEqual(
            server.paths[0],
            f"/download/wasi-sdk-{VERSION}/{self.directory_name}.tar.gz",
        )
        self.assert_installed()

    def test_retries_after_truncated_download(self):
        with Server(build_archive(self.directory_name), ["truncate", "ok"]) as server:
            result = self.run_script(server)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(server.requests, 2)
        self.assert_installed()

    def test_fails_without_partial_install_when_attempts_are_exhausted(self):
        with Server(build_archive(self.directory_name), ["503"]) as server:
            result = self.run_script(server, attempts=3)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(server.requests, 3)
        self.assertIn("after 3 attempt(s)", result.stderr)
        self.assertEqual(self.leftovers(), [])

    def test_rejects_archive_with_another_version(self):
        with Server(build_archive(self.directory_name, version="1.0"), ["ok"]) as server:
            result = self.run_script(server)
        self.assertEqual(result.returncode, 1)
        self.assertIn("expected '34.0'", result.stderr)
        self.assertEqual(self.leftovers(), [])

    def test_reuses_an_installed_sdk_without_downloading(self):
        (self.target / "wasi-sdk").mkdir(parents=True)
        (self.target / "wasi-sdk" / "VERSION").write_text(f"{VERSION}.0\n")
        with Server(build_archive(self.directory_name), ["ok"]) as server:
            result = self.run_script(server)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(server.requests, 0)
        self.assertEqual(self.leftovers(), ["wasi-sdk"])

    def test_replaces_an_outdated_install(self):
        (self.target / "wasi-sdk").mkdir(parents=True)
        (self.target / "wasi-sdk" / "VERSION").write_text("33.0\n")
        (self.target / "wasi-sdk" / "stale").write_text("old\n")
        with Server(build_archive(self.directory_name), ["ok"]) as server:
            result = self.run_script(server)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_installed()
        self.assertFalse((self.target / "wasi-sdk" / "stale").exists())


if __name__ == "__main__":
    unittest.main()
