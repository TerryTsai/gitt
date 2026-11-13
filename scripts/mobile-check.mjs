#!/usr/bin/env node
// Development-only browser checks. The served application contains no JavaScript.
import assert from 'node:assert/strict';
import { mkdir, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { join, resolve } from 'node:path';

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.GITT_PLAYWRIGHT || 'playwright');
const base = new URL(process.env.GITT_URL || 'http://127.0.0.1:18080');
const routes = JSON.parse(process.env.GITT_ROUTES || '["/", "/new", "/help"]')
  .map(route => typeof route === 'string' ? { path: route, status: 200 } : route);
const screenshotRoute = process.env.GITT_SCREENSHOT_ROUTE || routes.find(route => route.path.startsWith('/files'))?.path || '/';
const output = resolve(process.env.GITT_ARTIFACT_DIR || `/tmp/gitt-ui-${Date.now()}`);
const maxCodeTop = process.env.GITT_MAX_CODE_TOP === undefined ? null : Number(process.env.GITT_MAX_CODE_TOP);
assert(maxCodeTop === null || (Number.isFinite(maxCodeTop) && maxCodeTop > 0), 'GITT_MAX_CODE_TOP must be a positive pixel value');
await mkdir(output, { recursive: true });

const browser = await chromium.launch({ headless: true });
const report = { base: base.origin, generated: new Date().toISOString(), javaScriptEnabled: false, maxCodeTop, pages: [], workflows: [], screenshots: [], screenshotMetrics: [], failures: [] };

async function sourcePosition(page) {
  return page.evaluate(() => {
    const pane = document.querySelector('pre[aria-label="File contents"]');
    const lines = pane ? [...pane.querySelectorAll('code a[id]')] : [];
    const documentTop = element => element ? Math.round((element.getBoundingClientRect().top + window.scrollY) * 100) / 100 : null;
    const completelyVisible = lines.filter(element => {
      const box = element.getBoundingClientRect();
      return box.top >= 0 && box.bottom <= window.innerHeight;
    });
    const partlyVisible = lines.filter(element => {
      const box = element.getBoundingClientRect();
      return box.top < window.innerHeight && box.bottom > 0;
    });
    return {
      codePaneTop: documentTop(pane),
      firstCodeLineTop: documentTop(lines[0]),
      completeVisibleLines: completelyVisible.length,
      visibleLines: partlyVisible.length,
      firstVisibleLine: partlyVisible[0]?.id ?? null,
      lastVisibleLine: partlyVisible.at(-1)?.id ?? null,
    };
  });
}

async function openHeaderMenu(page) {
  const menu = page.locator('header > details').first();
  if (!await menu.count()) return false;
  if (await menu.getAttribute('open') === null) await menu.locator(':scope > summary').click();
  assert.notEqual(await menu.getAttribute('open'), null, 'The native header menu must open with JavaScript disabled');
  return true;
}

try {
  for (const [name, viewport] of [['mobile', { width: 360, height: 800 }], ['desktop', { width: 1280, height: 900 }]]) {
    const context = await browser.newContext({ viewport, colorScheme: 'light', javaScriptEnabled: false });
    const page = await context.newPage();
    const externalRequests = [];
    const pageErrors = [];
    page.on('pageerror', error => pageErrors.push(error.message));
    await context.route('**/*', route => {
      const target = new URL(route.request().url());
      if (target.origin !== base.origin) {
        externalRequests.push(target.href);
        return route.abort();
      }
      return route.continue();
    });

    for (const { path: route, status: expectedStatus = 200 } of routes) {
      const started = performance.now();
      let response;
      try {
        response = await page.goto(new URL(route, base).href, { waitUntil: 'networkidle', timeout: 30_000 });
        const checks = await page.evaluate(() => {
          const failures = [];
          const all = [...document.querySelectorAll('*')];
          const ids = new Set();
          for (const element of all) {
            if (['SCRIPT', 'IFRAME', 'EMBED', 'OBJECT', 'BASE'].includes(element.tagName)) failures.push(`Forbidden element: ${element.tagName}`);
            for (const attribute of element.attributes) {
              if (/^on/i.test(attribute.name)) failures.push(`Event handler: ${element.tagName}.${attribute.name}`);
              if (attribute.name === 'style') failures.push(`Inline style on ${element.tagName}`);
              if (attribute.name === 'class' && !(element.tagName === 'SPAN' && element.closest('pre code'))) failures.push(`Non-syntax class on ${element.tagName}`);
              if (attribute.name === 'id' && !(/^L[1-9][0-9]*$/.test(attribute.value) && element.closest('pre code'))) failures.push(`Non-line ID: ${attribute.value}`);
              if (attribute.name === 'id') {
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
          const closedDetails = [...document.querySelectorAll('details:not([open])')];
          const concealedByDetails = element => closedDetails.some(details =>
            details !== element && details.contains(element) && !details.querySelector(':scope > summary')?.contains(element)
          );
          // Chromium may return phantom boxes for descendants of closed details.
          const protruding = all.filter(element => !element.closest('pre') && !concealedByDetails(element)).filter(element => {
            const box = element.getBoundingClientRect();
            return box.width && (box.right > window.innerWidth + 1 || box.left < -1);
          }).map(element => `${element.tagName}: ${element.textContent.slice(0, 100)}`);
          if (protruding.length) failures.push(`Content outside viewport: ${protruding.slice(0, 5).join('; ')}`);
          if (!document.querySelector('main h1')?.textContent.trim()) failures.push('Missing main page heading');
          if (!document.querySelector('header nav')) failures.push('Missing site navigation');
          if (document.querySelectorAll('img, video, audio, source').length) failures.push('Repository content must not load embedded media');
          return { failures, title: document.title, width: window.innerWidth, documentWidth: document.documentElement.scrollWidth, sourceLines: document.querySelectorAll('pre code a[id]').length, syntaxTokens: document.querySelectorAll('pre code span[class]').length, links: document.links.length };
        });
        const position = await sourcePosition(page);
        const result = { viewport: name, route, status: response?.status(), elapsedMs: Math.round(performance.now() - started), ...checks, ...position };
        report.pages.push(result);
        if (response?.status() !== expectedStatus) report.failures.push(`${name} ${route}: expected HTTP ${expectedStatus}, got ${response?.status()}`);
        for (const failure of checks.failures) report.failures.push(`${name} ${route}: ${failure}`);
      } catch (error) {
        report.failures.push(`${name} ${route}: ${error.message}`);
      }
    }

    if (process.env.GITT_WORKFLOW_REPO) {
      const repo = process.env.GITT_WORKFLOW_REPO;
      const path = (route, values) => new URL(`${route}?${new URLSearchParams({ repo, ...values })}`, base).href;
      let nativeMenu = false;
      try {
        await page.goto(base.href);
        await page.getByLabel('Repository name', { exact: true }).fill(repo);
        await page.getByRole('button', { name: 'Find', exact: true }).click();
        await page.waitForURL(url => url.searchParams.get('q') === repo);
        await page.getByRole('link', { name: repo, exact: true }).click();
        await page.waitForURL(url => url.pathname === '/files');
        await page.getByLabel('Find a path', { exact: true }).fill('main.rs');
        await page.getByRole('button', { name: 'Find', exact: true }).click();
        await page.getByRole('link', { name: 'src/main.rs', exact: true }).click();
        assert.equal(new URL(page.url()).searchParams.get('path'), 'src/main.rs');
        await page.getByRole('link', { name: 'Line 2', exact: true }).click();
        assert.equal(new URL(page.url()).hash, '#L2');
        nativeMenu = await openHeaderMenu(page) || nativeMenu;
        await page.getByRole('link', { name: 'Refresh', exact: true }).click();
        assert.equal(new URL(page.url()).searchParams.get('path'), 'src/main.rs');
        await page.goto(path('/files', { path: 'README.md', rev: 'main' }));
        await page.getByRole('link', { name: 'Source', exact: true }).click();
        assert.equal(new URL(page.url()).searchParams.get('path'), 'src/main.rs');
        assert(new URL(page.url()).searchParams.get('rev'), 'Markdown links must preserve their pinned snapshot');
        const snapshot = new URL(page.url()).searchParams.get('rev');
        nativeMenu = await openHeaderMenu(page) || nativeMenu;
        await page.getByRole('link', { name: 'History', exact: true }).click();
        assert.equal(new URL(page.url()).searchParams.get('rev'), snapshot, 'History tab must preserve the selected snapshot');
        nativeMenu = await openHeaderMenu(page) || nativeMenu;
        await page.getByRole('link', { name: 'Files', exact: true }).click();
        assert.equal(new URL(page.url()).searchParams.get('rev'), snapshot, 'Files tab must preserve the selected snapshot');
        report.workflows.push({ viewport: name, nativeMenu, repositorySearch: true, pathSearch: true, lineLink: true, refresh: true, markdownLink: true, snapshotNavigation: true });
      } catch (error) {
        report.failures.push(`${name} native navigation workflow: ${error.message}`);
      }
    }

    // A representative source view is captured from real server-rendered HTML.
    await page.goto(new URL(screenshotRoute, base).href, { waitUntil: 'networkidle' });
    const position = await sourcePosition(page);
    report.screenshotMetrics.push({ viewport: name, route: screenshotRoute, ...position });
    if (maxCodeTop !== null && (position.firstCodeLineTop === null || position.firstCodeLineTop > maxCodeTop)) {
      report.failures.push(`${name} ${screenshotRoute}: first code line is ${position.firstCodeLineTop ?? 'missing'}, expected at most ${maxCodeTop}px from document top`);
    }
    const screenshot = join(output, `${name}.png`);
    await page.screenshot({ path: screenshot, fullPage: true });
    report.screenshots.push(screenshot);

    // The site remains usable when the browser disables script execution entirely.
    const noScript = await browser.newContext({ viewport, javaScriptEnabled: false });
    const noScriptPage = await noScript.newPage();
    const noScriptResponse = await noScriptPage.goto(new URL(screenshotRoute, base).href, { waitUntil: 'load' });
    assert(noScriptResponse?.ok(), 'Source view must load with JavaScript disabled');
    assert(await noScriptPage.locator('main h1').textContent(), 'Source heading must exist with JavaScript disabled');
    await noScript.close();

    for (const url of externalRequests) report.failures.push(`${name}: external resource request ${url}`);
    for (const error of pageErrors) report.failures.push(`${name}: browser error ${error}`);
    await context.close();
  }
} finally {
  await browser.close();
  await writeFile(join(output, 'browser-report.json'), JSON.stringify(report, null, 2) + '\n');
}

console.log(JSON.stringify({ output, pages: report.pages.length, screenshots: report.screenshots, failures: report.failures }, null, 2));
if (report.failures.length) process.exitCode = 1;
