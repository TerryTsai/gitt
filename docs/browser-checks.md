# Browser checks

Generate fixtures with `scripts/fixtures.py --security` and run gitt against their
`repositories/` directory. Install Playwright and its Chromium browser on the test
machine; they are development tools and are not part of the application.

This command checks 24 routes at both 360×800 and 1280×900 pixels and runs the
repository-search, path-search, source, line-link, refresh, Markdown-link, and
snapshot-navigation workflow at both sizes. Browser JavaScript is disabled in
all test contexts. Playwright's own inspection code reads the DOM and layout.

```sh
GITT_FIXTURES=/tmp/gitt-fixtures
gitt_root=$(git -C "$GITT_FIXTURES/repositories/demo" rev-list --max-parents=0 main)
gitt_merge=$(git -C "$GITT_FIXTURES/repositories/demo" rev-parse main)

GITT_URL=http://127.0.0.1:8080 \
GITT_PLAYWRIGHT=/path/to/node_modules/playwright \
GITT_WORKFLOW_REPO=demo \
GITT_SCREENSHOT_ROUTE='/files?repo=demo&path=src%2Fmain.rs' \
GITT_ARTIFACT_DIR=artifacts/browser-check \
GITT_ROUTES='[
  "/",
  "/new",
  "/help",
  "/files?repo=demo",
  "/files?repo=archive.git",
  "/files?repo=demo&path=src%2Fmain.rs",
  "/files?repo=archive.git&path=src%2Fmain.rs",
  "/files?repo=demo&path=README.md",
  "/files?repo=demo&path=README.md&source=1",
  "/files?repo=demo&path=docs%2Fhostile.md",
  "/files?repo=demo&path=hostile.html",
  "/files?repo=demo&path=binary.dat",
  "/changes?repo=demo",
  "/history?repo=demo",
  "/refs?repo=demo",
  "/compare?repo=demo",
  "/compare?repo=demo&from=v0.1&to=feature%2Fexample",
  "/commit?repo=demo&id='"$gitt_root"'",
  "/commit?repo=demo&id='"$gitt_merge"'",
  "/files?repo=team%2Fempty.git",
  "/files?repo=empty-checkout",
  {"path":"/files?repo=security%2Flinked","status":422},
  {"path":"/changes?repo=archive.git","status":422},
  {"path":"/files?repo=demo&path=missing","status":404}
]' \
node scripts/mobile-check.mjs
```

Choose a new output directory for each run. The report records individual checks
and failures; screenshots show a real source page. Unexpected external requests,
script elements, event handlers, inline styles, non-syntax classes, non-line IDs,
missing labels, and page-wide horizontal overflow fail the check. Scrolling inside
code blocks is allowed.
