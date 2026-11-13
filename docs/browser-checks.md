# Browser checks

Generate disposable fixtures with `scripts/fixtures.py --security` and run gitt
against their `repositories/` directory. Playwright and Chromium are development
tools on the test machine; they are not part of the application or required on an
SSH target. Use a separate server port when another instance is running.
`GITT_CHROMIUM=/path/to/chrome` can select an already-installed compatible browser
when only its full Chromium executable is available instead of the headless shell.

The compact interface is checked at 320×800, 390×844, and 1280×900 CSS pixels in
both light and dark mode. Browser JavaScript is disabled in every context.
Playwright's inspection code reads the DOM and layout through the browser's test
interface; the application sends no JavaScript.

The fixture manifest supplies root/merge commit IDs and enables ordinary, empty,
shallow, hostile-content, and unsupported-layout routes. Native browser actions
exercise the fixed Search button, repository selection, repository-name and committed path search, line links, Markdown links, separate Branches
and Tags pages, and Files/History/Branches/Tags navigation with a stable named selection or commit. File-table
checks compare the displayed last commit against the fixture's actual root commit
and ensure its subject and author remain in distinct columns on mobile. Repository, file, search-result, branch, tag, and history tables are checked for visible, aligned, single-line cells and usable internal scrolling; the page itself must fit the viewport.

Add a separate synthetic repository for long breadcrumbs before starting the
server. Do not commit into the fixture's dirty `demo` checkout: its staged and
unstaged changes are deliberate integration-test inputs. The long-path repository
needs at least four path components, a long filename, and a source line of at least
512 characters so it requires internal scrolling at desktop width. Use its full
commit SHA in the test URL. For example:

```sh
GITT_FIXTURES=/tmp/gitt-fixtures
gitt_long_path='nested/long-directory-name/another-directory/source/a-very-long-source-filename-for-checking-breadcrumb-truncation-and-navigation.rs'
gitt_long_commit=$(git -C "$GITT_FIXTURES/repositories/long-breadcrumbs" rev-parse HEAD)
gitt_long_route="/files?repo=long-breadcrumbs&path=$gitt_long_path&rev=$gitt_long_commit"

GITT_URL=http://127.0.0.1:8080 \
GITT_PLAYWRIGHT=/path/to/node_modules/playwright \
GITT_FIXTURES="$GITT_FIXTURES" \
GITT_WORKFLOW_REPO=demo \
GITT_SCREENSHOT_ROUTE='/files?repo=demo&path=src%2Fmain.rs' \
GITT_LONG_ROUTE="$gitt_long_route" \
GITT_MAX_CODE_TOP=180 \
GITT_ARTIFACT_DIR=artifacts/browser-check \
node scripts/mobile-check.mjs
```

Choose a new output directory for each run. `GITT_ROUTES` can override the default
route list, or `GITT_EXTRA_ROUTES` can append cases for long repository names, paths, messages, branch/tag names, with a JSON array of paths or objects such as
`{"path":"/files?repo=absent","status":404}`. Without a fixture manifest or a long
route, the report marks those coverage areas absent; that is a partial check.
Set `GITT_BINARY_SHA256` to the independently verified digest of the running
executable when recording release-candidate evidence; the report retains it.

The report records each route, color scheme, width, workflow, and failure. Six
screenshots capture the real server-rendered source page. Unexpected external
requests, executable elements, event handlers, inline styles, non-syntax classes,
non-line IDs, missing labels, and page-wide horizontal overflow fail the check.
The header has visible navigation with no Menu or Refresh disclosure, and only
home has a Create link. A long breadcrumb may use ellipsis while keeping its
full path in a title and its ancestors addressable. Long source lines must scroll
inside the code pane without widening or moving the page.

The report also records the source pane position, first code line position,
complete visible line count, source font/line height, and gutter font size. The
approved compact source uses 11 px type, 16 px lines, and 9 px line numbers. These
measurements support visual review without asserting the CSS's implementation.
Set `GITT_MAX_CODE_TOP=180` with a representative
source screenshot route to fail if its first line starts more than 180 CSS pixels
below the document top. This checks useful content density in addition to fitting
the viewport. Use an ordinary path without extra limit notices; long names
and warnings are intentionally allowed to add height.

Run `tests/smoke.py` separately against the same disposable fixture root. It checks
host and CSRF handling, byte-safe paths, escaping, unsupported layouts, object
limits, creation/collision behavior, and existing metadata preservation through
SSH or local filesystem inspection. The browser checks are read-only; the smoke
suite creates uniquely named empty repositories and retains them.
