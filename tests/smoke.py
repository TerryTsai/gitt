#!/usr/bin/env python3
"""Exercise a running gitt with disposable fixtures; no third-party Python modules.

Example: python3 tests/smoke.py --base-url http://127.0.0.1:18080 \
  --fixtures /tmp/gitt-fixtures --ssh-host gitbox \
  --remote-root /tmp/gitt-test/repositories --origin http://127.0.0.1:18080

The target must be the supplied disposable fixture root: these tests create repos.
"""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
from html.parser import HTMLParser
import http.cookiejar
import json
from pathlib import Path
import re
import shlex
import subprocess
import sys
import unittest
import urllib.error
import urllib.parse
import urllib.request
import uuid


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class Document(HTMLParser):
    def __init__(self, body: bytes):
        super().__init__(convert_charrefs=True)
        self.stack: list[str] = []
        self.problems: list[str] = []
        self.links: list[str] = []
        self.anchors: list[dict] = []
        self.open_anchors: list[int] = []
        self.navigation: list[str] = []
        self.inputs: dict[str, str] = {}
        self.tags: list[str] = []
        self.ids: list[str] = []
        self.highlighted_spans = 0
        self.text: list[str] = []
        self.viewport = False
        self.feed(body.decode("utf-8"))

    def handle_starttag(self, tag, attributes):
        attrs = dict(attributes)
        self.tags.append(tag)
        if tag in {"script", "iframe", "embed", "object", "base"}:
            self.problems.append(f"forbidden {tag} element")
        if tag in {"details", "summary"} and "header" in self.stack and "Breadcrumb" not in self.navigation:
            self.problems.append("unexpected header navigation disclosure")
        for name, value in attributes:
            if name.startswith("on"):
                self.problems.append(f"event handler {name}")
            if name == "style":
                self.problems.append("inline style")
            if name == "class" and (tag != "span" or not ("pre" in self.stack and "code" in self.stack)):
                self.problems.append(f"class on {tag} outside source highlighting")
            elif name == "class":
                self.highlighted_spans += 1
            if name == "id":
                if value in self.ids:
                    self.problems.append(f"duplicate ID {value}")
                self.ids.append(value or "")
                if tag != "a" or "pre" not in self.stack or "code" not in self.stack or not re.fullmatch(r"L[1-9]\d*", value or ""):
                    self.problems.append(f"non-line ID {value}")
            if name in {"href", "src", "action"} and value and value.strip().lower().startswith("javascript:"):
                self.problems.append("javascript URL")
        if tag in {"img", "iframe", "script", "video", "audio", "source"} and attrs.get("src", "").startswith(("http:", "https:", "//")):
            self.problems.append("automatic remote resource")
        if tag == "link" and attrs.get("href", "").startswith(("http:", "https:", "//")):
            self.problems.append("external stylesheet or resource")
        if tag == "meta" and attrs.get("name") == "viewport":
            self.viewport = "width=device-width" in attrs.get("content", "")
        if tag == "a" and "href" in attrs:
            self.links.append(attrs["href"])
            self.open_anchors.append(len(self.anchors))
            self.anchors.append({"href": attrs["href"], "text": "", "navigation": self.navigation[-1] if self.navigation else None, "current": attrs.get("aria-current")})
        if tag == "nav":
            self.navigation.append(attrs.get("aria-label", ""))
        if tag == "input" and "name" in attrs:
            self.inputs[attrs["name"]] = attrs.get("value", "")
        if tag not in {"area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"}:
            self.stack.append(tag)

    def handle_endtag(self, tag):
        if tag == "a" and self.open_anchors:
            self.open_anchors.pop()
        if tag == "nav" and self.navigation:
            self.navigation.pop()
        if tag in self.stack:
            index = len(self.stack) - 1 - self.stack[::-1].index(tag)
            del self.stack[index:]

    def handle_data(self, value):
        self.text.append(value)
        for index in self.open_anchors:
            self.anchors[index]["text"] += value


class Client:
    def __init__(self, base_url: str, origin: str):
        self.base_url = base_url.rstrip("/")
        self.origin = origin.rstrip("/")
        self.cookies = http.cookiejar.CookieJar()
        self.opener = urllib.request.build_opener(NoRedirect, urllib.request.HTTPCookieProcessor(self.cookies))

    def request(self, path: str, params=None, form=None, headers=None, method=None):
        url = self.base_url + path
        if params:
            url += "?" + urllib.parse.urlencode(params)
        request_headers = {"Host": urllib.parse.urlsplit(self.origin).netloc}
        data = None
        if form is not None:
            data = urllib.parse.urlencode(form).encode()
            request_headers.update({"Content-Type": "application/x-www-form-urlencoded", "Origin": self.origin})
        request_headers.update(headers or {})
        request = urllib.request.Request(url, data=data, headers=request_headers, method=method)
        try:
            response = self.opener.open(request, timeout=20)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            return response.code, response.headers, response.read(3 * 1024 * 1024)


class Inspector:
    """Read only files from the fixture root, locally or through an existing SSH account."""

    def __init__(self, root: Path, ssh_host: str | None):
        self.root = root
        self.ssh_host = ssh_host

    def read(self, relative: str) -> bytes:
        path = self.root / relative
        if self.ssh_host:
            result = subprocess.run(["ssh", "-oBatchMode=yes", self.ssh_host, "cat " + shlex.quote(str(path))], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            if result.returncode:
                raise FileNotFoundError(relative)
            return result.stdout
        return path.read_bytes()

    def exists(self, relative: str) -> bool:
        path = self.root / relative
        if self.ssh_host:
            result = subprocess.run(["ssh", "-oBatchMode=yes", self.ssh_host, "test -e " + shlex.quote(str(path))], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            return result.returncode == 0
        return path.exists()

    def fingerprints(self, paths: list[str]) -> dict[str, str]:
        if self.ssh_host:
            # One SSH round trip for all metadata; filenames originate in our manifest.
            arguments = " ".join(shlex.quote(str(self.root / path)) for path in paths)
            result = subprocess.run(["ssh", "-oBatchMode=yes", self.ssh_host, "sha256sum " + arguments], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            lines = result.stdout.decode().splitlines()
            if len(lines) != len(paths):
                raise RuntimeError("Unexpected remote fingerprint output")
            return {path: line.split()[0] for path, line in zip(paths, lines)}
        return {path: hashlib.sha256((self.root / path).read_bytes()).hexdigest() for path in paths}

    def permissions(self, paths: list[str]) -> dict[str, str]:
        if self.ssh_host:
            arguments = " ".join(shlex.quote(str(self.root / path)) for path in paths)
            command = "stat -c '%a:%u:%g' " + arguments
            result = subprocess.run(["ssh", "-oBatchMode=yes", self.ssh_host, command], check=True, capture_output=True, text=True)
            values = result.stdout.splitlines()
            if len(values) != len(paths):
                raise RuntimeError("Unexpected remote permission output")
            return dict(zip(paths, values))
        result = {}
        for path in paths:
            metadata = (self.root / path).stat()
            result[path] = f"{metadata.st_mode & 0o7777:o}:{metadata.st_uid}:{metadata.st_gid}"
        return result


ARGS = None
MANIFEST = None


class GittSmoke(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.client = Client(ARGS.base_url, ARGS.origin or ARGS.base_url)
        cls.created_prefix = "created-by-smoke-" + uuid.uuid4().hex[:12]
        if ARGS.http_only:
            cls.inspector = None
            return
        local_root = ARGS.fixtures / MANIFEST["root"]
        cls.inspector = Inspector(Path(ARGS.remote_root) if ARGS.remote_root else local_root, ARGS.ssh_host)
        cls.metadata_paths = list(MANIFEST["metadata_paths"])
        if cls.inspector.exists("delta-bomb.git/config"):
            cls.metadata_paths.extend(["delta-bomb.git/config", "delta-bomb.git/HEAD"])
        cls.before = cls.inspector.fingerprints(cls.metadata_paths)
        existing_paths = set(cls.metadata_paths)
        for path in cls.metadata_paths:
            existing_paths.update(str(parent) for parent in Path(path).parents)
        cls.permission_paths = sorted(existing_paths)
        cls.permissions_before = cls.inspector.permissions(cls.permission_paths)

    @classmethod
    def tearDownClass(cls):
        if ARGS.http_only:
            print("HTTP-only run: filesystem preservation, new metadata permissions, and hook marker checks were not performed.")
            print(f"Created fixtures retained under {cls.created_prefix}; no cleanup was performed.")
            return
        after = cls.inspector.fingerprints(cls.metadata_paths)
        if cls.before != after:
            changed = [name for name, digest in cls.before.items() if after.get(name) != digest]
            raise AssertionError(f"Read/create operations changed existing repository metadata: {changed}")
        if cls.permissions_before != cls.inspector.permissions(cls.permission_paths):
            raise AssertionError("Read/create operations changed existing ownership or permissions")
        print(f"Created fixtures retained under {cls.created_prefix}; no cleanup was performed.")

    def page(self, path, params=None, expected=200):
        status, headers, body = self.client.request(path, params)
        self.assertEqual(status, expected, body[:500].decode(errors="replace"))
        self.assertIn("text/html", headers.get("Content-Type", ""))
        self.assertEqual(headers.get("X-Content-Type-Options"), "nosniff")
        self.assertIn("script-src 'none'", headers.get("Content-Security-Policy", ""))
        document = Document(body)
        self.assertFalse(document.problems, document.problems)
        self.assertTrue(document.viewport, "Missing mobile viewport")
        self.assertIn("main", document.tags)
        return document, body

    def test_01_repository_listing_and_name_search(self):
        document, body = self.page("/")
        visible = " ".join(document.text)
        self.assertIn("demo", visible)
        self.assertIn("archive.git", visible)
        self.assertIn("team/empty.git", visible)
        self.assertNotIn("GITT_OUTSIDE_SECRET", visible)
        self.assertFalse(any("outside-repositories" in url for url in document.links))
        filtered, _ = self.page("/search", {"q": "archive"})
        repository_links = [url for url in filtered.links if urllib.parse.urlsplit(url).path == "/files"]
        self.assertTrue(any("archive" in url for url in repository_links))
        self.assertFalse(any(urllib.parse.parse_qs(urllib.parse.urlsplit(url).query).get("repo") == ["demo"] for url in repository_links))

    def test_02_checkout_and_committed_files(self):
        document, _ = self.page("/files", {"repo": "demo"})
        visible = " ".join(document.text)
        self.assertNotIn("untracked.txt", visible)
        self.assertIn("tracked-ignored.txt", visible)
        self.assertNotIn("ignored.txt", [urllib.parse.parse_qs(urllib.parse.urlsplit(url).query).get("path", [""])[0] for url in document.links])
        self.assertNotIn("ignored-dir", visible)
        for repo, rev in [("demo", ""), ("archive.git", "main")]:
            source, _ = self.page("/files", {"repo": repo, "rev": rev, "path": "src/main.rs"})
            self.assertIn("Hello, gitt", " ".join(source.text))
            self.assertTrue(source.ids, "Source lines need linkable anchors")
            self.assertGreater(source.highlighted_spans, 0, "Small Rust source should be highlighted on the server")
        source, _ = self.page("/files", {"repo": "demo", "path": "both.txt"})
        self.assertIn("original version", " ".join(source.text))
        self.assertNotIn("disk version", " ".join(source.text))
        committed, _ = self.page("/files", {"repo": "demo", "path": "both.txt", "rev": "HEAD"})
        self.assertIn("original version", " ".join(committed.text))
        filtered, _ = self.page("/search", {"repo": "demo", "q": "main.rs"})
        self.assertTrue(any(urllib.parse.parse_qs(urllib.parse.urlsplit(url).query).get("path") == ["src/main.rs"] for url in filtered.links))

    def test_03_byte_safe_paths_and_raw_attachment(self):
        path = bytes.fromhex(MANIFEST["fixtures"]["non_utf8_path_hex"])
        document, _ = self.page("/files", {"repo": "demo", "path": path})
        self.assertIn("Byte-safe filename", " ".join(document.text))
        document, _ = self.page("/files", {"repo": "demo", "path": "space & question?.txt"})
        self.assertIn("URL encoding", " ".join(document.text))
        for rev in ["", "HEAD"]:
            status, headers, body = self.client.request("/raw", {"repo": "demo", "path": "hostile.html", "rev": rev})
            self.assertEqual(status, 200)
            self.assertEqual(headers.get_content_type(), "application/octet-stream")
            self.assertIn("attachment", headers.get("Content-Disposition", ""))
            self.assertEqual(headers.get("X-Content-Type-Options"), "nosniff")
            self.assertEqual(body, b'<script>alert("source must be escaped")</script>\n')

    def test_04_safe_markdown_and_source(self):
        rendered, _ = self.page("/files", {"repo": "demo", "path": "docs/hostile.md"})
        self.assertNotIn("script", rendered.tags)
        self.assertNotIn("owned", rendered.ids)
        source, body = self.page("/files", {"repo": "demo", "path": "hostile.html"})
        self.assertIn("source must be escaped", " ".join(source.text))
        self.assertNotIn(b'<script>alert(', body)
        markdown_source, _ = self.page("/files", {"repo": "demo", "path": "README.md", "source": "1"})
        self.assertIn("# Example repository", "".join(markdown_source.text))

    def test_05_paths_symlinks_and_ignored_files(self):
        for route, params in [
            ("/files", {"repo": "../private"}),
            ("/files", {"repo": "demo", "path": "../../private/secret.txt"}),
            ("/raw", {"repo": "demo", "path": ".git/config"}),
            ("/files", {"repo": "demo", "path": "outside-directory/secret.txt"}),
            ("/files", {"repo": "demo", "path": "ignored.txt"}),
        ]:
            with self.subTest(route=route, params=params):
                status, _, body = self.client.request(route, params)
                self.assertIn(status, [400, 403, 404, 422])
                self.assertNotIn(b"GITT_OUTSIDE_SECRET", body)
                self.assertNotIn(b"IGNORED_UNTRACKED_SECRET", body)
        status, _, body = self.client.request("/files", {"repo": "demo", "path": "outside-link"})
        self.assertIn(status, [200, 422])
        self.assertNotIn(b"GITT_OUTSIDE_SECRET", body)
        status, _, body = self.client.request("/raw", {"repo": "demo", "path": "outside-link"})
        self.assertIn(status, [400, 403, 404, 422])
        self.assertNotIn(b"GITT_OUTSIDE_SECRET", body)

    def test_06_committed_history_branches_and_tags(self):
        self.page("/changes", {"repo": "demo"}, expected=404)
        history, _ = self.page("/history", {"repo": "demo"})
        commit_links = {anchor["href"] for anchor in history.anchors if not anchor["navigation"] and urllib.parse.urlsplit(anchor["href"]).path == "/history"}
        self.assertEqual(len(commit_links), 50)
        self.assertTrue(any(urllib.parse.parse_qs(urllib.parse.urlsplit(url).query).get("page") == ["1"] for url in history.links))
        older, _ = self.page("/history", {"repo": "demo", "page": "1"})
        self.assertIn("Initial fixture", " ".join(older.text))
        branches, _ = self.page("/branches", {"repo": "demo"})
        tags, _ = self.page("/tags", {"repo": "demo"})
        def snapshots(document):
            result = {}
            for anchor in document.anchors:
                target = urllib.parse.urlsplit(anchor["href"])
                revision = urllib.parse.parse_qs(target.query).get("rev", [""])[0]
                if not anchor["navigation"] and target.path in ["/branches", "/tags"] and revision.startswith("refs/"):
                    result[anchor["text"].strip()] = revision
            return result
        self.assertEqual(snapshots(branches), {"main": "refs/heads/main", "feature/example": "refs/heads/feature/example"})
        self.assertEqual(snapshots(tags), {"v0.1": "refs/tags/v0.1"})

    def test_07_removed_commit_and_compare_views(self):
        for route in ["/commit", "/compare"]:
            self.page(route, {"repo": "demo"}, expected=404)

    def test_08_empty_missing_binary_and_shallow(self):
        for repo in ["team/empty.git", "empty-checkout"]:
            document, _ = self.page("/files", {"repo": repo})
            self.assertRegex(" ".join(document.text).lower(), r"empty|no commits|unborn")
        self.page("/files", {"repo": "absent-repository"}, expected=404)
        binary, _ = self.page("/files", {"repo": "demo", "path": "binary.dat"})
        self.assertIn("binary", " ".join(binary.text).lower())
        shallow, body = self.page("/history", {"repo": "shallow"})
        self.assertIn(b"shallow", body.lower())

    def test_09_host_and_creation_csrf(self):
        status, _, _ = self.client.request("/", headers={"Host": "attacker.invalid"})
        self.assertIn(status, [400, 403])
        fields = {"path": self.created_prefix + "/csrf-refused", "bare": "1", "branch": "main"}
        status, _, _ = self.client.request("/new", form=fields)
        self.assertEqual(status, 403)
        page, _ = self.page("/new")
        self.assertIn("csrf", page.inputs)
        fields["csrf"] = page.inputs["csrf"]
        status, _, _ = self.client.request("/new", form=fields, headers={"Origin": "http://attacker.invalid"})
        self.assertEqual(status, 403)
        status, _, _ = self.client.request("/new", form=dict(fields, csrf="invalid"))
        self.assertEqual(status, 403)
        status, _, _ = self.client.request("/new", form=fields, headers={"Origin": ""})
        self.assertEqual(status, 403)
        if self.inspector:
            self.assertFalse(self.inspector.exists(self.created_prefix + "/csrf-refused"))
        else:
            self.page("/files", {"repo": self.created_prefix + "/csrf-refused"}, expected=404)

    def create(self, path, kind="bare", branch="main"):
        form, _ = self.page("/new")
        fields = {"path": path, "branch": branch, "csrf": form.inputs["csrf"]}
        if kind == "bare":
            fields["bare"] = "1"
        return self.client.request("/new", form=fields)

    def test_10_create_bare_checkout_and_no_overwrite(self):
        for name, kind, branch in [("bare.git", "bare", "main"), ("checkout with spaces", "checkout", "trunk")]:
            relative = self.created_prefix + "/" + name
            status, headers, body = self.create(relative, kind, branch)
            self.assertEqual(status, 303, body[:500])
            self.assertEqual(urllib.parse.urlsplit(headers["Location"]).path, "/files")
            metadata = relative + ("/.git" if kind == "checkout" else "")
            if self.inspector:
                self.assertEqual(self.inspector.read(metadata + "/HEAD"), f"ref: refs/heads/{branch}\n".encode())
                config = self.inspector.read(metadata + "/config").decode()
                self.assertRegex(config, r"bare\s*=\s*" + ("true" if kind == "bare" else "false"))
                self.assertNotIn("updateInstead", config)
                self.assertNotIn("denyCurrentBranch", config)
                checked = [relative, metadata, metadata + "/HEAD", metadata + "/config"]
                for item, permissions in self.inspector.permissions(checked).items():
                    self.assertTrue(int(permissions.split(":")[0], 8) & 0o020, f"Shared group cannot write {item}")
            else:
                created, _ = self.page("/files", {"repo": relative})
                self.assertRegex(" ".join(created.text).lower(), r"empty|no commits|unborn")
            status, _, _ = self.create(relative, kind, branch)
            self.assertEqual(status, 409)
        if self.inspector:
            self.assertFalse(self.inspector.exists(self.created_prefix + "/checkout with spaces/README.md"))
        else:
            self.page("/files", {"repo": self.created_prefix + "/checkout with spaces", "path": "README.md"}, expected=404)

    def test_11_invalid_creation_and_concurrent_collision(self):
        for path in ["../escape", "/absolute", "demo/nested", "team/.git/private", "one/../two", "contains\nnewline"]:
            with self.subTest(path=path):
                status, _, body = self.create(path)
                self.assertIn(status, [400, 403, 409, 422], body[:500])
        path = self.created_prefix + "/race.git"
        # Each client has its own valid cookie/form pair before the concurrent POST.
        clients = [Client(ARGS.base_url, ARGS.origin or ARGS.base_url) for _ in range(2)]
        tokens = [Document(client.request("/new")[2]).inputs["csrf"] for client in clients]
        def attempt(pair):
            client, token = pair
            return client.request("/new", form={"path": path, "bare": "1", "branch": "main", "csrf": token})[0]
        with ThreadPoolExecutor(max_workers=2) as pool:
            statuses = sorted(pool.map(attempt, zip(clients, tokens)))
        self.assertEqual(statuses, [303, 409])

    def test_12_unsupported_layouts_conflicts_and_filters(self):
        if not MANIFEST.get("security_repositories"):
            self.skipTest("Fixtures were generated without --security")
        for repo in ["security/linked", "security/alternates.git"]:
            status, _, body = self.client.request("/files", {"repo": repo})
            self.assertEqual(status, 422, body[:500])
            self.assertNotIn(b"GITT_OUTSIDE_SECRET", body)
        self.page("/files", {"repo": "security/filters"})
        self.page("/changes", {"repo": "security/filters"}, expected=404)
        for marker in ["filter-executed", "diff-executed", "fsmonitor-executed", "hook-executed"]:
            if self.inspector:
                self.assertFalse(self.inspector.exists("security/filters/.git/" + marker), marker)
        self.page("/files", {"repo": "security/conflict"})
        self.page("/changes", {"repo": "security/conflict"}, expected=404)

    def test_13_malformed_and_oversized_requests(self):
        status, _, _ = self.client.request("/files?repo=demo&repo=archive.git")
        self.assertEqual(status, 400)
        status, _, _ = self.client.request("/files?repo=%ZZ")
        self.assertEqual(status, 400)
        status, _, _ = self.client.request("/new", form={"path": "x" * 17000})
        self.assertEqual(status, 413)

    def test_14_large_object_limits(self):
        if not MANIFEST.get("large"):
            self.skipTest("No optional large fixture")
        for route in ["/files", "/raw"]:
            status, _, body = self.client.request(route, {"repo": "large.git", "path": "blob-000.bin", "rev": "main"})
            self.assertEqual(status, 422, body[:500])
        self.page("/files", {"repo": "large.git", "path": "README.md", "rev": "main"})

    def test_15_native_delta_allocation_bound(self):
        if (self.inspector and not self.inspector.exists("delta-bomb.git/config")) or (not self.inspector and not ARGS.native_delta):
            self.skipTest("No targeted native-allocation fixture")
        status, _, body = self.client.request("/raw", {"repo": "delta-bomb.git", "path": "small.txt", "rev": "HEAD"})
        self.assertEqual(status, 422, body[:500])
        self.assertRegex(body.lower(), rb"limit|budget|memory")
        self.page("/")
        self.page("/files", {"repo": "archive.git", "path": "src/main.rs"})

    def test_16_no_update_or_delete_routes(self):
        for method, route in [("DELETE", "/files?repo=demo"), ("PUT", "/files?repo=demo&path=README.md"), ("POST", "/changes?repo=demo")]:
            with self.subTest(method=method, route=route):
                status, _, _ = self.client.request(route, method=method)
                self.assertEqual(status, 405)

    def test_17_compact_navigation_and_pinned_snapshot(self):
        home, _ = self.page("/")
        create = [anchor for anchor in home.anchors if urllib.parse.urlsplit(anchor["href"]).path == "/new"]
        self.assertEqual([anchor["text"].strip() for anchor in create], ["Create"])
        snapshot = MANIFEST["fixtures"]["feature_commit"]
        source, _ = self.page("/files", {"repo": "archive.git", "path": "src/main.rs", "rev": snapshot})
        navigation = {anchor["text"].strip(): anchor for anchor in source.anchors if anchor["navigation"] == "Repository"}
        self.assertEqual(set(navigation), {"Files", "History", "Branches", "Tags"})
        self.assertEqual(navigation["Files"]["current"], "page")
        for name, route in [("Files", "/files"), ("History", "/history"), ("Branches", "/branches"), ("Tags", "/tags")]:
            target = urllib.parse.urlsplit(navigation[name]["href"])
            self.assertEqual(target.path, route)
            self.assertEqual(urllib.parse.parse_qs(target.query).get("rev"), [snapshot])
        parents = [urllib.parse.urlsplit(anchor["href"]) for anchor in source.anchors if anchor["navigation"] == "Parent folders" and urllib.parse.urlsplit(anchor["href"]).path == "/files"]
        self.assertTrue(any(urllib.parse.parse_qs(parent.query).get("path") == ["src"] for parent in parents))
        for parent in parents:
            query = urllib.parse.parse_qs(parent.query)
            self.assertEqual(query.get("repo"), ["archive.git"])
            self.assertEqual(query.get("rev"), [snapshot])
        checkout, _ = self.page("/files", {"repo": "demo"})
        self.assertNotIn("Changes", [anchor["text"].strip() for anchor in checkout.anchors if anchor["navigation"] == "Repository"])
        for route, params in [("/new", None), ("/help", None), ("/files", {"repo": "demo"}), ("/branches", {"repo": "demo"}), ("/tags", {"repo": "demo"})]:
            page, _ = self.page(route, params)
            self.assertFalse(any(urllib.parse.urlsplit(url).path == "/new" for url in page.links), f"Create link is not home-only: {route}")


def main():
    global ARGS, MANIFEST
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--origin", help="Canonical origin when the base URL is an SSH forward")
    parser.add_argument("--fixtures", type=Path, required=True, help="Local generated fixture directory containing manifest.json")
    parser.add_argument("--ssh-host", help="Inspect transferred fixture metadata through SSH")
    parser.add_argument("--remote-root", help="Remote repositories directory (not the parent fixture directory)")
    parser.add_argument("--http-only", action="store_true", help="For an isolated VM: explicitly omit filesystem/permission/hook-marker checks")
    parser.add_argument("--native-delta", action="store_true", help="Assert native delta limit in HTTP-only mode when delta-bomb.git was added")
    ARGS = parser.parse_args()
    if bool(ARGS.ssh_host) != bool(ARGS.remote_root):
        parser.error("--ssh-host and --remote-root are used together")
    if ARGS.http_only and ARGS.ssh_host:
        parser.error("Choose either --http-only or remote filesystem inspection")
    MANIFEST = json.loads((ARGS.fixtures / "manifest.json").read_text())
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(GittSmoke)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    sys.exit(not result.wasSuccessful())


if __name__ == "__main__":
    main()
