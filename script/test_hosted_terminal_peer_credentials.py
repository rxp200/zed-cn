#!/usr/bin/env python3
"""Test the production peer check without building the remote_server dependency graph.

Run through the repository's build runner where available. --target checks a
cross target (including tests) without linking; install its Rust std first.
Every Cargo invocation has its own temporary target, removed even on failure.
"""

import argparse
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib


PRELUDE = r'''
#![allow(dead_code)]
mod net {
    pub mod async_net {
        // Only AsRawFd is used by the production check. This borrowed adapter
        // lets the error test pass -1 without constructing an invalid OwnedFd.
        pub struct UnixStream(pub std::os::fd::RawFd);
        impl std::os::fd::AsRawFd for UnixStream {
            fn as_raw_fd(&self) -> std::os::fd::RawFd { self.0 }
        }
    }
}
'''
TESTS = r'''
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn connected_same_account_is_accepted() -> std::io::Result<()> {
        let (client, server) = std::os::unix::net::UnixStream::pair()?;
        assert!(peer_is_current_user(&net::async_net::UnixStream(server.as_raw_fd())));
        assert!(peer_is_current_user(&net::async_net::UnixStream(client.as_raw_fd())));
        Ok(())
    }

    #[test]
    fn invalid_descriptor_is_rejected() {
        assert!(!peer_is_current_user(&net::async_net::UnixStream(-1)));
    }

    #[test]
    fn non_socket_descriptor_is_rejected() -> std::io::Result<()> {
        let file = std::fs::File::open(std::env::current_exe()?)?;
        assert!(!peer_is_current_user(&net::async_net::UnixStream(file.as_raw_fd())));
        Ok(())
    }
}
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", action="append", default=[])
    arguments = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    source = (root / "crates/remote_server/src/hosted_terminal.rs").read_text()
    start = source.index("fn peer_is_current_user(")
    end = source.index("\nfn ensure_private_directory(", start)
    function = source[start:end]
    lock = tomllib.loads((root / "Cargo.lock").read_text())
    libc_version, = {package["version"] for package in lock["package"]
                     if package["name"] == "libc"}
    with tempfile.TemporaryDirectory(prefix="hosted-terminal-peer-") as temporary:
        directory = Path(temporary)
        (directory / "peer.rs").write_text(PRELUDE + function + TESTS)
        (directory / "Cargo.toml").write_text(
            '[package]\nname = "hosted-terminal-peer-check"\nversion = "0.0.0"\n'
            'edition = "2024"\n[lib]\npath = "peer.rs"\n'
            f'[dependencies]\nlibc = "={libc_version}"\n[workspace]\n'
        )
        commands = [["cargo", "test", "--lib"]]
        commands += [["cargo", "check", "--tests", "--target", target]
                     for target in arguments.target]
        for command in commands:
            target_directory = directory / "target"
            environment = dict(os.environ, CARGO_TARGET_DIR=str(target_directory))
            command += ["--manifest-path", str(directory / "Cargo.toml"), "--offline"]
            print("+ " + " ".join(command), flush=True)
            try:
                subprocess.run(command, env=environment, cwd=root, check=True)
            finally:
                before = shutil.disk_usage(directory).free
                if target_directory.exists():
                    shutil.rmtree(target_directory)
                after = shutil.disk_usage(directory).free
                print(f"Removed {target_directory}; free-space delta {after - before} bytes", flush=True)


if __name__ == "__main__":
    main()
