#!/usr/bin/env python3
"""Flatten a UCI fixture into shell-friendly files.

config.sh reads UCI through config_load / config_get / config_list_foreach.
run.sh provides tiny implementations of those helpers on top of this script:

    <outdir>/<section>/<option>        value of an `option` line
    <outdir>/<section>/<option>.list   one value per line for `list` lines

The fixture is trusted test input, but it is still parsed with shlex and never
evaluated as shell.
"""

import os
import shlex
import sys


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: harness.py <fixture.uci> <outdir>", file=sys.stderr)
        return 2

    fixture, outdir = sys.argv[1], sys.argv[2]
    section = "config"
    os.makedirs(outdir, exist_ok=True)

    with open(fixture, encoding="utf-8") as fh:
        for lineno, raw in enumerate(fh, 1):
            line = raw.split("#", 1)[0].strip()
            if not line:
                continue
            try:
                parts = shlex.split(line)
            except ValueError as exc:
                print(f"{fixture}:{lineno}: {exc}", file=sys.stderr)
                return 2
            if not parts:
                continue

            kind = parts[0]
            if kind == "config":
                # config <type> '<name>'
                if len(parts) >= 3:
                    section = parts[2]
                elif len(parts) == 2:
                    section = parts[1]
                else:
                    print(f"{fixture}:{lineno}: malformed config line", file=sys.stderr)
                    return 2
                os.makedirs(os.path.join(outdir, section), exist_ok=True)
            elif kind in ("option", "list"):
                if len(parts) < 2:
                    print(f"{fixture}:{lineno}: {kind} without name", file=sys.stderr)
                    return 2
                name = parts[1]
                value = parts[2] if len(parts) > 2 else ""
                secdir = os.path.join(outdir, section)
                os.makedirs(secdir, exist_ok=True)
                if kind == "option":
                    with open(os.path.join(secdir, name), "w", encoding="utf-8") as out:
                        out.write(value + "\n")
                else:
                    with open(os.path.join(secdir, name + ".list"), "a", encoding="utf-8") as out:
                        out.write(value + "\n")
            else:
                print(f"{fixture}:{lineno}: unknown directive {kind!r}", file=sys.stderr)
                return 2

    return 0


if __name__ == "__main__":
    sys.exit(main())
