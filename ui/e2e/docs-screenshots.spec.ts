import { test, expect, Page } from '@playwright/test';
import fs from 'fs';
import path from 'path';
import {
  authenticate,
  createEntityWithMetadata,
  createGroupViaAPI,
  getUserTheme,
  listEntityIdsByTag,
  loginViaUI,
  setEditorContent,
  setUserTheme,
  TEST_PASS,
  TEST_USER,
  uploadFile,
  waitForEditor,
} from './helpers';

// Capture spec for the user-book screenshots of the rules/rule-sets/hunts docs pages
// (api/docs/src/users/{yara_rules,sigma_rules,rule_sets,hunts}.md). It drives a LIVE
// Thorium instance (THORIUM_API_URL) and writes viewport PNGs into e2e/screenshots/docs/.
//
// It runs in two halves so a larger corpus (e.g. a /bin upload) and the thorctl smoke
// tests can land between them:
//   npx playwright test docs-screenshots --grep "docs shots: rules"
//   npx playwright test docs-screenshots --grep "docs shots: hunts"
// The rules half seeds the demo group/rules/rule set and hands their ids to the hunts
// half through STATE_FILE.
//
// Seeding deviates from the usual throwaway-name guidance deliberately: the docs art
// must show presentable names (no timestamp suffixes), so this spec does NO cleanup —
// the capture session's final sweep deletes the whole demo group and everything in it.
// Rerunning without that sweep duplicates the demo entities (names are not unique).
// NOTE for operators: hit tags fire default-toolbox tag triggers on minithor, so run
// ~/.minithor/delete-stuck-reactions.sh after the session.
const SCREENSHOT_DIR = path.join(import.meta.dirname, 'screenshots', 'docs');
const STATE_FILE = '/tmp/thorium-docs-capture/state.json';

const GROUP = 'demolab';
const MARKER = 'THORIUM-DEMO-C2-BEACON';
const MARKER_RULE = 'demo_downloader_c2';
const NOMATCH_RULE = 'demo_overlay_check';
const SIGMA_RULE = 'demo_susp_process_spawn';
const RULESET = 'demo-triage-rules';
const HUNT = 'demo-history-sweep';
const CANCEL_HUNT = 'demo-ruleset-sweep';

const MARKER_RULE_SOURCE = `rule demo_downloader_c2
{
    meta:
        author = "Thorium Demo"
        description = "Detects the demo downloader's hardcoded C2 beacon marker"
    strings:
        $c2 = "${MARKER}"
    condition:
        $c2
}`;

const NOMATCH_RULE_SOURCE = `rule demo_overlay_check
{
    meta:
        author = "Thorium Demo"
        description = "Detects the demo overlay marker"
    strings:
        $overlay = "THORIUM-DEMO-OVERLAY-9f3a"
    condition:
        $overlay
}`;

const SIGMA_RULE_SOURCE = `title: Demo Suspicious Process Spawn
status: experimental
description: Detects a word processor spawning a shell, the demo lateral-movement tell
author: Thorium Demo
level: high
logsource:
  product: windows
  category: process_creation
detection:
  selection:
    ParentImage|endswith: '\\winword.exe'
    Image|endswith: '\\powershell.exe'
  condition: selection`;

interface CaptureState {
  markerRule: string;
  nomatchRule: string;
  sigmaRule: string;
  ruleset: string;
  markerShas: string[];
  priorTheme: string;
}

// docs art renders at width="800"; capture at 1280x800 @2x so it downscales crisply.
// colorScheme makes the pre-login pages and an 'Automatic' user theme resolve to dark,
// matching the book's existing dark-mode screenshots; the beforeAll theme PATCH covers
// users whose stored theme is explicitly light.
test.use({ viewport: { width: 1280, height: 800 }, deviceScaleFactor: 2, colorScheme: 'dark' });

// take a viewport screenshot, optionally scrolling a region into view first
const shoot = async (page: Page, name: string, scrollTo?: string) => {
  if (scrollTo) {
    await page.locator(scrollTo).first().scrollIntoViewIfNeeded();
    await page.waitForTimeout(300);
  }
  await page.screenshot({ path: path.join(SCREENSHOT_DIR, `${name}.png`), fullPage: false });
};

// settle a page and snap it back to the top: SPA navigation preserves the previous
// page's scroll position, which otherwise beheads the next screenshot
const settleTop = async (page: Page) => {
  await page.waitForLoadState('networkidle');
  await page.evaluate(() => window.scrollTo(0, 0));
  await page.waitForTimeout(400);
};

// pick an option by typing into one of the page's react-select inputs (nth in DOM order)
const pickSelectOption = async (page: Page, inputIdx: number, label: string) => {
  const input = page.locator('input[id*="react-select"]').nth(inputIdx);
  await input.click();
  await input.fill(label);
  await page.locator('[id*="react-select"][id*="option"]', { hasText: label }).first().click();
};

test.describe.configure({ mode: 'serial' });

test.describe('docs shots: rules', () => {
  let token: string;
  const state: CaptureState = {
    markerRule: '',
    nomatchRule: '',
    sigmaRule: '',
    ruleset: '',
    markerShas: [],
    priorTheme: 'Automatic',
  };
  let markerRuleCreatedAt = 0;
  let anyFailed = false;

  test.beforeAll(async () => {
    fs.mkdirSync(SCREENSHOT_DIR, { recursive: true });
    fs.mkdirSync(path.dirname(STATE_FILE), { recursive: true });
    token = await authenticate(TEST_USER, TEST_PASS);
    // capture in dark mode regardless of the user's stored theme; the prior theme
    // rides through STATE_FILE so the hunts half can restore it when the session ends
    state.priorTheme = await getUserTheme(token);
    await setUserTheme(token, 'Dark');
    // a dedicated demo group bounds the hunts' corpus to this session's samples;
    // tolerate a pre-existing group so a crashed run can resume
    await createGroupViaAPI(token, GROUP).catch(() => {});
    // the no-match rule never appears in a screenshot, so seed it via the API
    state.nomatchRule = await createEntityWithMetadata(
      token,
      NOMATCH_RULE,
      'YaraRule',
      [GROUP],
      [
        { key: 'metadata[yara_rule]', value: NOMATCH_RULE_SOURCE },
        { key: 'metadata[score]', value: 10 },
        { key: 'metadata[yara_applies_to][]', value: 'Samples' },
      ],
    );
  });

  // track failures so an aborted session can put the user's theme back: in serial
  // mode a failed rules half means the hunts half (and its restore) never runs
  test.afterEach(async ({}, testInfo) => {
    if (testInfo.status !== testInfo.expectedStatus) {
      anyFailed = true;
    }
  });

  test.afterAll(async () => {
    // hand the seeded ids to the hunts half (and the clip-recording phase)
    fs.writeFileSync(STATE_FILE, JSON.stringify(state, null, 2));
    // the session continues in dark mode only if this half passed
    if (anyFailed) {
      await setUserTheme(token, state.priorTheme || 'Automatic').catch(() => {});
    }
  });

  test('yara rule create form and details', async ({ page }) => {
    await loginViaUI(page);
    await page.goto('/create/rules/yara');
    await page.waitForLoadState('networkidle');
    // name, group, rule source, applies-to, score — the filled form is the shot
    await page.locator('input.form-control').first().fill(MARKER_RULE);
    await pickSelectOption(page, 0, GROUP);
    await pickSelectOption(page, 1, 'Samples');
    await waitForEditor(page);
    await setEditorContent(page, MARKER_RULE_SOURCE);
    const score = page.locator('input[type="number"]').first();
    await score.fill('50');
    // one Flag action so the actions card renders populated
    const addFlag = page.getByRole('button', { name: /add flag action/i });
    if (await addFlag.count()) {
      await addFlag.first().click();
      await page.getByPlaceholder('Reason for this flag (required)').fill('Beacon marker identifies the demo downloader family');
    }
    // a description rounds out the form (the description box is the page's last textarea)
    await page.locator('textarea').last().fill('Flags the demo downloader beacon so triage can pivot fast');
    // let the live syntax check settle so no transient lint markers land in the art
    await page.waitForTimeout(1000);
    await settleTop(page);
    await shoot(page, 'yara-rule-create');
    await page.getByRole('button', { name: 'Create', exact: true }).click();
    await page.waitForURL((url) => url.pathname.startsWith('/rules/yara/'), { timeout: 30000 });
    state.markerRule = page.url().split('/rules/yara/')[1];
    markerRuleCreatedAt = Date.now();
    // the details page shows the source plus the intrinsic tags
    await expect(page.getByText('YaraRuleHash', { exact: false }).first()).toBeVisible({ timeout: 30000 });
    await settleTop(page);
    await shoot(page, 'yara-rule-details');
  });

  test('sigma rule create form and details', async ({ page }) => {
    await loginViaUI(page);
    await page.goto('/create/rules/sigma');
    await page.waitForLoadState('networkidle');
    await page.locator('input.form-control').first().fill(SIGMA_RULE);
    await pickSelectOption(page, 0, GROUP);
    await pickSelectOption(page, 1, 'WindowsProcesses');
    await waitForEditor(page);
    await setEditorContent(page, SIGMA_RULE_SOURCE);
    await page.waitForTimeout(1000);
    await settleTop(page);
    await shoot(page, 'sigma-rule-create');
    await page.getByRole('button', { name: 'Create', exact: true }).click();
    await page.waitForURL((url) => url.pathname.startsWith('/rules/sigma/'), { timeout: 30000 });
    state.sigmaRule = page.url().split('/rules/sigma/')[1];
    await expect(page.getByText('SigmaRuleLevel', { exact: false }).first()).toBeVisible({ timeout: 30000 });
    await settleTop(page);
    await shoot(page, 'sigma-rule-details');
  });

  test('rule set create form and details', async ({ page }) => {
    await loginViaUI(page);
    await page.goto('/create/ruleset');
    await page.waitForLoadState('networkidle');
    await page.locator('input.form-control').first().fill(RULESET);
    // groups picker is the page's first react-select, the member picker the second;
    // mixed engines (two yara + one sigma) show rule sets are engine-agnostic
    await pickSelectOption(page, 0, GROUP);
    await pickSelectOption(page, 1, MARKER_RULE);
    await pickSelectOption(page, 1, NOMATCH_RULE);
    await pickSelectOption(page, 1, SIGMA_RULE);
    await settleTop(page);
    await shoot(page, 'ruleset-create');
    await page.getByRole('button', { name: 'Create', exact: true }).click();
    await page.waitForURL((url) => url.pathname.startsWith('/ruleset/'), { timeout: 30000 });
    state.ruleset = page.url().split('/ruleset/')[1];
    // anchor on the details page's Type row so a stale create-form frame can't be shot
    await expect(page.getByText('Rule Set', { exact: true })).toBeVisible({ timeout: 30000 });
    await expect(page.getByText(MARKER_RULE, { exact: false }).first()).toBeVisible({ timeout: 30000 });
    await settleTop(page);
    await shoot(page, 'ruleset-details');
  });

  test('marker samples live-scan into hit tags and hit details', async ({ page }) => {
    // give the live hits time to land: ruleset-rebuild debounce plus scan plus writes
    test.setTimeout(300_000);
    // the compiled ruleset rebuild is debounced (~5s poll + up to 30s debounce + compile),
    // so wait out the remainder of 45s since the marker rule was created before uploading
    const elapsed = Date.now() - markerRuleCreatedAt;
    if (elapsed < 45_000) {
      await new Promise((resolve) => setTimeout(resolve, 45_000 - elapsed));
    }
    // three marker samples the demo rule matches
    for (let i = 0; i < 3; i++) {
      const content = Buffer.from(`thorium demo sample ${i}\ndropper build ${i}\n${MARKER}\n`);
      const sample = await uploadFile(token, content, `demo-dropper-${i}.bin`, [GROUP]);
      state.markerShas.push(sample.sha256);
    }
    // poll until the live scan path writes at least one hit for the marker rule
    const deadline = Date.now() + 180_000;
    let hits: string[] = [];
    while (Date.now() < deadline) {
      hits = await listEntityIdsByTag(token, 'YaraRuleHit', 'YaraHitRuleName', MARKER_RULE);
      if (hits.length > 0) break;
      await new Promise((resolve) => setTimeout(resolve, 5000));
    }
    expect(hits.length).toBeGreaterThan(0);
    // the sample's file details page renders the YaraRuleHits danger tag
    await loginViaUI(page);
    await page.goto(`/file/${state.markerShas[0]}`);
    await page.waitForLoadState('networkidle');
    await expect(page.getByText('YaraRuleHits', { exact: false }).first()).toBeVisible({ timeout: 60000 });
    await shoot(page, 'yara-hit-file-tags', 'text=YaraRuleHits');
    // and the hit entity's details page carries the attribution facts
    await page.goto(`/rules/yara/hit/${hits[0]}`);
    await page.waitForLoadState('networkidle');
    await expect(page.getByText('Rule Hash', { exact: true })).toBeVisible({ timeout: 30000 });
    await shoot(page, 'yara-hit-details');
  });
});

test.describe('docs shots: hunts', () => {
  let state: CaptureState;

  test.beforeAll(async () => {
    // the rules half must have run first (its ids seed the hunt selectors)
    state = JSON.parse(fs.readFileSync(STATE_FILE, 'utf-8'));
    // re-assert dark mode in case something changed the theme between the halves
    const token = await authenticate(TEST_USER, TEST_PASS);
    await setUserTheme(token, 'Dark');
  });

  test.afterAll(async () => {
    // the capture session is over, so restore the user's prior theme
    const token = await authenticate(TEST_USER, TEST_PASS);
    await setUserTheme(token, state?.priorTheme || 'Automatic');
  });

  test('hunt create form, lane progress, completion, and hits browse', async ({ page }) => {
    // a /bin-sized corpus takes the busy lane several paced chunks to walk
    test.setTimeout(600_000);
    await loginViaUI(page);
    await page.goto('/create/hunt');
    await page.waitForLoadState('networkidle');
    await page.locator('input.form-control').first().fill(HUNT);
    // groups picker is the first react-select, the explicit-rules picker the second
    await pickSelectOption(page, 0, GROUP);
    await pickSelectOption(page, 1, MARKER_RULE);
    await pickSelectOption(page, 1, NOMATCH_RULE);
    await shoot(page, 'hunt-create');
    await page.getByRole('button', { name: 'Create', exact: true }).click();
    await page.waitForURL((url) => url.pathname.startsWith('/hunt/'), { timeout: 30000 });
    await expect(page.getByText(/Lane progress \(/)).toBeVisible({ timeout: 30000 });
    // reload-poll every 2s (the 10s auto-refresh can skip the window) until the hunt is
    // Running with some lane progress on screen, then keep polling to Completed
    let shotRunning = false;
    const deadline = Date.now() + 480_000;
    while (Date.now() < deadline) {
      const badge = page.locator('.badge').first();
      const label = (await badge.textContent().catch(() => '')) ?? '';
      if (!shotRunning && label.includes('Running')) {
        // the docs explain lanes, so expand the collapsed lane disclosure before the
        // shot (each reload above re-collapses it)
        await page.getByText(/Lane progress \(/).click();
        await shoot(page, 'hunt-details-running');
        shotRunning = true;
      }
      if (label.includes('Completed')) break;
      await page.waitForTimeout(2000);
      await page.reload();
      await page.waitForLoadState('networkidle');
    }
    await expect(page.locator('.badge').filter({ hasText: 'Completed' }).first()).toBeVisible({ timeout: 30000 });
    await shoot(page, 'hunt-details-completed');
    // the findings section shows the hits inline on its default tab; it sits below the
    // fold, so scroll to the tabs-plus-active-panel container by its testid once the
    // hit rows land
    await expect(page.locator('[data-testid="hunt-hits"] a[href*="/rules/yara/hit/"]').first()).toBeVisible({ timeout: 60000 });
    await shoot(page, 'hunt-hits-inline', '[data-testid="hunt-findings"]');
    // the Frozen Rules tab swaps the panels in place (regex name: the tab's count badge
    // joins its accessible name); shoot the same container so the strip stays in frame
    await page.getByRole('tab', { name: /Frozen Rules/ }).click();
    await expect(page.getByText('of 2 rules', { exact: false })).toBeVisible({ timeout: 30000 });
    await shoot(page, 'hunt-frozen-rules', '[data-testid="hunt-findings"]');
    // back on the hits tab; the toolbar's crosshairs button at the top of the page
    // lands on the HuntId-filtered hit browse
    await page.getByRole('tab', { name: 'Hits' }).click();
    await page.getByRole('button', { name: 'View hunt hits' }).click();
    await page.waitForURL((url) => url.pathname.startsWith('/rules/yara/hits'), { timeout: 15000 });
    await page.waitForLoadState('networkidle');
    await expect(page.locator('a[href*="/rules/yara/hit/"]').first()).toBeVisible({ timeout: 60000 });
    await shoot(page, 'hunt-hits-browse');
  });

  test('ruleset-selector hunt cancels and the hunts browse renders both', async ({ page }) => {
    await loginViaUI(page);
    await page.goto('/create/hunt');
    await page.waitForLoadState('networkidle');
    await page.locator('input.form-control').first().fill(CANCEL_HUNT);
    await pickSelectOption(page, 0, GROUP);
    // selects in DOM order: engine, selector mode, then the rule set picker it reveals
    await page.locator('select').nth(1).selectOption('RuleSet');
    await page.locator('select').nth(2).selectOption(state.ruleset);
    await page.getByRole('button', { name: 'Create', exact: true }).click();
    await page.waitForURL((url) => url.pathname.startsWith('/hunt/'), { timeout: 30000 });
    // cancel through the confirm modal; the refresh flips the badge without a reload
    await page.getByRole('button', { name: 'Cancel hunt' }).click();
    await page.getByRole('button', { name: 'Confirm', exact: true }).click();
    await expect(page.locator('.badge').filter({ hasText: 'Cancelled' }).first()).toBeVisible({ timeout: 30000 });
    await shoot(page, 'hunt-details-cancelled');
    // the hunts browse shows the Completed and Cancelled demo hunts side by side
    await page.goto('/hunts');
    await page.waitForLoadState('networkidle');
    await expect(page.getByText(HUNT, { exact: false }).first()).toBeVisible({ timeout: 30000 });
    await shoot(page, 'hunts-browse');
  });
});

test.describe('docs shots: hunt status', () => {
  let token: string;
  let priorTheme = 'Automatic';

  test.beforeAll(async () => {
    fs.mkdirSync(SCREENSHOT_DIR, { recursive: true });
    token = await authenticate(TEST_USER, TEST_PASS);
    // capture in dark mode regardless of the stored theme and restore after
    priorTheme = await getUserTheme(token);
    await setUserTheme(token, 'Dark');
  });

  test.afterAll(async () => {
    await setUserTheme(token, priorTheme);
  });

  // the admin hunt status page for api/docs/src/admins/hunt_status.md; expects a live
  // instance with the m5-status-harness demo lab running (an ongoing hunt + snapshots)
  test('hunt status page with a running hunt and live charts', async ({ page }) => {
    await loginViaUI(page);
    await page.goto('/hunt-status');
    await settleTop(page);
    // wait for the census tables and at least one chart to carry real data
    await expect(page.getByText('Active hunts vs global quota')).toBeVisible();
    await expect(page.getByRole('heading', { name: 'Scans per second' })).toBeVisible();
    // the demo hunt must sit in the ongoing table as a Running row before the
    // shot (a terminal hunt of the same name in Recently finished must not
    // satisfy this wait, so require the Running badge in the same row)
    const runningRow = page.locator('tr', { hasText: 'corn-status-hunt' }).filter({ hasText: 'Running' });
    await expect(runningRow.first()).toBeVisible({ timeout: 60000 });
    // let a poll tick land so the tiles and charts carry the hunt's activity
    await page.waitForTimeout(2500);
    await shoot(page, 'hunt-status');
    // a second shot scrolled to the census tables for the docs table section;
    // wheel-scroll over the content pane since the SPA scrolls a container and
    // a partially visible header already satisfies scrollIntoViewIfNeeded
    await page.mouse.move(960, 500);
    await page.mouse.wheel(0, 1400);
    await page.waitForTimeout(400);
    await shoot(page, 'hunt-status-census');
  });
});
