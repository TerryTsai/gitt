#!/usr/bin/env python3
"""Check installation and failure handling against a real packaged executable."""
import argparse
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

parser = argparse.ArgumentParser()
parser.add_argument("--archive", type=Path, required=True)
args = parser.parse_args()
archive = args.archive.resolve()
project = Path(__file__).resolve().parents[1]


class Installer(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="gitt install ")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.downloads = self.root / "downloads"
        self.downloads.mkdir()
        shutil.copy2(archive, self.downloads / archive.name)
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        (self.downloads / "SHA256SUMS").write_text(f"{digest}  {archive.name}\n")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        downloader = self.bin / "curl"
        downloader.write_text(f"#!{sys.executable}\n" + '''import os, pathlib, shutil, sys
url = next(value for value in sys.argv if value.startswith("https://"))
assert url.startswith("https://github.com/TerryTsai/gitt/releases/")
output = sys.argv[sys.argv.index("-o") + 1]
root = pathlib.Path(os.environ["GITT_TEST_DOWNLOADS"])
with (root / "requests").open("a") as log:
    log.write(url + "\\n")
shutil.copyfile(root / url.rsplit("/", 1)[1], output)
''')
        downloader.chmod(0o755)
        self.destination = self.root / "installation with spaces"
        self.env = os.environ | {
            "PATH": str(self.bin) + os.pathsep + os.environ["PATH"],
            "GITT_TEST_DOWNLOADS": str(self.downloads),
            "GITT_INSTALL_DIR": str(self.destination),
            "GITT_VERSION": "latest",
        }

    def install(self):
        return subprocess.run(["sh", str(project / "install.sh")], env=self.env,
                              capture_output=True, text=True)

    def test_install_and_upgrade(self):
        for _ in range(2):
            result = self.install()
            self.assertEqual(result.returncode, 0, result.stderr)
        result = subprocess.check_output([self.destination / "gitt", "--version"], text=True)
        self.assertRegex(result, r"^gitt \d+\.\d+\.\d+")
        self.assertFalse(list(self.destination.glob(".gitt.*")))

    def test_corrupt_download_preserves_existing_install(self):
        self.destination.mkdir()
        installed = self.destination / "gitt"
        installed.write_bytes(b"previous installation")
        (self.downloads / archive.name).write_bytes(b"corrupt download")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Checksum verification failed", result.stderr)
        self.assertEqual(installed.read_bytes(), b"previous installation")

    def test_version_pin(self):
        self.env["GITT_VERSION"] = "0.2.0"
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("/releases/download/v0.2.0/", (self.downloads / "requests").read_text())

    def test_failed_download_leaves_no_installation(self):
        (self.downloads / archive.name).unlink()
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.destination / "gitt").exists())


unittest.main(argv=[sys.argv[0]])
