import os
import sys


def project_listener_ports(roots, candidates, proc="/proc"):
    roots = [os.path.realpath(root) for root in roots]
    inodes = {}
    for table in ("tcp", "tcp6"):
        try:
            with open(os.path.join(proc, "net", table)) as stream:
                for line in stream:
                    fields = line.split()
                    if len(fields) < 10 or fields[3] != "0A":
                        continue
                    port = int(fields[1].rsplit(":", 1)[1], 16)
                    if port in candidates:
                        inodes.setdefault(fields[9], set()).add(port)
        except FileNotFoundError:
            continue

    result = set()
    for process in os.scandir(proc):
        if not process.name.isdecimal():
            continue
        try:
            cwd = os.path.realpath(os.readlink(os.path.join(process.path, "cwd")))
            if not any(os.path.commonpath([cwd, root]) == root for root in roots):
                continue
            for descriptor in os.scandir(os.path.join(process.path, "fd")):
                try:
                    target = os.readlink(descriptor.path)
                except OSError:
                    continue
                if target.startswith("socket:[") and target.endswith("]"):
                    result.update(inodes.get(target[8:-1], ()))
        except (OSError, ValueError):
            # Processes can exit during the snapshot; unreadable owners are not trusted.
            continue
    return sorted(result)


if __name__ == "__main__":
    if not sys.platform.startswith("linux"):
        sys.exit("自动转发归属校验目前仅支持 Linux；请手动添加端口")
    candidates = {int(port) for port in sys.argv[1].split(",")}
    for port in project_listener_ports(sys.argv[2:], candidates):
        print(port)
