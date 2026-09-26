import os
import sys


def promote(staged, destination):
    # link is atomic and never replaces an existing directory entry. A failed
    # competitor must verify the winner's bytes before using that executable.
    try:
        os.link(staged, destination, follow_symlinks=False)
    except FileExistsError:
        pass


if __name__ == "__main__":
    promote(sys.argv[1], sys.argv[2])
