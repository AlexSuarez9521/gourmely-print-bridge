#!/usr/bin/env python3
"""Bump (or check) the bridge version across the three files that carry it.

Why a script instead of inline shell in the workflow: the renewal workflow used
to do this with inline python plus an awk pass, and it only touched two of the
three files — which is how `Cargo.lock` ended up pinned at 0.1.1 while both
manifests said 0.1.2. Inline heredocs inside a YAML block scalar are also easy
to break in ways nothing catches until the workflow runs months later (a
terminator at the wrong indentation silently ends the YAML block).

Here it is one place, runnable locally, with a --check mode the CI uses so the
three files can never drift apart again.

    python3 ops/bump-version.py --check     # all three agree? (exit 1 if not)
    python3 ops/bump-version.py --patch     # x.y.z -> x.y.(z+1), prints the new
    python3 ops/bump-version.py --set 0.2.0 # explicit
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CONF = ROOT / "src-tauri" / "tauri.conf.json"
CARGO = ROOT / "src-tauri" / "Cargo.toml"
LOCK = ROOT / "src-tauri" / "Cargo.lock"

SEMVER = re.compile(r"^\d+\.\d+\.\d+$")


def read_conf() -> str:
    return json.loads(CONF.read_text(encoding="utf-8"))["version"]


def write_conf(new: str) -> None:
    """Replace just the version value, leaving every other byte alone.

    Re-serialising the whole file with `json.dumps` looks tidier and is a trap:
    `ensure_ascii` defaults to True, so it rewrote the installer's
    shortDescription and copyright as `\\u2014` / `\\u00a9` escapes. Same JSON,
    but a gratuitous diff on every renewal — in the file the MSI bundler reads.
    """
    text = CONF.read_text(encoding="utf-8")
    match = re.search(r'^(\s*"version"\s*:\s*")([^"]+)(")', text, re.M)
    if not match:
        raise SystemExit('no top-level "version" key in tauri.conf.json')
    CONF.write_text(
        text[: match.start(2)] + new + text[match.end(2) :], encoding="utf-8", newline="\n"
    )


def _cargo_package_version_span(text: str) -> tuple[int, int]:
    """Span of the version value inside the [package] table only.

    Dependencies further down also have `version = "..."` lines, so a plain
    search would rewrite the wrong one.
    """
    start = text.index("[package]")
    # End of the [package] table: the next top-level `[` or EOF.
    nxt = re.search(r"^\[", text[start + len("[package]") :], re.M)
    end = start + len("[package]") + (nxt.start() if nxt else len(text))
    match = re.search(r'^version\s*=\s*"([^"]+)"', text[start:end], re.M)
    if not match:
        raise SystemExit("no `version` line inside [package] in Cargo.toml")
    return start + match.start(1), start + match.end(1)


def read_cargo() -> str:
    text = CARGO.read_text(encoding="utf-8")
    lo, hi = _cargo_package_version_span(text)
    return text[lo:hi]


def write_cargo(new: str) -> None:
    text = CARGO.read_text(encoding="utf-8")
    lo, hi = _cargo_package_version_span(text)
    CARGO.write_text(text[:lo] + new + text[hi:], encoding="utf-8", newline="\n")


LOCK_MARKER = 'name = "print-bridge"\nversion = "'


def _lock_span(text: str) -> tuple[int, int]:
    idx = text.find(LOCK_MARKER)
    if idx == -1:
        raise SystemExit("no `print-bridge` package entry found in Cargo.lock")
    lo = idx + len(LOCK_MARKER)
    return lo, text.index('"', lo)


def read_lock() -> str:
    text = LOCK.read_text(encoding="utf-8")
    lo, hi = _lock_span(text)
    return text[lo:hi]


def write_lock(new: str) -> None:
    text = LOCK.read_text(encoding="utf-8")
    lo, hi = _lock_span(text)
    LOCK.write_text(text[:lo] + new + text[hi:], encoding="utf-8", newline="\n")


def current() -> dict[str, str]:
    return {
        "tauri.conf.json": read_conf(),
        "Cargo.toml": read_cargo(),
        "Cargo.lock": read_lock(),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--check", action="store_true", help="verify the three agree")
    group.add_argument("--patch", action="store_true", help="bump the patch component")
    group.add_argument("--set", metavar="X.Y.Z", help="set an explicit version")
    args = parser.parse_args()

    versions = current()

    if args.check:
        distinct = set(versions.values())
        for name, value in versions.items():
            print(f"{name}: {value}")
        if len(distinct) != 1:
            print(
                "\nERROR: these must all carry the same version. "
                "Run `python3 ops/bump-version.py --set <version>` to line them up.",
                file=sys.stderr,
            )
            return 1
        print("\nall three agree")
        return 0

    old = versions["tauri.conf.json"]
    if not SEMVER.match(old):
        raise SystemExit(f"version in tauri.conf.json is not x.y.z: {old!r}")

    if args.patch:
        major, minor, patch = old.split(".")
        new = f"{major}.{minor}.{int(patch) + 1}"
    else:
        new = args.set
        if not SEMVER.match(new):
            raise SystemExit(f"--set expects x.y.z, got {new!r}")

    write_conf(new)
    write_cargo(new)
    write_lock(new)

    # stdout is the contract with the workflow, which captures it.
    print(new)
    print(f"bumped {old} -> {new} in all three files", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
