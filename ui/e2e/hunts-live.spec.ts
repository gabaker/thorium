import { test, expect, Page } from '@playwright/test';
import path from 'path';
import {
  authenticate,
  cancelHuntViaAPI,
  createEntityWithMetadata,
  createGroupViaAPI,
  deleteEntity,
  deleteGroupViaAPI,
  deleteSubmission,
  listEntityIdsByTag,
  loginViaUI,
  snapshot,
  TEST_PASS,
  TEST_USER,
  uploadFile,
} from './helpers';

// Live spec for the M4d hunt/rule-set UI against a real Thorium instance (THORIUM_API_URL):
// browse shells, the rule-set create/edit member flow, the hunt create page (Rules and
// RuleSet selectors), live lane progress to Completed via the details auto-refresh, the
// HuntId-filtered hits browse, hit details, and the cancel flow.
//
// Seeding deviates from the usual system/static-group guidance deliberately: everything
// lands in a dedicated throwaway group so the hunts' corpus (which is group-scoped) walks
// exactly this spec's samples — never other users' data on a shared dev cluster — and the
// group is deleted in afterAll. NOTE for operators: hit tags fire default-toolbox tag
// triggers on minithor, so run ~/.minithor/delete-stuck-reactions.sh after this suite.
const SCREENSHOT_DIR = path.join(import.meta.dirname, 'screenshots');

// unique suffix so repeated runs never collide
const RUN = Date.now();
const GROUP = `e2ehunts${RUN}`;
const MARKER = `thorium_e2e_marker_${RUN}`;
const MARKER_RULE = `e2e-marker-${RUN}`;
const NOMATCH_RULE = `e2e-nomatch-${RUN}`;
const RULESET = `e2e-ruleset-${RUN}`;

// the hunt UI flows build on each other (created hunt -> hits browse), so run serially
test.describe.configure({ mode: 'serial' });

test.describe('Hunts and Rule Sets — live UI flows', () => {
  let token: string;
  const ids = { markerRule: '', nomatchRule: '', ruleset: '', uiRuleset: '', hunt: '', cancelHunt: '' };
  const samples: { sha256: string; id: string }[] = [];

  test.beforeAll(async () => {
    token = await authenticate(TEST_USER, TEST_PASS);
    // a dedicated group makes the hunts' corpus exactly this spec's samples
    expect(await createGroupViaAPI(token, GROUP)).toBe(true);
    // two yara rules: one matching the marker samples, one matching nothing
    ids.markerRule = await createEntityWithMetadata(
      token,
      MARKER_RULE,
      'YaraRule',
      [GROUP],
      [
        { key: 'metadata[yara_rule]', value: `rule e2e_marker_${RUN} { strings: $m = "${MARKER}" condition: $m }` },
        { key: 'metadata[score]', value: 10 },
        { key: 'metadata[yara_applies_to][]', value: 'Samples' },
      ],
    );
    ids.nomatchRule = await createEntityWithMetadata(
      token,
      NOMATCH_RULE,
      'YaraRule',
      [GROUP],
      [
        { key: 'metadata[yara_rule]', value: `rule e2e_nomatch_${RUN} { strings: $m = "thorium_e2e_nomatch_${RUN}" condition: $m }` },
        { key: 'metadata[score]', value: 10 },
        { key: 'metadata[yara_applies_to][]', value: 'Samples' },
      ],
    );
    // a rule set holding both rules, for the RuleSet-selector hunt
    ids.ruleset = await createEntityWithMetadata(
      token,
      RULESET,
      'RuleSet',
      [GROUP],
      [
        { key: 'metadata[ruleset_rules][]', value: ids.markerRule },
        { key: 'metadata[ruleset_rules][]', value: ids.nomatchRule },
      ],
    );
    // five tiny unique samples; three carry the marker string the rule matches
    for (let i = 0; i < 5; i++) {
      const marked = i < 3 ? MARKER : 'no marker here';
      const content = Buffer.from(`thorium hunt e2e sample ${RUN}-${i}: ${marked}`);
      samples.push(await uploadFile(token, content, `e2e-hunt-${RUN}-${i}.txt`, [GROUP]));
    }
    // let the live scan path drain so the hunt's writes converge onto existing hits
    await new Promise((resolve) => setTimeout(resolve, 15000));
  });

  test.afterAll(async () => {
    // best-effort cleanup: cancel both hunts first so in-flight chunks stop writing hits
    // mid-sweep, then hits (listed per hunt by its HuntId tag — the RuleSet cancel hunt
    // can also land hits in its create-to-cancel window, so it is swept too), then the
    // hunts, rule sets, rules, submissions, and finally the group
    for (const hunt of [ids.hunt, ids.cancelHunt]) {
      if (hunt) await cancelHuntViaAPI(token, hunt).catch(() => {});
    }
    for (const hunt of [ids.hunt, ids.cancelHunt]) {
      if (!hunt) continue;
      try {
        const hits = await listEntityIdsByTag(token, 'YaraRuleHit', 'HuntId', hunt);
        for (const hit of hits) {
          await deleteEntity(token, hit).catch(() => {});
        }
      } catch {
        /* best effort */
      }
      await deleteEntity(token, hunt).catch(() => {});
    }
    for (const id of [ids.uiRuleset, ids.ruleset, ids.markerRule, ids.nomatchRule]) {
      if (id) await deleteEntity(token, id).catch(() => {});
    }
    for (const sample of samples) {
      if (sample.id) await deleteSubmission(token, sample.sha256, sample.id).catch(() => {});
    }
    await deleteGroupViaAPI(token, GROUP).catch(() => {});
  });

  // pick an option by typing into one of the page's react-select inputs (nth in DOM order)
  const pickSelectOption = async (page: Page, inputIdx: number, label: string) => {
    const input = page.locator('input[id*="react-select"]').nth(inputIdx);
    await input.click();
    await input.fill(label);
    await page.locator('[id*="react-select"][id*="option"]', { hasText: label }).first().click();
  };

  test('the rule set, hunt, and hit browse shells render', async ({ page }) => {
    await loginViaUI(page);
    for (const [route, title] of [
      ['/rulesets', 'Rule Sets'],
      ['/hunts', 'Hunts'],
      ['/rules/yara/hits', 'Yara Rule Hits'],
    ] as const) {
      await page.goto(route);
      await page.waitForLoadState('networkidle');
      // the nav sidebar carries the same label in a hidden node — assert a visible match
      await expect(page.getByText(title, { exact: true }).locator('visible=true').first()).toBeVisible({ timeout: 30000 });
    }
    // hits are machine-written: the hits browse page renders no create "+" button
    await expect(page.getByRole('button', { name: '+' })).toHaveCount(0);
    // the creatable pages do render it
    await page.goto('/hunts');
    await page.waitForLoadState('networkidle');
    await expect(page.getByRole('button', { name: '+' }).first()).toBeVisible();
    await snapshot(page, SCREENSHOT_DIR, 'hunts-browse-shell');
  });

  test('a rule set creates through the UI and edits its members', async ({ page }) => {
    await loginViaUI(page);
    await page.goto('/create/ruleset');
    await page.waitForLoadState('networkidle');
    await page.locator('input.form-control').first().fill(`${RULESET}-ui`);
    // groups picker is the page's first react-select, the member picker the second
    await pickSelectOption(page, 0, GROUP);
    await pickSelectOption(page, 1, MARKER_RULE);
    await pickSelectOption(page, 1, NOMATCH_RULE);
    await page.getByRole('button', { name: 'Create', exact: true }).click();
    await page.waitForURL((url) => url.pathname.startsWith('/ruleset/'), { timeout: 30000 });
    ids.uiRuleset = page.url().split('/ruleset/')[1];
    // the details page renders both hydrated members as links
    await expect(page.getByText('Rule Set', { exact: true })).toBeVisible({ timeout: 30000 });
    await expect(page.getByText(MARKER_RULE, { exact: false }).first()).toBeVisible();
    await expect(page.getByText(NOMATCH_RULE, { exact: false }).first()).toBeVisible();
    await snapshot(page, SCREENSHOT_DIR, 'ruleset-details');
    // edit: drop the marker rule from the membership (react-select exposes each chip's
    // remove control by accessible name) and save
    await page.locator('button.icon-btn').first().click();
    const removeMember = page.getByRole('button', { name: `Remove ${MARKER_RULE}` });
    await expect(removeMember).toBeVisible({ timeout: 30000 });
    await removeMember.click();
    await page.locator('button.icon-btn').nth(1).click();
    await page.waitForLoadState('networkidle');
    // back in read mode only the nomatch member remains
    await expect(page.getByText(NOMATCH_RULE, { exact: false }).first()).toBeVisible({ timeout: 30000 });
    await expect(page.locator('a').filter({ hasText: MARKER_RULE })).toHaveCount(0);
    await snapshot(page, SCREENSHOT_DIR, 'ruleset-details-member-removed');
  });

  test('a Rules-selector hunt runs to Completed with live lane progress and browsable hits', async ({ page }) => {
    // the completion poll alone can take a couple minutes on a loaded dev cluster
    test.setTimeout(240_000);
    await loginViaUI(page);
    await page.goto('/create/hunt');
    await page.waitForLoadState('networkidle');
    await page.locator('input.form-control').first().fill(`e2e-hunt-${RUN}`);
    // groups picker is the first react-select, the explicit-rules picker the second
    await pickSelectOption(page, 0, GROUP);
    await pickSelectOption(page, 1, MARKER_RULE);
    await snapshot(page, SCREENSHOT_DIR, 'hunt-create-filled');
    await page.getByRole('button', { name: 'Create', exact: true }).click();
    await page.waitForURL((url) => url.pathname.startsWith('/hunt/'), { timeout: 30000 });
    ids.hunt = page.url().split('/hunt/')[1];
    // lane detail is collapsed behind the Lane progress disclosure by default
    const laneToggle = page.getByText('Lane progress (4 lanes)', { exact: true });
    await expect(laneToggle).toBeVisible({ timeout: 30000 });
    await expect(page.getByText('Lane 0', { exact: true })).toBeHidden();
    // expand it: the four default lanes render inside
    await laneToggle.click();
    await expect(page.getByText('Lane 0', { exact: true })).toBeVisible();
    await expect(page.getByText('Lane 3', { exact: true })).toBeVisible();
    await snapshot(page, SCREENSHOT_DIR, 'hunt-details-created');
    // the 10s auto-refresh carries the page to Completed without a reload
    await expect(page.locator('.badge').filter({ hasText: 'Completed' }).first()).toBeVisible({ timeout: 120000 });
    await expect(page.getByText('written:', { exact: false }).first()).toBeVisible();
    await snapshot(page, SCREENSHOT_DIR, 'hunt-details-completed');
    // the findings card lists the hits inline (the Completed transition refetched it);
    // count by the hit-details row links — the rule-name tag chips in each row also
    // carry the rule name, so match on the href shape
    const inlineHits = page.locator('[data-testid="hunt-hits"] a[href*="/rules/yara/hit/"]').filter({ hasText: MARKER_RULE });
    await expect(inlineHits).toHaveCount(3, { timeout: 60000 });
    await snapshot(page, SCREENSHOT_DIR, 'hunt-hits-inline');
    // the toolbar's crosshairs button (top action row, next to the dashboard button)
    // opens the full HuntId-filtered hit browse
    await page.getByRole('button', { name: 'View hunt hits' }).click();
    await page.waitForURL((url) => url.pathname.startsWith('/rules/yara/hits'), { timeout: 15000 });
    await page.waitForLoadState('networkidle');
    // the same three findings on the browse page
    await expect(page.locator('a[href*="/rules/yara/hit/"]').filter({ hasText: MARKER_RULE })).toHaveCount(3, { timeout: 60000 });
    await snapshot(page, SCREENSHOT_DIR, 'hunt-hits-browse');
    // a hit details page links the hit back to its rule and sample
    await page.locator('a[href*="/rules/yara/hit/"]').filter({ hasText: MARKER_RULE }).first().click();
    await page.waitForURL((url) => url.pathname.startsWith('/rules/yara/hit/'), { timeout: 15000 });
    await expect(page.getByText('Rule Hash', { exact: true })).toBeVisible({ timeout: 30000 });
    await expect(page.locator('a').filter({ hasText: MARKER_RULE }).first()).toBeVisible();
    await expect(page.getByText('$m', { exact: false }).first()).toBeVisible();
    await snapshot(page, SCREENSHOT_DIR, 'hunt-hit-details');
  });

  test('a RuleSet-selector hunt creates and cancels from its details page', async ({ page }) => {
    await loginViaUI(page);
    await page.goto('/create/hunt');
    await page.waitForLoadState('networkidle');
    await page.locator('input.form-control').first().fill(`e2e-hunt-cancel-${RUN}`);
    await pickSelectOption(page, 0, GROUP);
    // selects in DOM order: engine, selector mode, then the rule set picker it reveals
    await page.locator('select').nth(1).selectOption('RuleSet');
    await page.locator('select').nth(2).selectOption(ids.ruleset);
    await page.getByRole('button', { name: 'Create', exact: true }).click();
    await page.waitForURL((url) => url.pathname.startsWith('/hunt/'), { timeout: 30000 });
    ids.cancelHunt = page.url().split('/hunt/')[1];
    // cancel through the confirm modal; the refresh flips the badge without a reload
    await page.getByRole('button', { name: 'Cancel hunt' }).click();
    await page.getByRole('button', { name: 'Confirm', exact: true }).click();
    await expect(page.locator('.badge').filter({ hasText: 'Cancelled' }).first()).toBeVisible({ timeout: 30000 });
    await expect(page.getByRole('button', { name: 'Cancel hunt' })).toHaveCount(0);
    await snapshot(page, SCREENSHOT_DIR, 'hunt-details-cancelled');
  });
});
