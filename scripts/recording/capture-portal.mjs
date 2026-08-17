// Capture the H3 recall cards view as evidence, by driving a LOCAL
// `lambo serve-web` the way a judge would.
//
// The old script targeted the deployed exhibit's pre-rebuild DOM
// (#recall-query/#recall-go/#recall-out/#event-list) which no longer exists.
// The portal was rebuilt (5ccd48f) around #lookup-input/#lookup-btn/
// #lookup-cards/#lookup-fallback/#audit/#session-name, and H3 added
// #response-annotations and #excluded-warnings. This script drives that DOM
// against a local serve-web (the evidence runbook in evidence/h3-recall-cards/
// explains how to provision the session and start the server):
//
//   node scripts/recording/capture-portal.mjs          # PORTAL=http://127.0.0.1:7710
//   PORTAL=http://127.0.0.1:7799 node scripts/recording/capture-portal.mjs
//
// Playwright writes the video from the browser context itself, so nothing
// depends on the compositor; screenshots land in evidence/h3-recall-cards/.
// The script FAILS on unexpected browser console errors and on the XSS check,
// so a silent regression cannot be captured as evidence.
//
// Output: evidence/h3-recall-cards/cards-<utc>.png (+ full-page stills and a
// webm video). Captures are written unedited; no DSNs or keys are read here.

import { chromium } from '@playwright/test';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, '..', '..');
const OUT = resolve(REPO, 'evidence', 'h3-recall-cards');
const PORTAL = process.env.PORTAL ?? 'http://127.0.0.1:7710';

// The queries that tell the H3 story, in order:
// 1. a blended query whose top hit is the Canonical load-bearing pillar
//    (score bar, status badge, load-bearing annotation, cards view);
// 2. a structural query, so the traversal banner is on camera;
// 3. an excluded-budget query (intercepted with a tiny max_tokens) so the
//    persistent excluded-hit warnings area is on camera;
// 4. a malicious-content query, proving untrusted text renders as text.
const QUERIES = [
  { label: 'blended', query: 'update user schema' },
  { label: 'structural', query: 'what depends on SG-Base-VPC' },
  { label: 'tiny-budget', query: 'update user schema', maxTokens: 24 },
  { label: 'xss', query: 'malicious markup' },
];

const BEAT = 1800;

mkdirSync(OUT, { recursive: true });

const browser = await chromium.launch();
const context = await browser.newContext({
  viewport: { width: 1600, height: 900 },
  deviceScaleFactor: 2,
  recordVideo: { dir: OUT, size: { width: 1600, height: 900 } },
});
const page = await context.newPage();

const problems = [];
page.on('console', (m) => m.type() === 'error' && problems.push(`console: ${m.text()}`));
page.on('pageerror', (e) => problems.push(`pageerror: ${e.message}`));
page.on('requestfailed', (r) => problems.push(`requestfailed: ${r.method()} ${r.url()}`));

console.log(`portal: ${PORTAL}`);
await page.goto(PORTAL, { waitUntil: 'networkidle', timeout: 60_000 });

// Fail loudly rather than capturing a blank page: if the session name never
// renders, serve-web is up but not talking to the store.
await page.waitForFunction(
  () => (document.querySelector('#session-name')?.textContent ?? '').trim().length > 0,
  { timeout: 30_000 },
);
const session = (await page.locator('#session-name').textContent())?.trim();
console.log(`session on page: ${session}`);
await page.waitForTimeout(BEAT);

for (const q of QUERIES) {
  console.log(`recall: ${q.label}: ${q.query}`);
  const seq = `${q.label}-${q.maxTokens ?? 'default'}`;

  // Tiny-budget proof: intercept the next recall request and force a small
  // max_tokens, so the portal itself renders the excluded-hit warnings area.
  if (q.maxTokens !== undefined) {
    await page.route('**/api/recall**', (route) => {
      const url = new URL(route.request().url());
      url.searchParams.set('max_tokens', String(q.maxTokens));
      route.continue({ url: url.toString() });
    });
  }

  await page.waitForSelector('#lookup-btn:not([disabled])', { timeout: 90_000 });
  await page.fill('#lookup-input', '');
  await page.type('#lookup-input', q.query, { delay: 30, timeout: 60_000 });
  await page.click('#lookup-btn');
  await page
    .waitForFunction(
      () => {
        const busy = document.querySelector('#lookup-btn')?.disabled;
        const cards = document.querySelector('#lookup-cards')?.textContent ?? '';
        const fallback = document.querySelector('#lookup-fallback')?.textContent ?? '';
        return !busy && (cards.length > 0 || fallback.length > 0);
      },
      { timeout: 90_000 },
    )
    .catch(() => problems.push(`no recall output for: ${q.label}`));

  // The cards view is the default; the verbatim context view stays one toggle
  // away. Let the stage settle, then capture.
  await page.waitForTimeout(BEAT);
  await page.screenshot({ path: resolve(OUT, `cards-${q.label}-${seq}.png`), fullPage: false });

  if (q.label === 'tiny-budget') {
    // Prove the excluded-hit warning area is populated and its warning text
    // is the typed load-bearing annotation (no expander required).
    const excludedText = await page.locator('#excluded-warnings').textContent();
    if (!excludedText || !excludedText.includes('Load-bearing pillar')) {
      problems.push(`excluded-warnings area missing the load-bearing warning: ${excludedText}`);
    }
    const collapsedBodies = await page
      .locator('#lookup-cards .card.is-excluded .card-body')
      .count();
    if (collapsedBodies !== 0) {
      problems.push(`excluded cards must be collapsed by default (found ${collapsedBodies} bodies)`);
    }
  }

  if (q.label === 'xss') {
    // XSS regression: untrusted content must render as text, never markup.
    const img = await page.locator('#lookup-cards img').count();
    if (img !== 0) problems.push(`an <img> element was created from untrusted text`);
    const text = await page.locator('#lookup-cards').textContent();
    const marker = '<img src=x onerror=window.__h3xss=1>';
    if (!text.includes(marker)) problems.push(`untrusted text must appear verbatim, got: ${text}`);
    const fired = await page.evaluate(() => window.__h3xss);
    if (fired) problems.push('the injected onerror handler executed');
  }

  if (q.maxTokens !== undefined) {
    await page.unroute('**/api/recall**');
  }
}

// The verbatim context view stays available (H3 keeps it).
await page.fill('#lookup-input', '');
await page.type('#lookup-input', 'update user schema', { delay: 30, timeout: 60_000 });
await page.click('#lookup-btn');
await page.waitForFunction(
  () => {
    const busy = document.querySelector('#lookup-btn')?.disabled;
    return !busy && (document.querySelector('#lookup-cards')?.textContent ?? '').length > 0;
  },
  { timeout: 90_000 },
);
await page.click('#fallback-toggle');
await page.waitForTimeout(BEAT);
const verbatim = await page.locator('#lookup-fallback').textContent();
if (!verbatim || !verbatim.includes(', canonical]')) {
  problems.push(`verbatim context view missing the canonical marker: ${verbatim}`);
}
await page.screenshot({ path: resolve(OUT, 'verbatim-context.png'), fullPage: false });
await page.click('#fallback-toggle');
await page.waitForTimeout(BEAT);

// Linger on the canonization feed (the audit list).
await page.locator('#audit').scrollIntoViewIfNeeded().catch(() => {});
await page.waitForTimeout(BEAT);
await page.screenshot({ path: resolve(OUT, 'audit-feed.png'), fullPage: false });

const stamp = new Date().toISOString().replace(/[:.]/g, '-');
writeFileSync(resolve(OUT, `capture-${stamp}.txt`),
  `portal: ${PORTAL}\nsession: ${session}\nqueries: ${QUERIES.map((q) => q.label).join(', ')}\n`);

await context.close(); // flushes the video
await browser.close();

if (problems.length) {
  console.log('\nproblems observed:');
  for (const p of problems) console.log(`  ${p}`);
  process.exit(1);
}
console.log(`\nwrote H3 recall-cards evidence to evidence/h3-recall-cards/`);
