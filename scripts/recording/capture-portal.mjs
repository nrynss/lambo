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
// Round-1 review fixes (H3-R1-1): each cards screenshot now waits for the
// QUERY-SPECIFIC content to be actually rendered and visible (a real card
// element, the traversal banner, or the excluded-warnings area — never just
// `#lookup-cards` having leftover text from the previous render), then
// scrolls the results region (#lookup-results) into view so the cards, score
// bars, status badges, response annotations and excluded-warnings area are
// genuinely on camera instead of sitting below the 900px fold.
//
// Output: evidence/h3-recall-cards/cards-<utc>.png (+ full-page stills and a
// webm video). Captures are written unedited; no DSNs or keys are read here.

import pw from '/home/nryn/.hermes/hermes-agent/node_modules/playwright/index.js';
const { chromium } = pw;
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
const XSS_MARKER = '<img src=x onerror=window.__h3xss=1>';

mkdirSync(OUT, { recursive: true });

const browser = await chromium.launch(
  process.env.PLAYWRIGHT_EXECUTABLE
    ? { executablePath: process.env.PLAYWRIGHT_EXECUTABLE }
    : {},
);
const context = await browser.newContext({
  viewport: { width: 1600, height: 900 },
  deviceScaleFactor: 2,
  recordVideo: { dir: OUT, size: { width: 1600, height: 900 } },
});
const page = await context.newPage();

const problems = [];
page.on('console', (m) => {
  if (m.type() !== 'error') return;
  if (m.text().includes('favicon')) return; // the browser always 404s /favicon.ico on this server
  const loc = m.location();
  if (loc.url.includes('favicon')) return; // this server serves no favicon
  problems.push(`console: ${m.text()} @ ${loc.url}`);
});
page.on('pageerror', (e) => problems.push(`pageerror: ${e.message}`));
page.on('requestfailed', (r) => problems.push(`requestfailed: ${r.method()} ${r.url()}`));
page.on('response', (r) => {
  if (r.status() >= 400 && !r.url().includes('/favicon')) problems.push(`http ${r.status()}: ${r.url()}`);
});

// True when the element exists and is laid out (not `display: none` under a
// `.hidden` ancestor). Visibility checks are scroll-independent: they assert
// a REAL rendered element, not stale DOM text from a previous query.
const RENDERED = `(sel) => {
  const el = document.querySelector(sel);
  return !!el && el.getClientRects().length > 0;
}`;

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

// Per-query render proof: the specific content THIS query must produce is
// visible on screen (and the request has finished, so it cannot be a stale
// render from the previous query riding the same DOM text).
const RENDER_CONDITIONS = {
  blended: `() => {
    if (document.querySelector('#lookup-btn')?.disabled) return false;
    if (!(${RENDERED})('#lookup-results')) return false;
    // The Canonical pillar card with its score track and blast-radius note.
    const pillar = document.querySelector('#lookup-cards .card.is-pillar');
    return !!pillar && (${RENDERED})('#lookup-cards .card.is-pillar .score-track')
      && pillar.textContent.includes(' depend on it');
  }`,
  structural: `() => {
    if (document.querySelector('#lookup-btn')?.disabled) return false;
    if (!(${RENDERED})('#lookup-results')) return false;
    // The traversal banner (response_annotations) above a visible card.
    const ann = document.querySelector('#response-annotations');
    return (${RENDERED})('#response-annotations')
      && !!ann && ann.textContent.includes('graph traversal')
      && document.querySelectorAll('#lookup-cards .card').length > 0;
  }`,
  'tiny-budget': `() => {
    if (document.querySelector('#lookup-btn')?.disabled) return false;
    if (!(${RENDERED})('#lookup-results')) return false;
    // The persistent excluded-hit warnings area, populated with the typed
    // load-bearing warning and its owning hit; collapsed excluded cards.
    const w = document.querySelector('#excluded-warnings');
    return (${RENDERED})('#excluded-warnings')
      && !!w && w.textContent.includes('Load-bearing pillar')
      && w.textContent.includes('outside the context budget')
      && document.querySelectorAll('#lookup-cards .card.is-excluded').length > 0;
  }`,
  xss: `() => {
    if (document.querySelector('#lookup-btn')?.disabled) return false;
    if (!(${RENDERED})('#lookup-results')) return false;
    // The untrusted marker rendered as text inside a real card.
    const cards = document.querySelector('#lookup-cards');
    return (${RENDERED})('#lookup-cards')
      && !!cards && cards.textContent.includes(${JSON.stringify(XSS_MARKER)})
      && document.querySelectorAll('#lookup-cards .card').length > 0;
  }`,
};

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
    .waitForFunction(RENDER_CONDITIONS[q.label], { timeout: 90_000 })
    .catch(() => problems.push(`no rendered ${q.label} output for: ${q.query}`));

  // Let the stage settle, then put the H3 results region on camera: the
  // region top (response annotations, first card) pins to the viewport top,
  // so the screenshot genuinely shows the cards view instead of the legend
  // and structure tree that sit above it (round-1 finding H3-R1-1).
  await page.waitForTimeout(BEAT);
  await page.evaluate(() => {
    document.querySelector('#lookup-results')?.scrollIntoView({ block: 'start', inline: 'start' });
  });
  if (q.label === 'tiny-budget') {
    // The excluded-warnings area must be on camera too. After pinning the
    // results top, bring the warnings area fully into the viewport bottom if
    // the collapsed cards above it push it below the fold.
    const warningsBelowFold = await page.evaluate(() => {
      const w = document.querySelector('#excluded-warnings');
      if (!w) return true;
      const r = w.getBoundingClientRect();
      return r.bottom > window.innerHeight || r.top < 0;
    });
    if (warningsBelowFold) {
      await page.evaluate(() => {
        document.querySelector('#excluded-warnings')?.scrollIntoView({ block: 'end', inline: 'start' });
      });
    }
  }
  await page.waitForTimeout(400);
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
      .evaluateAll((els) => els.filter((el) => el.offsetParent !== null).length);
    if (collapsedBodies !== 0) {
      problems.push(`excluded cards must be collapsed by default (found ${collapsedBodies} visible bodies)`);
    }
    const expanded = await page.locator('#lookup-cards .card.is-excluded.is-open').count();
    if (expanded !== 0) problems.push('excluded cards must start collapsed');
  }

  if (q.label === 'xss') {
    // XSS regression: untrusted content must render as text, never markup.
    const img = await page.locator('#lookup-cards img').count();
    if (img !== 0) problems.push(`an <img> element was created from untrusted text`);
    const text = await page.locator('#lookup-cards').textContent();
    if (!text.includes(XSS_MARKER)) problems.push(`untrusted text must appear verbatim, got: ${text}`);
    const fired = await page.evaluate(() => window.__h3xss);
    if (fired) problems.push('the injected onerror handler executed');
  }

  if (q.maxTokens !== undefined) {
    await page.unroute('**/api/recall**');
  }
}

// The verbatim context view stays available (H3 keeps it). Wait for the
// fallback pane to actually render the canonical marker before capturing.
await page.fill('#lookup-input', '');
await page.type('#lookup-input', 'update user schema', { delay: 30, timeout: 60_000 });
await page.click('#lookup-btn');
await page.waitForFunction(
  RENDER_CONDITIONS.blended,
  { timeout: 90_000 },
).catch(() => problems.push('no rendered cards for the verbatim pre-query'));
await page.click('#fallback-toggle');
await page.waitForFunction(
  `() => {
    if (document.querySelector('#lookup-btn')?.disabled) return false;
    const fb = document.querySelector('#lookup-fallback');
    return !!fb && fb.getClientRects().length > 0 && fb.textContent.includes(', canonical]');
  }`,
  { timeout: 90_000 },
);
await page.waitForTimeout(BEAT);
await page.evaluate(() => {
  document.querySelector('#lookup-results')?.scrollIntoView({ block: 'start', inline: 'start' });
});
await page.waitForTimeout(400);
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
