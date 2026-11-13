#!/usr/bin/env python3
"""Regenerate the dependency license inventory from the locked Cargo graph."""
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent.parent
metadata = json.loads(subprocess.check_output(
    ["cargo", "metadata", "--locked", "--format-version", "1"], cwd=ROOT
))
sections = ["# Third-party notices\n\n"
            "This inventory covers the locked Cargo dependency graph, including build-time "
            "and platform-specific packages that may not be linked in a Linux release. "
            "Bundled native sources and syntax grammars retain the notices below. "
            "Rust standard-library notices are in `assets/licenses/rust-1.95-standard-library.html`; "
            "linked musl and compiler-support notices are in `assets/licenses/`. "
            "Compiler/build tooling is not distributed.\n"]
missing = []
for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"])):
    if package["name"] == "gitt":
        continue
    directory = Path(package["manifest_path"]).parent
    files = sorted(p for p in directory.rglob("*") if p.is_file() and
                   p.name.lower().startswith(("license", "copying", "copyright", "notice", "unlicense")))
    if package.get("license_file"):
        files = sorted(set(files + [directory / package["license_file"]]))
    sections.append(f"\n## {package['name']} {package['version']}\n\n"
                    f"License expression: {package.get('license') or 'See license file'}\n\n"
                    f"Source: {package.get('repository') or 'https://crates.io/crates/' + package['name']}\n")
    if not files and (directory / "AUTHORS").is_file():
        files = [directory / "AUTHORS"]
    if not files:
        missing.append(package["name"])
    for path in files:
        sections.append(f"\n### {path.relative_to(directory)}\n\n```text\n"
                        + path.read_text(errors="replace") + "\n```\n")
sections.append("\n## Bundled syntax definitions\n\n"
                "Source: https://github.com/sublimehq/Packages/tree/"
                "fa6b8629c95041bf262d4c1dab95c456a0530122\n")
for name in ["LICENSE", "Rust/LICENSE.txt"]:
    path = ROOT / "assets" / "syntaxes" / name
    sections.append(f"\n### {name}\n\n```text\n" + path.read_text() + "\n```\n")
if missing:
    raise SystemExit("Missing license text; review before release: " + ", ".join(missing))
(ROOT / "THIRD_PARTY_NOTICES.md").write_text("".join(sections))
print(f"Wrote notices for {len(metadata['packages']) - 1} dependencies and bundled grammars.")
