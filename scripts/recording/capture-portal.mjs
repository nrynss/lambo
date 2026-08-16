// Capture the judge portal as video, by driving it the way a judge would.
//
// This does not screen-record. Playwright writes the video from the browser
// context itself, so nothing depends on the compositor: on a KDE Wayland
// session `ffmpeg -f x11grab` captures a black frame (XWayland's root window
// cannot see KWin's surfaces) and the xdg-desktop-portal ScreenCast request
// times out. Recording the context sidesteps both, and re-runs identically.
//
//   node capture-portal.mjs                      # against the live exhibit
//   PORTAL=http://127.0.0.1:7710 node capture-portal.mjs
//
// Output: evidence/cloudops-video/portal-<utc>.webm, plus a still of the
// recall result for the thumbnail.

import { chromium } from '@playwright/test';
import { mkdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, '..', '..');
const OUT = resolve(REPO, 'evidence', 'cloudops-video');
const PORTAL = process.env.PORTAL ?? 'https://lambo.nryn.dev';

// The queries a judge would actually type, in the order that tells the story:
// the shared pillar first, then the workload that depends on it.
const QUERIES = [
  'what depends on SG-Base-VPC',
  'can I delete the shared security group',
  'RDS-Lambo-Demo-DB',
];

// Slow enough to read on playback. The portal is a live reader, so these waits
// are also what lets the stats tiles refresh on camera.
const BEAT = 2500;

mkdirSync(OUT, { recursive: true });

const browser = await chromium.launch();
const context = await browser.newContext({
  viewport: { width: 1600, height: 900 },
  deviceScaleFactor: 2,
  recordVideo: { dir: OUT, size: { width: 1600, height: 900 } },
});
const page = await context.newPage();

const problems = [];
page.on('console', (m) => m.type() === 'error' && problems.push(m.text()));
page.on('requestfailed', (r) => problems.push(`${r.method()} ${r.url()}`));

console.log(`portal: ${PORTAL}`);
await page.goto(PORTAL, { waitUntil: 'networkidle', timeout: 60_000 });

// Fail loudly rather than recording a blank page: if the session id never
// renders, the portal is up but not talking to the store, and a silent video
// of an empty shell is worse than no video.
await page.waitForFunction(
  () => (document.querySelector('#session-id')?.textContent ?? '').trim().length > 0,
  { timeout: 30_000 },
);
const session = (await page.locator('#session-id').textContent())?.trim();
console.log(`session on page: ${session}`);
await page.waitForTimeout(BEAT);

for (const q of QUERIES) {
  console.log(`recall: ${q}`);
  // The button disables for the duration of a recall. On the exhibit that is
  // several seconds, because the query is embedded by a llama.cpp running on
  // two vCPUs, so wait for the previous one to finish rather than racing it.
  await page.waitForSelector('#recall-go:not([disabled])', { timeout: 90_000 });
  await page.fill('#recall-query', '');
  await page.type('#recall-query', q, { delay: 55, timeout: 60_000 }); // typing, on camera
  await page.click('#recall-go');
  await page
    .waitForFunction(
      () => {
        const out = (document.querySelector('#recall-out')?.textContent ?? '').trim();
        const busy = document.querySelector('#recall-go')?.disabled;
        return out.length > 0 && !busy;
      },
      { timeout: 90_000 },
    )
    .catch(() => problems.push(`no recall output for: ${q}`));
  await page.waitForTimeout(BEAT);
}

// Linger on the canonization feed, which is the part the submission argues
// about: status earned through an audited transition, not asserted.
await page.locator('#event-list').scrollIntoViewIfNeeded().catch(() => {});
await page.waitForTimeout(BEAT);

const stamp = new Date().toISOString().replace(/[:.]/g, '-');
await page.screenshot({ path: resolve(OUT, `portal-${stamp}.png`), fullPage: true });

await context.close(); // flushes the video
await browser.close();

if (problems.length) {
  console.log('\nproblems observed:');
  for (const p of problems) console.log(`  ${p}`);
}
console.log(`\nwrote video + still to evidence/cloudops-video/`);
