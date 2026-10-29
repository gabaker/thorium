// Headless check that the vendored asciinema player initializes in the built book.
//
// Usage: python3 -m http.server 8111 --directory api/docs/book &
//        node api/docs/demo/check-player.mjs http://localhost:8111 users/hunts.html
//
// Asserts: no console errors, every .cast fetch returned 200, and at least one
// .asciinema-clip div grew an .ap-player child (the player's initialized marker).
import { createRequire } from 'module';

// resolve playwright out of the ui workspace regardless of the caller's cwd
const require = createRequire(new URL('../../../ui/package.json', import.meta.url));
const { chromium } = require('playwright');

const base = process.argv[2] ?? 'http://localhost:8111';
const page = process.argv[3] ?? 'users/hunts.html';

const browser = await chromium.launch();
const tab = await browser.newPage();

const consoleErrors = [];
const badResponses = [];
tab.on('console', (msg) => {
  if (msg.type() === 'error') consoleErrors.push(msg.text());
});
tab.on('response', (res) => {
  if (res.url().endsWith('.cast') && res.status() !== 200) {
    badResponses.push(`${res.status()} ${res.url()}`);
  }
});

await tab.goto(`${base}/${page}`, { waitUntil: 'load' });
// the init script runs on window load; give the player a beat to mount
await tab.waitForSelector('.asciinema-clip .ap-player', { timeout: 15000 });
const clips = await tab.locator('.asciinema-clip').count();
const players = await tab.locator('.asciinema-clip .ap-player').count();
await browser.close();

console.log(`clips=${clips} players=${players} consoleErrors=${consoleErrors.length} badCastFetches=${badResponses.length}`);
if (consoleErrors.length) console.error('console errors:', consoleErrors);
if (badResponses.length) console.error('bad cast fetches:', badResponses);
if (players < clips || players === 0 || consoleErrors.length || badResponses.length) {
  process.exit(1);
}
