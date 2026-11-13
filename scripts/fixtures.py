#!/usr/bin/env python3
"""Create disposable, deterministic Git fixtures. Existing destinations are refused."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import random
import subprocess
import time


class Builder:
    def __init__(self, destination: Path):
        self.destination = destination.resolve()
        self.destination.mkdir(mode=0o700, parents=False, exist_ok=False)
        self.root = self.destination / "repositories"
        self.private = self.destination / "private"
        self.root.mkdir()
        self.private.mkdir()
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        self.env.update(
            GIT_CONFIG_GLOBAL=os.devnull,
            GIT_CONFIG_SYSTEM=os.devnull,
            GIT_CONFIG_NOSYSTEM="1",
            GIT_AUTHOR_NAME="Fixture Author",
            GIT_AUTHOR_EMAIL="fixture@example.invalid",
            GIT_COMMITTER_NAME="Fixture Author",
            GIT_COMMITTER_EMAIL="fixture@example.invalid",
            LC_ALL="C",
        )
        self.tick = 0
        self.names: list[str] = []
        outside_repo = self.private / "outside.git"
        outside_repo.mkdir()
        self.git(outside_repo, "init", "--bare", "--initial-branch=main")

    def git(self, path: Path, *args: str, data: bytes | None = None) -> str:
        stamp = f"{1700000000 + self.tick} +0000"
        env = dict(self.env, GIT_AUTHOR_DATE=stamp, GIT_COMMITTER_DATE=stamp)
        result = subprocess.run(
            ["git", "-c", "protocol.file.allow=always", "-C", str(path), *args],
            input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
        )
        if result.returncode:
            raise RuntimeError(f"git {args!r}: {result.stderr.decode(errors='replace')}")
        return result.stdout.decode("utf-8", "surrogateescape").strip()

    def init(self, name: str, bare: bool = False) -> Path:
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.mkdir()
        self.git(path, "init", "--initial-branch=main", *( ["--bare"] if bare else [] ))
        self.names.append(name)
        return path

    def commit(self, path: Path, message: str) -> str:
        self.tick += 1
        self.git(path, "add", "--all")
        self.git(path, "commit", "-m", message)
        return self.git(path, "rev-parse", "HEAD")

    def clone(self, source: Path, name: str, bare: bool = False, depth: int | None = None) -> Path:
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        args = ["clone", "--quiet", "--no-hardlinks"]
        if bare:
            args.append("--bare")
        if depth:
            args += ["--depth", str(depth)]
        args += [source.as_uri(), str(path)]
        self.git(self.root, *args)
        self.names.append(name)
        return path

    def ordinary(self, count: int) -> dict:
        demo = self.init("demo")
        (demo / "src").mkdir()
        (demo / "docs").mkdir()
        (demo / "README.md").write_text(
            "# Example repository\n\nSmall fixtures for browsing ordinary Git changes.\n\n"
            "- [Source](src/main.rs)\n- [Notes](docs/notes.md)\n\n"
            "```rust\nfn main() { println!(\"Hello, gitt\"); }\n```\n"
        )
        (demo / "docs/notes.md").write_text("# Notes\n\nA **small** project with a [source file](../src/main.rs).\n")
        (demo / "docs/hostile.md").write_text(
            '# Untrusted document\n\n<script>window.BAD = true</script>\n\n'
            '<img src="https://example.invalid/tracker" onerror="alert(1)">\n\n'
            '<div class="owned" id="owned" style="color:red">UNTRUSTED HTML</div>\n\n'
            '[Unsafe link](javascript:alert(1))\n\n![Remote](https://example.invalid/image.png)\n'
        )
        (demo / "hostile.html").write_text('<script>alert("source must be escaped")</script>\n')
        code = 'fn main() {\n    println!("Hello, gitt");\n}\n\n'
        code += "\n".join(f"// Source line {i:03d}: small, deterministic fixture text." for i in range(64)) + "\n"
        (demo / "src/main.rs").write_text(code)
        large_code = "// A 64 KiB source-view fixture.\n"
        number = 0
        while True:
            line = f"pub fn sample_{number:04d}(value: i32) -> i32 {{ value + {number} }}\n"
            if len(large_code) + len(line) > 65536 - 3:
                break
            large_code += line
            number += 1
        large_code += "//" + "x" * (65536 - len(large_code) - 3) + "\n"
        (demo / "src/sample.rs").write_text(large_code)
        for name in ["staged.txt", "unstaged.txt", "both.txt", "deleted-staged.txt", "deleted-worktree.txt"]:
            (demo / name).write_text("original version\n")
        (demo / "tracked-ignored.txt").write_text("tracked files remain visible\n")
        (demo / ".gitignore").write_text("ignored.txt\nignored-dir/\ntracked-ignored.txt\n")
        self.git(demo, "add", "--force", "tracked-ignored.txt")
        (demo / "binary.dat").write_bytes(b"\x00\x01\x02binary\xff\n")
        (demo / "space & question?.txt").write_text("A path needing URL encoding.\n")
        non_utf8 = os.fsencode(demo) + b"/name-\xff.txt"
        with open(non_utf8, "wb") as output:
            output.write(b"Byte-safe filename content.\n")
        (self.private / "secret.txt").write_text("GITT_OUTSIDE_SECRET_SHOULD_NEVER_BE_SERVED\n")
        (demo / "outside-link").symlink_to("../../private/secret.txt")
        (demo / "inside-link").symlink_to("README.md")
        (demo / "outside-directory").symlink_to("../../private", target_is_directory=True)
        (demo / "history.txt").write_text("Revision 0\n")
        root_commit = self.commit(demo, "Initial fixture")
        for number in range(1, 60):
            (demo / "history.txt").write_text(f"Revision {number}\n")
            self.commit(demo, f"Update example {number:02d}")
        self.git(demo, "tag", "v0.1")
        main_before_merge = self.git(demo, "rev-parse", "HEAD")
        self.git(demo, "checkout", "--quiet", "-b", "feature/example", "HEAD~3")
        (demo / "feature.txt").write_text("A branch changes this snapshot.\n")
        feature = self.commit(demo, "Add example feature")
        self.git(demo, "checkout", "--quiet", "main")
        self.tick += 1
        self.git(demo, "merge", "--quiet", "--no-ff", "feature/example", "-m", "Merge example feature")
        merge = self.git(demo, "rev-parse", "HEAD")
        self.git(demo, "gc", "--quiet")
        archive = self.clone(demo, "archive.git", bare=True)
        self.init("team/empty.git", bare=True)
        self.init("empty-checkout")
        self.clone(archive, "shallow", depth=2)
        self.clone(archive, "team/spaced repository")
        while len(self.names) < count:
            number = len(self.names)
            bare = number % 2 == 0
            name = f"collection/repo-{number:03d}" + (".git" if bare else "")
            self.clone(archive, name, bare=bare)

        (demo / "staged.txt").write_text("staged version\n")
        (demo / "both.txt").write_text("index version\n")
        self.git(demo, "add", "staged.txt", "both.txt")
        self.git(demo, "rm", "--quiet", "deleted-staged.txt")
        (demo / "both.txt").write_text("disk version\n")
        (demo / "unstaged.txt").write_text("working version\n")
        (demo / "deleted-worktree.txt").unlink()
        (demo / "untracked.txt").write_text("new untracked content\n")
        (demo / "ignored.txt").write_text("IGNORED_UNTRACKED_SECRET\n")
        (demo / "ignored-dir").mkdir()
        (demo / "ignored-dir/hidden.txt").write_text("IGNORED_DIRECTORY_SECRET\n")
        (demo / "tracked-ignored.txt").write_text("tracked changes remain visible despite ignore\n")
        return {
            "demo": "demo", "bare": "archive.git", "root_commit": root_commit,
            "merge_commit": merge, "feature_commit": feature,
            "before_merge": main_before_merge, "tag": "v0.1",
            "non_utf8_path_hex": b"name-\xff.txt".hex(),
            "commits_reachable_from_main": int(self.git(demo, "rev-list", "--count", "main")),
            "source_bytes": (demo / "src/main.rs").stat().st_size,
            "source_large_bytes": (demo / "src/sample.rs").stat().st_size,
            "status_porcelain_hex": self.git(demo, "status", "--porcelain=v1", "-z").encode("utf-8", "surrogateescape").hex(),
        }

    def security(self) -> list[str]:
        archive = self.root / "archive.git"
        names = []
        hostile = self.clone(archive, "security/filters")
        names.append("security/filters")
        (hostile / ".gitattributes").write_text("*.txt filter=hostile diff=hostile\n")
        self.git(hostile, "config", "filter.hostile.clean", "sh -c 'touch .git/filter-executed; cat'")
        self.git(hostile, "config", "filter.hostile.smudge", "sh -c 'touch .git/filter-executed; cat'")
        self.git(hostile, "config", "diff.hostile.command", "sh -c 'touch .git/diff-executed'")
        self.git(hostile, "config", "core.fsmonitor", "sh -c 'touch .git/fsmonitor-executed'")
        (hostile / "history.txt").write_text("Filtered local change\n")
        for hook in ["post-checkout", "post-index-change", "pre-commit", "post-commit"]:
            target = hostile / ".git/hooks" / hook
            target.write_text("#!/bin/sh\ntouch .git/hook-executed\n")
            target.chmod(0o755)

        linked = self.root / "security/linked"
        linked.mkdir()
        (linked / ".git").write_text("gitdir: ../../../private/not-a-gitdir\n")
        names.append("security/linked")
        alternate = self.init("security/alternates.git", bare=True)
        (alternate / "objects/info/alternates").write_text("../../../../private/objects\n")
        names.append("security/alternates.git")
        (self.root / "outside-repositories").symlink_to("../private", target_is_directory=True)

        conflict = self.clone(archive, "security/conflict")
        names.append("security/conflict")
        self.git(conflict, "checkout", "--quiet", "-b", "conflicting")
        (conflict / "history.txt").write_text("Conflict branch\n")
        self.commit(conflict, "Conflicting branch change")
        self.git(conflict, "checkout", "--quiet", "main")
        (conflict / "history.txt").write_text("Main branch conflict\n")
        self.commit(conflict, "Conflicting main change")
        result = subprocess.run(["git", "-C", str(conflict), "merge", "conflicting"], env=self.env, capture_output=True)
        if result.returncode != 1 or not (conflict / ".git/MERGE_HEAD").exists():
            raise RuntimeError("Expected an unresolved merge in the conflict fixture")
        return names

    def large(self, mebibytes: int) -> dict:
        repo = self.init("large.git", bare=True)
        entries = []
        total = 0
        generator = random.Random(472916)
        for index in range((mebibytes + 63) // 64):
            length = min(64, mebibytes - index * 64) * 1024 * 1024
            process = subprocess.Popen(["git", "-C", str(repo), "hash-object", "-w", "--stdin"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=self.env)
            assert process.stdin and process.stdout
            for _ in range(length // (1024 * 1024)):
                process.stdin.write(generator.randbytes(1024 * 1024))
            process.stdin.close()
            oid = process.stdout.read().decode().strip()
            if process.wait() != 0:
                raise RuntimeError("Could not write large fixture blob")
            entries.append(f"100644 blob {oid}\tblob-{index:03d}.bin\n")
            total += length
        readme = self.git(repo, "hash-object", "-w", "--stdin", data=b"# Large fixture\n\nDeterministic incompressible blobs; no personal data.\n")
        entries.append(f"100644 blob {readme}\tREADME.md\n")
        tree = self.git(repo, "mktree", data="".join(entries).encode())
        commit = self.git(repo, "commit-tree", tree, data=b"Add large fixture\n")
        self.git(repo, "update-ref", "refs/heads/main", commit)
        self.git(repo, "-c", "pack.window=0", "-c", "pack.threads=1", "gc", "--quiet")
        return {"name": "large.git", "blob_bytes": total, "seed": 472916, "largest_blob_bytes": min(mebibytes, 64) * 1024 * 1024, "commit": commit}


def generate(destination: Path, repositories: int = 12, security: bool = False, large_mib: int = 0) -> dict:
    if repositories < 6 + bool(large_mib):
        raise ValueError("At least six small repositories are required")
    builder = Builder(destination)
    fixtures = builder.ordinary(repositories - bool(large_mib))
    large = builder.large(large_mib) if large_mib else None
    security_names = builder.security() if security else []
    metadata_paths = []
    for name in builder.names:
        base = builder.root / name
        gitdir = base / ".git" if (base / ".git").is_dir() else base
        for item in ["config", "index", "HEAD", "packed-refs"]:
            if (gitdir / item).is_file():
                metadata_paths.append(str((gitdir / item).relative_to(builder.root)))
    manifest = {
        "schema": 1, "root": "repositories", "ordinary_repositories": repositories,
        "security_repositories": security_names, "fixtures": fixtures, "large": large,
        "metadata_paths": metadata_paths,
        "git_version": subprocess.check_output(["git", "--version"], text=True).strip(),
        "storage": "Independent object files, packed after history creation. No shared hardlinks.",
        "languages": ["Rust", "Markdown", "plain text"],
        "working_changes": {"staged_paths": 3, "unstaged_paths": 4, "untracked_paths": 1},
    }
    (builder.destination / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return manifest


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path, help="New directory; never overwrites or removes an existing destination")
    parser.add_argument("--repositories", type=int, default=12, help="Ordinary repository count including the optional large repo (default: 12)")
    parser.add_argument("--security", action="store_true", help="Add separate unsupported/conflicting/filter fixtures")
    parser.add_argument("--large-mib", type=int, default=0, help="Add a bare repo of deterministic incompressible blobs (1024 = 1 GiB)")
    args = parser.parse_args()
    if args.large_mib < 0:
        parser.error("--large-mib cannot be negative")
    started = time.monotonic()
    manifest = generate(args.destination, args.repositories, args.security, args.large_mib)
    print(json.dumps({"destination": str(args.destination.resolve()), "elapsed_seconds": round(time.monotonic() - started, 3), "manifest": manifest}, indent=2))


if __name__ == "__main__":
    main()
