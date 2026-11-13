#!/usr/bin/env node
// Development-only browser checks. The served application contains no JavaScript.
import assert from 'node:assert/strict';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { join, resolve } from 'node:path';

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.GITT_PLAYWRIGHT || 'playwright');
const base = new URL(process.env.GITT_URL || 'http://127.0.0.1:18080');
const manifest = process.env.GITT_FIXTURES
  ? JSON.parse(await readFile(join(process.env.GITT_FIXTURES, 'manifest.json'), 'utf8')) : null;
const repo = process.env.GITT_WORKFLOW_REPO || manifest?.fixtures?.demo;
const route = (pathname, values = {}) => `${pathname}?${new URLSearchParams(values)}`;
const defaults = ['/', '/search', '/new', '/help'];
if (repo) {
  defaults.push(
    route('/files', { repo }), route('/files', { repo: 'archive.git' }),
    route('/files', { repo, path: 'src/main.rs' }),
    route('/files', { repo: 'archive.git', path: 'src/main.rs' }),
    route('/files', { repo, path: 'README.md' }),
    route('/files', { repo, path: 'README.md', source: '1' }),
    route('/files', { repo, path: 'docs/hostile.md' }),
    route('/files', { repo, path: 'hostile.html' }),
    route('/files', { repo, path: 'binary.dat' }),
    route('/search', { repo, q: 'main.rs' }), route('/history', { repo }),
    route('/history', { repo, page: '1' }), route('/history', { repo: 'shallow' }),
    route('/branches', { repo }), route('/tags', { repo }),
    route('/branches', { repo: 'archive.git' }), route('/tags', { repo: 'archive.git' }),
    {path: route('/compare', {repo}), status: 404},
    {path: route('/commit', {repo}), status: 404},
    route('/files', { repo: 'team/empty.git' }),
    route('/files', { repo: 'empty-checkout' }),
    { path: route('/changes', { repo: 'archive.git' }), status: 404 },
    { path: route('/files', { repo, path: 'missing' }), status: 404 },
  );
  for (const id of [manifest?.fixtures?.root_commit, manifest?.fixtures?.merge_commit].filter(Boolean)) {
    defaults.push(route('/history', { repo, rev: id }));
  }
  if (manifest?.security_repositories?.length) {
    defaults.push({ path: route('/files', { repo: 'security/linked' }), status: 422 });
  }
}
const routes = [...(process.env.GITT_ROUTES ? JSON.parse(process.env.GITT_ROUTES) : defaults),
  ...JSON.parse(process.env.GITT_EXTRA_ROUTES || '[]')]
  .map(value => typeof value === 'string' ? { path: value, status: 200 } : value);
const longRoute = process.env.GITT_LONG_ROUTE;
if (longRoute && !routes.some(value => value.path === longRoute)) routes.push({ path: longRoute, status: 200 });
const screenshotRoute = process.env.GITT_SCREENSHOT_ROUTE
  || (repo ? route('/files', { repo, path: 'src/main.rs' }) : routes.find(value => value.path.startsWith('/files'))?.path)
  || '/';
const output = resolve(process.env.GITT_ARTIFACT_DIR || `/tmp/gitt-ui-${Date.now()}`);
const maxCodeTop = process.env.GITT_MAX_CODE_TOP === undefined ? null : Number(process.env.GITT_MAX_CODE_TOP);
assert(maxCodeTop === null || (Number.isFinite(maxCodeTop) && maxCodeTop > 0), 'GITT_MAX_CODE_TOP must be a positive pixel value');
const binarySha256 = process.env.GITT_BINARY_SHA256 || null;
assert(binarySha256 === null || /^[0-9a-f]{64}$/.test(binarySha256), 'GITT_BINARY_SHA256 must be a SHA-256 digest of the running executable');
await mkdir(output, { recursive: true });

const viewports = [
  ['narrow', { width: 320, height: 800 }],
  ['mobile', { width: 390, height: 844 }],
  ['desktop', { width: 1280, height: 900 }],
];
const browser = await chromium.launch({ headless: true, ...(process.env.GITT_CHROMIUM ? { executablePath: process.env.GITT_CHROMIUM } : {}) });
const report = {
  base: base.origin, generated: new Date().toISOString(), binarySha256, browserVersion: browser.version(), javaScriptEnabled: false,
  maxCodeTop, coverage: { fixtureRoutes: Boolean(manifest), workflows: Boolean(repo), longBreadcrumbs: Boolean(longRoute) },
  pages: [], workflows: [], screenshots: [], screenshotMetrics: [], failures: [],
};

async function sourcePosition(page) {
  return page.evaluate(() => {
    const pane = document.querySelector('pre[aria-label="File contents"]');
    const code = pane?.querySelector('code');
    const lines = pane ? [...pane.querySelectorAll('code a[id]')] : [];
    const documentTop = element => element ? Math.round((element.getBoundingClientRect().top + window.scrollY) * 100) / 100 : null;
    const visible = lines.filter(element => {
      const box = element.getBoundingClientRect();
      return box.top < window.innerHeight && box.bottom > 0;
    });
    return {
      codePaneTop: documentTop(pane), firstCodeLineTop: documentTop(lines[0]),
      completeVisibleLines: lines.filter(element => {
        const box = element.getBoundingClientRect();
        return box.top >= 0 && box.bottom <= window.innerHeight;
      }).length,
      visibleLines: visible.length, firstVisibleLine: visible[0]?.id ?? null, lastVisibleLine: visible.at(-1)?.id ?? null,
      sourceFontSize: code ? getComputedStyle(code).fontSize : null,
      sourceLineHeight: code ? getComputedStyle(code).lineHeight : null,
      gutterFontSize: lines[0] ? getComputedStyle(lines[0]).fontSize : null,
      paneClientWidth: pane?.clientWidth ?? null, paneScrollWidth: pane?.scrollWidth ?? null,
    };
  });
}

async function inspectPage(page) {
  return page.evaluate(() => {
    const failures = [];
    const all = [...document.querySelectorAll('*')];
    const ids = new Set();
    for (const element of all) {
      if (['SCRIPT', 'IFRAME', 'EMBED', 'OBJECT', 'BASE'].includes(element.tagName)) failures.push(`Forbidden element: ${element.tagName}`);
      for (const attribute of element.attributes) {
        if (/^on/i.test(attribute.name)) failures.push(`Event handler: ${element.tagName}.${attribute.name}`);
        if (attribute.name === 'style') failures.push(`Inline style on ${element.tagName}`);
        if (attribute.name === 'class' && !(element.tagName === 'SPAN' && element.closest('pre code'))) failures.push(`Non-syntax class on ${element.tagName}`);
        if (attribute.name === 'id') {
          if (!(element.tagName === 'A' && /^L[1-9][0-9]*$/.test(attribute.value) && element.closest('pre code'))) failures.push(`Non-line ID: ${attribute.value}`);
          if (ids.has(attribute.value)) failures.push(`Duplicate ID: ${attribute.value}`);
          ids.add(attribute.value);
        }
      }
    }
    for (const anchor of document.querySelectorAll('a[href]')) {
      if (!['http:', 'https:', 'mailto:'].includes(new URL(anchor.href).protocol)) failures.push(`Unsafe link: ${anchor.getAttribute('href')}`);
    }
    for (const field of document.querySelectorAll('input:not([type="hidden"]), select, textarea')) {
      if (!field.labels?.length && !field.getAttribute('aria-label') && !field.getAttribute('aria-labelledby')) failures.push(`Unlabelled control: ${field.name}`);
    }
    const overflow = document.documentElement.scrollWidth - window.innerWidth;
    if (overflow > 1) failures.push(`Page overflows horizontally by ${overflow}px`);
    const listPanes = [...document.querySelectorAll('main > div[role="region"]')];
    const lists = [];
    for (const pane of listPanes) {
      const table = pane.querySelector(':scope > table');
      if (!table) continue;
      const name = table.getAttribute('aria-label');
      const headers = [...table.querySelectorAll('thead th')];
      const rows = [...table.querySelectorAll('tbody tr')];
      if (pane.tabIndex !== 0 || !pane.getAttribute('aria-label')) failures.push(`${name}: scroll region needs a keyboard stop and accessible name`);
      for (const row of table.rows) {
        if (row.cells.length !== headers.length) failures.push(`${name}: inconsistent column count`);
        for (const [index, cell] of [...row.cells].entries()) {
          const style = getComputedStyle(cell);
          const box = cell.getBoundingClientRect();
          if (!box.width || !box.height || style.visibility !== 'visible') failures.push(`${name}: column ${index + 1} is hidden`);
          if (cell.scrollWidth > cell.clientWidth + 1) failures.push(`${name}: column ${index + 1} clips its content`);
          const walker = document.createTreeWalker(cell, NodeFilter.SHOW_TEXT);
          const textBoxes = [];
          while (walker.nextNode()) {
            if (!walker.currentNode.textContent.trim()) continue;
            const range = document.createRange();
            range.selectNodeContents(walker.currentNode);
            textBoxes.push(...[...range.getClientRects()].filter(rect => rect.width && rect.height));
          }
          if (textBoxes.length && Math.max(...textBoxes.map(rect => rect.bottom)) - Math.min(...textBoxes.map(rect => rect.top)) > parseFloat(style.lineHeight) + 2) {
            failures.push(`${name}: column ${index + 1} wraps onto multiple lines`);
          }
          const heading = headers[index]?.getBoundingClientRect();
          if (heading && (Math.abs(box.left - heading.left) > 1 || Math.abs(box.right - heading.right) > 1)) failures.push(`${name}: column ${index + 1} is misaligned`);
        }
        const tops = [...row.cells].map(cell => cell.getBoundingClientRect().top);
        if (Math.max(...tops) - Math.min(...tops) > 1) failures.push(`${name}: cells stack vertically`);
      }
      pane.scrollLeft = pane.scrollWidth;
      const scrollLeft = pane.scrollLeft;
      if (pane.scrollWidth > pane.clientWidth + 1 && !scrollLeft) failures.push(`${name}: overflowing list cannot scroll`);
      pane.scrollLeft = 0;
      lists.push({ name, columns: headers.map(cell => cell.textContent.trim()), rows: rows.length, clientWidth: pane.clientWidth, scrollWidth: pane.scrollWidth, scrollLeft });
    }
    const concealed = element => [...document.querySelectorAll('details:not([open])')].some(details =>
      details !== element && details.contains(element) && !details.querySelector(':scope > summary')?.contains(element));
    const protruding = all.filter(element => !element.closest('pre') && !concealed(element)).filter(element => {
      const box = element.getBoundingClientRect();
      // List content scrolls within its pane; the pane itself must fit the page.
      if (listPanes.some(pane => element !== pane && pane.contains(element))) return false;
      // Text clipped by the breadcrumb's ellipsis is allowed, but its container must fit.
      const clip = element.closest('nav[aria-label="Breadcrumb"]');
      if (clip && element !== clip) return false;
      return box.width && (box.right > window.innerWidth + 1 || box.left < -1);
    }).map(element => `${element.tagName}: ${element.textContent.slice(0, 100)}`);
    if (protruding.length) failures.push(`Content outside viewport: ${protruding.slice(0, 5).join('; ')}`);
    const main = document.querySelector('main');
    const breadcrumb = document.querySelector('body > header nav[aria-label="Breadcrumb"]');
    if (!main) failures.push('Missing main content landmark');
    if (!document.title.trim()) failures.push('Missing page title');
    if (!breadcrumb) failures.push('Missing breadcrumb navigation');
    if ([...document.querySelectorAll('body > header details')].some(element => !element.closest('nav[aria-label="Breadcrumb"]'))) failures.push('Only parent folders use a header disclosure');
    const headerLabels = [...document.querySelectorAll('body > header a')].map(anchor => anchor.textContent.trim());
    if (headerLabels.includes('Refresh') || headerLabels.includes('Menu') || headerLabels.includes('Repositories')) failures.push('Retired header control is still present');
    const create = [...document.querySelectorAll('a[href]')].filter(anchor => new URL(anchor.href).pathname === '/new');
    if (location.pathname === '/') {
      if (create.length !== 1 || create[0].textContent.trim() !== 'Create') failures.push('Home must provide one Create link');
    } else if (create.length) failures.push('Create must appear only on home');
    const search = document.querySelector('nav[aria-label="Page actions"] > a:last-child');
    if (!search || search.textContent !== 'Search' || getComputedStyle(search).position !== 'fixed') failures.push('Search must be fixed at the top right');
    if (document.querySelectorAll('img, video, audio, source').length) failures.push('Repository content must not load embedded media');
    for (const pane of document.querySelectorAll('pre')) {
      const box = pane.getBoundingClientRect();
      if (box.left < -1 || box.right > window.innerWidth + 1) failures.push('Source/diff pane extends outside the viewport');
    }
    return {
      failures, title: document.title, width: window.innerWidth, documentWidth: document.documentElement.scrollWidth,
      sourceLines: document.querySelectorAll('pre code a[id]').length,
      syntaxTokens: document.querySelectorAll('pre code span[class]').length,
      links: document.links.length, lists,
    };
  });
}

async function checkMetadata(page) {
  await page.goto(new URL(route('/files', { repo, path: 'src', rev: 'main' }), base).href);
  const metadata = await page.evaluate(() => {
    const table = document.querySelector('table[aria-label="Files"]');
    const headers = table ? [...table.querySelectorAll('thead th')].map(cell => cell.textContent.trim()) : [];
    const row = table ? [...table.querySelectorAll('tbody tr')].find(candidate => [...candidate.querySelectorAll('a[href]')].some(anchor => {
      const target = new URL(anchor.href);
      return target.pathname === '/files' && target.searchParams.get('path') === 'src/main.rs';
    })) : null;
    const cells = row ? [...row.children] : [];
    const commit = row?.cells[1]?.querySelector('a[href]');
    const time = row?.querySelector('time[datetime]');
    return {
      headers, visibleText: row?.innerText ?? '',
      cells: Object.fromEntries(headers.map((header, index) => [header, cells[index]?.textContent.trim()])),
      commitId: commit ? new URL(commit.href).searchParams.get('rev') : null,
      commitTitle: commit?.textContent.trim(), datetime: time?.dateTime ?? null,
    };
  });
  assert.deepEqual(metadata.headers, ['Name', 'Last commit', 'Author', 'Size', 'Age', 'Mode'], 'File listing must expose the agreed metadata');
  assert.match(metadata.visibleText, /Fixture Author/, 'Commit author must remain visible on mobile');
  assert.match(metadata.visibleText, /Initial fixture/, 'Last commit must remain visible on mobile');
  assert.equal(metadata.cells.Mode, 'rw- r-- r--');
  assert(metadata.cells.Size && !['—', '-'].includes(metadata.cells.Size), 'Tracked file size must be available');
  assert(Number.isFinite(Date.parse(metadata.datetime)), 'Age needs a machine-readable commit date');
  if (manifest?.fixtures?.root_commit) assert.equal(metadata.commitId, manifest.fixtures.root_commit, 'Metadata must link the actual last touching commit');
  return metadata;
}

async function checkLongBreadcrumbs(page) {
  const target = new URL(longRoute, base);
  const file = target.searchParams.get('path');
  const revision = target.searchParams.get('rev');
  assert(file?.split('/').length >= 4, 'GITT_LONG_ROUTE must exercise at least four path components');
  await page.goto(target.href);
  const crumbs = page.getByRole('navigation', { name: 'Breadcrumb', exact: true });
  const breadcrumb = await crumbs.evaluate(element => ({
    text: element.textContent, title: element.querySelector('strong')?.getAttribute('title') || element.getAttribute('title'),
    links: [...element.querySelectorAll('a[href]')].map(anchor => anchor.href),
    current: element.querySelector('strong')?.textContent,
  }));
  assert.equal(breadcrumb.current, file.split('/').at(-1), 'Truncation must retain the full filename in the DOM');
  assert(breadcrumb.title?.includes(file), 'Long breadcrumb needs its full path in a title');
  const parents = breadcrumb.links.map(value => new URL(value)).filter(value => value.pathname === '/files');
  assert(parents.length >= 3, 'Long breadcrumb needs navigable ancestors');
  for (const parent of parents) {
    assert.equal(parent.searchParams.get('repo'), target.searchParams.get('repo'));
    if (revision) assert.equal(parent.searchParams.get('rev'), revision, 'Breadcrumb must preserve the selected snapshot');
  }
  const parentPath = file.split('/').slice(0, -1).join('/');
  const parentIndex = breadcrumb.links.findIndex(value => new URL(value).searchParams.get('path') === parentPath);
  assert(parentIndex >= 0, 'Immediate parent must remain addressable');
  const scrolling = await page.locator('pre[aria-label="File contents"]').evaluate(pane => {
    pane.scrollLeft = pane.scrollWidth;
    return { scrollLeft: pane.scrollLeft, clientWidth: pane.clientWidth, scrollWidth: pane.scrollWidth, pageScrollX: window.scrollX, pageWidth: document.documentElement.scrollWidth, viewportWidth: window.innerWidth };
  });
  assert(scrolling.scrollWidth > scrolling.clientWidth && scrolling.scrollLeft > 0, 'Long source lines must scroll inside the code pane');
  assert.equal(scrolling.pageScrollX, 0, 'Source scrolling must not move the page horizontally');
  assert(scrolling.pageWidth <= scrolling.viewportWidth + 1, 'Long breadcrumbs/source must not widen the page');
  await crumbs.locator('summary').click();
  await crumbs.locator('a[href]').nth(parentIndex).focus();
  await page.keyboard.press('Enter');
  await page.waitForURL(value => value.pathname === '/files' && value.searchParams.get('path') === parentPath);
  if (revision) assert.equal(new URL(page.url()).searchParams.get('rev'), revision, 'Opening the parent must keep the snapshot');
  return { breadcrumb, scrolling, parentKeyboardNavigation: true };
}

try {
  for (const [name, viewport] of viewports) {
    for (const colorScheme of ['light', 'dark']) {
      const label = `${name}-${colorScheme}`;
      const context = await browser.newContext({ viewport, colorScheme, javaScriptEnabled: false });
      const page = await context.newPage();
      const externalRequests = [];
      const pageErrors = [];
      page.on('pageerror', error => pageErrors.push(error.message));
      await context.route('**/*', request => {
        const target = new URL(request.request().url());
        if (target.origin !== base.origin) {
          externalRequests.push(target.href);
          return request.abort();
        }
        return request.continue();
      });
      for (const { path, status = 200 } of routes) {
        const started = performance.now();
        try {
          const response = await page.goto(new URL(path, base).href, { waitUntil: 'load', timeout: 30_000 });
          const checks = await inspectPage(page);
          report.pages.push({ viewport: name, colorScheme, route: path, status: response?.status(), elapsedMs: Math.round(performance.now() - started), ...checks, ...await sourcePosition(page) });
          if (response?.status() !== status) report.failures.push(`${label} ${path}: expected HTTP ${status}, got ${response?.status()}`);
          for (const failure of checks.failures) report.failures.push(`${label} ${path}: ${failure}`);
        } catch (error) {
          report.failures.push(`${label} ${path}: ${error.message}`);
        }
      }

      if (repo) {
        try {
          await page.goto(base.href);
          await page.getByRole('link', { name: 'Search', exact: true }).click();
          await page.getByLabel('Search query', { exact: true }).fill(repo);
          await page.getByRole('button', { name: 'Search', exact: true }).click();
          await page.waitForURL(url => url.searchParams.get('q') === repo);
          await page.getByRole('link', { name: repo, exact: true }).click();
          await page.waitForURL(url => url.pathname === '/files');
          await page.getByRole('link', { name: 'Search', exact: true }).click();
          await page.getByLabel('Search query', { exact: true }).fill('main.rs');
          await page.getByRole('button', { name: 'Search', exact: true }).click();
          await page.getByRole('link', { name: 'src/main.rs', exact: true }).click();
          assert.equal(new URL(page.url()).searchParams.get('path'), 'src/main.rs');
          await page.getByRole('link', { name: 'Line 2', exact: true }).focus();
          await page.keyboard.press('Enter');
          assert.equal(new URL(page.url()).hash, '#L2');
          await page.goto(new URL(route('/files', { repo, path: 'README.md', rev: 'main' }), base).href);
          await page.getByRole('link', { name: 'Source', exact: true }).click();
          assert.equal(new URL(page.url()).searchParams.get('path'), 'src/main.rs');
          const snapshot = new URL(page.url()).searchParams.get('rev');
          assert.equal(snapshot, 'main', 'Markdown links must preserve the selected branch name');
          const breadcrumbLinks = await page.getByRole('navigation', { name: 'Breadcrumb', exact: true }).locator('a[href]').evaluateAll(links => links.map(link => link.href));
          const parent = breadcrumbLinks.map(value => new URL(value)).find(value => value.searchParams.get('path') === 'src');
          assert.equal(parent?.searchParams.get('rev'), snapshot, 'Parent breadcrumb must retain the snapshot');
          await page.goto(parent.href);
          const nav = page.getByRole('navigation', { name: 'Repository', exact: true });
          await nav.getByRole('link', { name: 'History', exact: true }).click();
          assert.equal(new URL(page.url()).searchParams.get('rev'), snapshot, 'History must preserve the selected snapshot');
          await nav.getByRole('link', { name: 'Files', exact: true }).click();
          assert.equal(new URL(page.url()).searchParams.get('rev'), snapshot, 'Files must preserve the selected snapshot');
          for (const [name, pathname, ref] of [['Branches', '/branches', 'feature/example'], ['Tags', '/tags', 'v0.1']]) {
            await nav.getByRole('link', { name, exact: true }).click();
            assert.equal(new URL(page.url()).pathname, pathname);
            assert.equal(await nav.getByRole('link', { name, exact: true }).getAttribute('aria-current'), 'page');
            const refLink = page.locator('main table').getByRole('link', { name: ref, exact: true });
            await refLink.click();
            assert.equal(new URL(page.url()).pathname, pathname);
            const chosen = `refs/${name === 'Tags' ? 'tags' : 'heads'}/${ref}`;
            assert.equal(new URL(page.url()).searchParams.get('rev'), chosen);
            assert.equal((await page.locator('p[aria-label="Current revision"]').textContent()).trim(), ref);
            assert.equal(await page.locator('p[aria-label="Current revision"] a').count(), 0);
            await nav.getByRole('link', {name: 'Files', exact: true}).click();
            assert.equal(new URL(page.url()).searchParams.get('rev'), chosen);
          }
          const metadata = await checkMetadata(page);
          const longBreadcrumbs = longRoute ? await checkLongBreadcrumbs(page) : null;
          report.workflows.push({ viewport: name, colorScheme, repositorySearch: true, pathSearch: true, lineLink: true, markdownLink: true, snapshotNavigation: true, branches: true, tags: true, metadata, longBreadcrumbs });
        } catch (error) {
          report.failures.push(`${label} navigation workflow: ${error.message}`);
        }
      }

      try {
        await page.goto(new URL(screenshotRoute, base).href, { waitUntil: 'load' });
        const position = await sourcePosition(page);
        report.screenshotMetrics.push({ viewport: name, colorScheme, route: screenshotRoute, ...position });
        if (maxCodeTop !== null && (position.firstCodeLineTop === null || position.firstCodeLineTop > maxCodeTop)) {
          report.failures.push(`${label} ${screenshotRoute}: first code line is ${position.firstCodeLineTop ?? 'missing'}, expected at most ${maxCodeTop}px from document top`);
        }
        const screenshot = join(output, `${label}.png`);
        await page.screenshot({ path: screenshot, fullPage: true });
        report.screenshots.push(screenshot);
      } catch (error) {
        report.failures.push(`${label} screenshot: ${error.message}`);
      }
      for (const url of externalRequests) report.failures.push(`${label}: external resource request ${url}`);
      for (const error of pageErrors) report.failures.push(`${label}: browser error ${error}`);
      await context.close();
    }
  }
} finally {
  await browser.close();
  await writeFile(join(output, 'browser-report.json'), JSON.stringify(report, null, 2) + '\n');
}

console.log(JSON.stringify({ output, pages: report.pages.length, coverage: report.coverage, screenshots: report.screenshots, failures: report.failures }, null, 2));
if (report.failures.length) process.exitCode = 1;
