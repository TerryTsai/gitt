#!/usr/bin/env python3
"""Create a valid small Git pack whose tiny blob needs a 64 MiB delta base.

The destination must not exist. This is a disposable development fixture; no
existing repository is modified. Python 3 and an installed Git are required.
gitt itself never invokes Git or this generator.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import struct
import subprocess
import zlib


def object_header(kind, size):
    first = (kind << 4) | (size & 15)
    size >>= 4
    out = bytearray([first | (128 if size else 0)])
    while size:
        byte = size & 127
        size >>= 7
        out.append(byte | (128 if size else 0))
    return bytes(out)


def variable_integer(value):
    out = bytearray()
    while True:
        byte = value & 127
        value >>= 7
        out.append(byte | (128 if value else 0))
        if not value:
            return bytes(out)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path)
    args = parser.parse_args()
    destination = args.destination.resolve()
    destination.mkdir()  # Deliberately fail when anything already exists here.
    environment = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    environment.update({
        "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_AUTHOR_NAME": "gitt fixture", "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
        "GIT_COMMITTER_NAME": "gitt fixture", "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
        "GIT_AUTHOR_DATE": "1700000000 +0000", "GIT_COMMITTER_DATE": "1700000000 +0000",
    })

    def git(*arguments, data=None):
        return subprocess.run(
            ["git", "--git-dir", str(destination), *arguments], input=data,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=environment, check=True,
        ).stdout.strip()

    subprocess.run(
        ["git", "init", "--bare", "--template=", "--initial-branch=main", str(destination)],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=environment, check=True,
    )
    size = 64 * 1024 * 1024
    base_hash = hashlib.sha1(b"blob " + str(size).encode() + b"\0")
    pack_hash = hashlib.sha1()
    pack_dir = destination / "objects" / "pack"
    pending = pack_dir / "gitt-fixture.pack.tmp"
    with pending.open("xb") as output:
        def put(data):
            output.write(data)
            pack_hash.update(data)

        put(b"PACK" + struct.pack(">II", 2, 2))
        put(object_header(3, size))  # blob
        compressor = zlib.compressobj(level=6)
        chunk = b"A" * (1024 * 1024)
        for _ in range(size // len(chunk)):
            base_hash.update(chunk)
            put(compressor.compress(chunk))
        put(compressor.flush())
        result = b"small"
        result_id = hashlib.sha1(b"blob 5\0" + result).hexdigest()
        delta = variable_integer(size) + variable_integer(len(result)) + bytes([len(result)]) + result
        put(object_header(7, len(delta)))  # REF_DELTA
        put(base_hash.digest())
        put(zlib.compress(delta))
        output.write(pack_hash.digest())
    pack = pack_dir / ("pack-" + pack_hash.hexdigest() + ".pack")
    pending.rename(pack)
    git("index-pack", str(pack))
    tree = git("mktree", data=(
        "100644 blob " + base_hash.hexdigest() + "\tbase.bin\n"
        "100644 blob " + result_id + "\tsmall.txt\n"
    ).encode())
    commit = git("commit-tree", tree.decode(), data=b"Exercise a bounded delta-base read\n")
    git("update-ref", "refs/heads/main", commit.decode())
    git("fsck", "--full", "--strict")
    if git("cat-file", "blob", result_id) != result:
        raise RuntimeError("Git could not reconstruct the small delta result")
    print(json.dumps({
        "repository": str(destination), "commit": commit.decode(), "result_blob": result_id,
        "base_blob": base_hash.hexdigest(), "base_bytes": size, "result_bytes": len(result),
        "pack_bytes": pack.stat().st_size, "path": "small.txt",
        "expected_gitt_status": 422,
        "reason": "The result is small but reconstructing its delta requires a base larger than the 24 MiB native allocation budget.",
    }, indent=2))


if __name__ == "__main__":
    main()
