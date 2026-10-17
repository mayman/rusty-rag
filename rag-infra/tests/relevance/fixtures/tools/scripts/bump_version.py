"""Bumps the version line in Cargo.toml."""

import re
import sys


def bump_version(text, new_version):
    return re.sub(r'^version = ".*"$', f'version = "{new_version}"', text, flags=re.M)


if __name__ == "__main__":
    sys.stdout.write(bump_version(sys.stdin.read(), sys.argv[1]))
