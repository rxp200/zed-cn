import os
import tempfile
import socket
import sys
import unittest
from pathlib import Path

from port_forwarding_probe import project_listener_ports


class ProbeTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform.startswith("linux"), "requires procfs")
    def test_real_listener_and_unrelated_root(self):
        with socket.socket() as listener, tempfile.TemporaryDirectory() as unrelated:
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            port = listener.getsockname()[1]
            self.assertEqual(project_listener_ports([os.getcwd()], {port}), [port])
            self.assertEqual(project_listener_ports([unrelated], {port}), [])
        self.assertEqual(project_listener_ports([os.getcwd()], {port}), [])

    def test_only_project_owned_listeners(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            proc = root / "proc"
            (proc / "net").mkdir(parents=True)
            (proc / "net" / "tcp").write_text(
                "header\n"
                "0: 0100007F:1F90 00000000:0000 0A 0 0 0 0 0 101\n"
                "1: 0100007F:1F91 00000000:0000 0A 0 0 0 0 0 102\n"
                "2: 0100007F:1F92 00000000:0000 01 0 0 0 0 0 103\n"
            )
            project = root / "project"
            for pid, cwd, inode in [(1, project / "src", 101),
                                    (2, root / "project-other", 102),
                                    (3, project, 103)]:
                process = proc / str(pid)
                (process / "fd").mkdir(parents=True)
                os.symlink(cwd, process / "cwd")
                os.symlink(f"socket:[{inode}]", process / "fd" / "4")
            self.assertEqual(project_listener_ports([str(project)], {8080, 8081, 8082}, str(proc)), [8080])
            self.assertEqual(project_listener_ports([str(project)], {8081}, str(proc)), [])
            self.assertEqual(project_listener_ports([], {8080}, str(proc)), [])


if __name__ == "__main__":
    unittest.main()
