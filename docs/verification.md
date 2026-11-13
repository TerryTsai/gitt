# Release verification: v0.1.0

The binary hashes in [performance.md](performance.md) identify the tested release.
All checks below used synthetic repositories; existing personal repositories were
not used as fixtures.

| Check | Result |
| --- | --- |
| Rust unit tests | 21 passed |
| Formatting and Clippy | Passed; Clippy warnings treated as errors |
| x86-64 integration suite | 15 passed; optional large-object case checked separately against the 1 GiB fixture |
| Real 1 GiB pack | 64 MiB blob preview/raw returned explicit 422 limits; subsequent README returned 200 |
| Packed delta allocation | A valid five-byte result requiring a 64 MiB delta base returned 422; server remained available |
| Full ARM64 guest integration | All 16 HTTP tests passed, no skips; filesystem/permission checks passed |
| Browser routes | 48 route/viewport checks at 360 and 1280 CSS pixels passed |
| Browser navigation | Two complete native-form/link workflows passed with JavaScript disabled |
| Standalone packaging | Both ELF executables have no interpreter and no shared-library dependencies |
| Existing Git compatibility | Real SSH clone/push/fetch and native Git commits/fsck passed on copied created repositories |
| Resource acceptance | Every target passed for the documented 300-repository workload |

The integration suite checks bare and working repositories, root/merge/shallow
history, snapshot comparison, staged/unstaged/untracked changes, ignored files,
non-UTF-8 paths, conflicting index stages, unsafe Markdown, unsupported layouts,
path/symlink escapes, Host/Origin and CSRF handling, creation collisions, body
limits, and absence of update/delete operations. Existing metadata hashes,
ownership, and permissions are compared before and after. New repositories must
have shared-group writable metadata. Filter and hook markers must remain absent.

The ARM run used QEMU 7.2.22, a cortex-a57 virtual CPU, 512 MiB guest RAM, Alpine
3.23.4, Linux 6.18.22, and Landlock ABI 7. The server ran as UID 65534, with no Git
executable installed. Creation's architecture-specific seccomp policy was exercised.
The VM was shut down after testing. This is compatibility evidence, not a native
ARM performance benchmark.

SSH compatibility used a temporary localhost-only OpenSSH server and Git 2.39.5.
An empty bare repository created by gitt was cloned, committed into, pushed,
cloned again, updated, and fetched. An empty checkout created by gitt accepted
ordinary local Git commits. Both passed `git fsck --strict`. This checks repository
format compatibility; configuring an operator's SSH service remains independent
of running gitt.

Browser checks reject scripts, external resource requests, unexpected classes or
IDs, missing labels, and page-wide overflow. They exercise repository/path search,
Markdown/source navigation, line links, refresh, and revision-preserving links.
Code is allowed to scroll horizontally. The README screenshots are real browser
captures, updated for the mobile layout follow-up below.

## Mobile layout follow-up — 2026-11-13

The first version fitted a phone viewport but placed too much navigation and
repeated metadata above the code. The revised layout uses native disclosures,
one linked path heading, inline file actions, and compact search forms.

Measured on `tests/smoke.py`, with JavaScript disabled in Chromium 149:

| Measurement | Before | After |
| --- | --- | --- |
| First code line, 360 and 390 px widths | 490.77 px | 147 px |
| Complete visible lines, 360×800 | 13 | 29 |
| Complete visible lines, 390×844 | 15 | 31 |
| Header height | 181.8 px | 45 px |
| Source font / line spacing | 14 / 22.4 px | 14 / 22.4 px |

The follow-up passed 21 unit tests, formatting/Clippy, 15 HTTP smoke cases
(the optional 1 GiB case was not repeated), 48 route/viewport browser checks,
and both native-menu navigation workflows. Six additional profiles cover
360, 390, and 1280 px widths in light and dark modes, including open disclosures,
parent links, raw downloads, snapshot navigation, and long paths. Main disclosure
and action controls retain 44 px touch heights. The stylesheet URL includes its
content hash so existing browser caches receive the new layout.

These are browser viewport measurements; phone browser chrome is not included.
WebKit could not launch on the test host because its required system libraries
are missing, so this follow-up does not claim a Safari or physical-device test.
The v0.1.0 resource measurements above were not rerun for this presentation change.

## Reproduction

Run `cargo test --locked`, `cargo fmt --all -- --check`, and
`cargo clippy --locked --all-targets -- -D warnings` with the pinned toolchain.
See [benchmark-method.md](benchmark-method.md) for fixtures, HTTP integration,
and performance commands. `scripts/mobile-check.mjs` uses `GITT_URL`, a JSON route list in `GITT_ROUTES`,
`GITT_WORKFLOW_REPO`, and `GITT_ARTIFACT_DIR`; it requires Playwright and Chromium
on the test machine. [Browser commands](browser-checks.md) reproduce the 48-route run. `scripts/arm-vm-test.sh --help` describes the full-system ARM harness
and its checksum-verified Alpine archive inputs.

These checks establish the listed behavior and workload results. They are not an
exhaustive proof for every possible Git object graph, concurrent filesystem
mutation, kernel configuration, or browser.
