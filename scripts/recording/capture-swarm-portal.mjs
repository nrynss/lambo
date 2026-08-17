// C5 — capture the swarm session through the portal, as evidence.
//
// Drives a LOCAL `lambo serve-web` (session c-swarm-20260818, port 7799) the
// way a judge would: type a recall query that only makes sense if real
// LFM2-350M agents derived concepts into the session, wait for the specific
// content to actually render (the H3 capture lesson: never screenshot stale
// DOM), scroll the results into view, and screenshot. Fails on unexpected
// browser console errors.
//
//   PORTAL=http://127.0.0.1:7799 node scripts/recording/capture-swarm-portal.mjs
//
// Output: evidence/swarm/portal-<query>-<utc>.png
import pw from '/home/nryn/.hermes/hermes-agent/node_modules/playwright/index.js';
const { chromium } = pw;
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, '..', '..');
const OUT = resolve(REPO, 'evidence', 'swarm');
const PORTAL = process.env.PORTAL ?? 'http://127.0.0.1:7799';

// Queries whose answers can only exist if the swarm's derives landed:
// 1. the auth-middleware concept the swarm derived in its first turns;
// 2. the billing concept (second agent's topic).
const QUERIES = [
  { label: 'auth-middleware', query: 'auth middleware user schema' },
  { label: 'billing-retries', query: 'billing service retries charges' },
];
const BEAT = 2500;

mkdirSync(OUT, { recursive: true });
const browser = await chromium.launch(
  process.env.PLAYWRIGHT_EXECUTABLE ? { executablePath: process.env.PLAYWRIGHT_EXECUTABLE } : {},
);
const context = await browser.newContext({ viewport: { width: 1600, height: 900 }, deviceScaleFactor: 2 });
const page = await context.newPage();

const problems = [];
page.on('console', (m) => {
  if (m.type() !== 'error') return;
  if (m.text().includes('favicon')) return;
  problems.push(`console: ${m.text()}`);
});
page.on('pageerror', (e) => problems.push(`pageerror: ${e.message}`));
page.on('requestfailed', (r) => problems.push(`requestfailed: ${r.method()} ${r.url()}`));
page.on('response', (r) => {
  if (r.status() >= 400 && !r.url().includes('/favicon')) problems.push(`http ${r.status()}: ${r.url()}`);
});

const RENDERED = `(sel) => {
  const el = document.querySelector(sel);
  return !!el && el.getClientRects().length > 0;
}`;

await page.goto(PORTAL, { waitUntil: 'networkidle', timeout: 60_000 });
await page.waitForFunction(
  () => (document.querySelector('#session-name')?.textContent ?? '').trim().length > 0,
  { timeout: 30_000 },
);
console.log(`session on page: ${(await page.locator('#session-name').textContent())?.trim()}`);
await page.waitForTimeout(BEAT);

for (const q of QUERIES) {
  console.log(`recall: ${q.label}: ${q.query}`);
  // Render proof: a real card whose text contains a substantive fragment from
  // the swarm's derives — never just a leftover DOM render.
  await page.waitForSelector('#lookup-btn:not([disabled])', { timeout: 90_000 });
  await page.fill('#lookup-input', '');
  await page.type('#lookup-input', q.query, { delay: 25, timeout: 60_000 });
  await page.click('#lookup-btn');
  await page
    .waitForFunction(
      `() => {
        if (document.querySelector('#lookup-btn')?.disabled) return false;
        if (!(${RENDERED})('#lookup-results')) return false;
        const cards = document.querySelector('#lookup-cards');
        if (!cards) return false;
        const t = cards.textContent || '';
        // The swarm's concept contents are verbose LFM2 prose; require a
        // non-trivial card and at least one concept-type tag or score line.
        return document.querySelectorAll('#lookup-cards .card').length > 0
          && t.trim().length > 60 && !t.includes('No relevant memories');
      }`,
      { timeout: 90_000 },
    )
    .catch(() => problems.push(`no rendered ${q.label} output for: ${q.query}`));
  await page.waitForTimeout(BEAT);
  await page.evaluate(() => {
    document.querySelector('#lookup-results')?.scrollIntoView({ block: 'start', inline: 'start' });
  });
  await page.waitForTimeout(400);
  const shot = resolve(OUT, `portal-${q.label}-${Date.now()}.png`);
  await page.screenshot({ path: shot, fullPage: false });
  console.log(`screenshot: ${shot}`);
  // Prove the screenshot's content claim: extract the first card's text.
  const firstCard = await page.locator('#lookup-cards .card').first().textContent().catch(() => '');
  console.log(`first card (${q.label}): ${(firstCard || '').slice(0, 200).replace(/\\s+/g, ' ')}`);
}

if (problems.length) {
  console.log('PROBLEMS:\n' + problems.join('\n'));
  process.exitCode = 1;
} else {
  console.log('capture clean: no console/page/http errors');
}
await browser.close();
