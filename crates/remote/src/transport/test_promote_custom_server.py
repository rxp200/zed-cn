import importlib.util
from pathlib import Path
import tempfile
import threading
import unittest

spec = importlib.util.spec_from_file_location("promotion", Path(__file__).with_name("promote_custom_server.py"))
promotion = importlib.util.module_from_spec(spec)
spec.loader.exec_module(promotion)


class PromotionTests(unittest.TestCase):
    def test_existing_file_directory_and_symlink_are_never_replaced(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            staged = root / "staged"
            staged.write_bytes(b"new")
            destination = root / "server"
            destination.write_bytes(b"old")
            promotion.promote(staged, destination)
            self.assertEqual(destination.read_bytes(), b"old")
            destination.unlink()
            destination.mkdir()
            promotion.promote(staged, destination)
            self.assertTrue(destination.is_dir())
            destination.rmdir()
            destination.symlink_to(root / "missing")
            promotion.promote(staged, destination)
            self.assertTrue(destination.is_symlink())

    def test_concurrent_install_keeps_one_complete_winner(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stages = [root / "first", root / "second"]
            contents = [b"a" * 65536, b"b" * 65536]
            for stage, content in zip(stages, contents):
                stage.write_bytes(content)
                stage.chmod(0o755)
            barrier = threading.Barrier(2)
            errors = []
            def install(stage):
                try:
                    barrier.wait(timeout=5)
                    promotion.promote(stage, root / "server")
                except Exception as error:
                    errors.append(error)
            threads = [threading.Thread(target=install, args=(stage,)) for stage in stages]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join(timeout=10)
                self.assertFalse(thread.is_alive())
            self.assertFalse(errors)
            for stage in stages:
                stage.unlink()
            self.assertIn((root / "server").read_bytes(), contents)
            self.assertTrue((root / "server").stat().st_mode & 0o111)

    def test_other_errors_propagate(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(FileNotFoundError):
                promotion.promote(Path(directory) / "missing", Path(directory) / "server")


if __name__ == "__main__":
    unittest.main()
