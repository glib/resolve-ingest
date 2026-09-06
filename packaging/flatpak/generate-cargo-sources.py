#!/usr/bin/env python3
"""Generate flatpak-builder Cargo sources from the locked Rust dependency set.

This is intentionally stdlib-only so it can be run inside the Fedora
Distrobox without installing a second Python dependency stack. The output
format matches flatpak-cargo-generator: every crate is downloaded by
flatpak-builder, checked against Cargo.lock, and placed in a vendored Cargo
source directory.
"""

from __future__ import annotations

import argparse
import json
import tomllib
from pathlib import Path


def generate(lock_path: Path) -> list[dict[str, object]]:
    lock = tomllib.loads(lock_path.read_text(encoding="utf-8"))
    sources: list[dict[str, object]] = []
    for package in lock.get("package", []):
        source = package.get("source", "")
        checksum = package.get("checksum")
        if not source:
            # The root package is the only path package in this repository.
            continue
        if not source.startswith("registry+") or not checksum:
            raise SystemExit(
                f"Unsupported Cargo.lock source for {package['name']} "
                f"{package['version']}: {source}"
            )
        name = package["name"]
        version = package["version"]
        vendor_dir = f"cargo/vendor/{name}-{version}"
        sources.extend(
            [
                {
                    "type": "archive",
                    "archive-type": "tar-gzip",
                    "url": f"https://static.crates.io/crates/{name}/{name}-{version}.crate",
                    "sha256": checksum,
                    "dest": vendor_dir,
                },
                {
                    "type": "inline",
                    "contents": json.dumps({"package": checksum, "files": {}}),
                    "dest": vendor_dir,
                    "dest-filename": ".cargo-checksum.json",
                },
            ]
        )

    sources.append(
        {
            "type": "inline",
            "contents": (
                "[source.crates-io]\n"
                'replace-with = "vendored-sources"\n\n'
                "[source.vendored-sources]\n"
                'directory = "cargo/vendor"\n'
            ),
            "dest": "cargo",
            "dest-filename": "config",
        }
    )
    return sources


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("lock", type=Path, nargs="?", default=Path("Cargo.lock"))
    parser.add_argument(
        "-o",
        "--output",
        type=Path,
        default=Path("packaging/flatpak/cargo-sources.json"),
    )
    args = parser.parse_args()
    args.output.write_text(
        json.dumps(generate(args.lock), indent=4) + "\n", encoding="utf-8"
    )


if __name__ == "__main__":
    main()
