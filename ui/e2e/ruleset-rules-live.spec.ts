import { test, expect, Page } from '@playwright/test';
import path from 'path';
import {
  authenticate,
  createEntityWithMetadata,
  createGroupViaAPI,
  deleteEntity,
  deleteGroupViaAPI,
  loginViaUI,
  snapshot,
  TEST_PASS,
  TEST_USER,
} from './helpers';

// Live spec for the ruleset details page's member Rules section against a real Thorium
// instance (THORIUM_API_URL): the section replaces the Associations graph on rulesets
// only, pages the hydrated member list, and the omnibar's free text narrows it by name.
// Seeds into a dedicated throwaway group like hunts-live.spec.ts; no hunts run, so no
// hit tags (and no toolbox trigger reactions) are ever created.
const SCREENSHOT_DIR = path.join(import.meta.dirname, 'screenshots');

// unique suffix so repeated runs never collide
const RUN = Date.now();
const GROUP = `e2errules${RUN}`;
// 28 yara + 2 sigma members: two pages at the default 25-per-page limit
const YARA_COUNT = 28;
const SIGMA_COUNT = 2;
const RULESET = `e2e-rr-set-${RUN}`;

// a sigma rule body the API's create-time validation accepts
const SIGMA_RULE = ['title: e2e rr sigma', 'logsource:', '  product: test', 'detection:', '  sel:', '    foo: bar', '  condition: sel'].join(
  '\n',
);

// the member rows render as details links named after the seeded rules
const memberRows = (page: Page) => page.getByRole('link', { name: new RegExp(`e2e-rr-(yara|sigma)-\\d+-${RUN}`) });

// open the seeded ruleset's details page and wait for the Rules section to land
const openRuleset = async (page: Page, id: string) => {
  await loginViaUI(page);
  await page.goto(`/ruleset/${id}`);
  await expect(page.getByText(`30 of 30 rules`)).toBeVisible({ timeout: 30000 });
};

// the section's flows build on the same seeded ruleset, so run serially
test.describe.configure({ mode: 'serial' });

test.describe('Ruleset member rules section — live UI flows', () => {
  let token: string;
  const ids = { ruleset: '', rules: [] as string[], firstYara: '', firstSigma: '' };

  test.beforeAll(async () => {
    token = await authenticate(TEST_USER, TEST_PASS);
    // a dedicated group keeps the seeds off other users' browse pages on a shared cluster
    expect(await createGroupViaAPI(token, GROUP)).toBe(true);
    // seed enough members to force a second page at the default 25-per-page limit
    for (let i = 0; i < YARA_COUNT; i++) {
      const num = String(i).padStart(2, '0');
      const id = await createEntityWithMetadata(token, `e2e-rr-yara-${num}-${RUN}`, 'YaraRule', [GROUP], [
        { key: 'metadata[yara_rule]', value: `rule e2e_rr_yara_${num}_${RUN} { strings: $m = "e2e_rr_${RUN}_${num}" condition: $m }` },
        { key: 'metadata[yara_applies_to][]', value: 'Samples' },
      ]);
      ids.rules.push(id);
      if (!ids.firstYara) ids.firstYara = id;
    }
    for (let i = 0; i < SIGMA_COUNT; i++) {
      const num = String(i).padStart(2, '0');
      const id = await createEntityWithMetadata(token, `e2e-rr-sigma-${num}-${RUN}`, 'SigmaRule', [GROUP], [
        { key: 'metadata[sigma_rule]', value: SIGMA_RULE },
        { key: 'metadata[sigma_applies_to][]', value: 'WindowsProcesses' },
      ]);
      ids.rules.push(id);
      if (!ids.firstSigma) ids.firstSigma = id;
    }
    // the ruleset holding every seeded rule as a member
    ids.ruleset = await createEntityWithMetadata(
      token,
      RULESET,
      'RuleSet',
      [GROUP],
      ids.rules.map((id) => ({ key: 'metadata[ruleset_rules][]', value: id })),
    );
  });

  test.afterAll(async () => {
    // rule set first, then members, then the group
    if (ids.ruleset) await deleteEntity(token, ids.ruleset);
    for (const id of ids.rules) {
      await deleteEntity(token, id);
    }
    await deleteGroupViaAPI(token, GROUP);
  });

  test('the rules section replaces the associations graph and pages the members', async ({ page }) => {
    await openRuleset(page, ids.ruleset);
    // the section owns the bottom card: no associations graph renders on a ruleset
    await expect(page.getByText('Rules', { exact: true }).first()).toBeVisible();
    await expect(page.getByText('Associations', { exact: true })).toHaveCount(0);
    // the metadata card shows the member count instead of a badge wall
    await expect(page.getByText('30', { exact: true }).first()).toBeVisible();
    // page one holds the default 25 rows, name-sorted so the sigma members lead
    await expect(memberRows(page)).toHaveCount(25);
    await expect(memberRows(page).first()).toHaveText(new RegExp(`e2e-rr-sigma-00-${RUN}`));
    await snapshot(page, SCREENSHOT_DIR, 'ruleset-rules-section');
    // the remaining five members live on the second page
    await page.getByText('Next', { exact: true }).click();
    await expect(memberRows(page)).toHaveCount(5);
    await page.getByText('Back', { exact: true }).click();
    await expect(memberRows(page)).toHaveCount(25);
  });

  test('omnibar free text narrows the members and rows link to the rule pages', async ({ page }) => {
    await openRuleset(page, ids.ruleset);
    const omni = page.getByPlaceholder('Filter rules...');
    await omni.click();
    await omni.fill('sigma');
    await omni.press('Enter');
    // the substring filter leaves only the two sigma members (and the count line says so)
    await expect(page.getByText('2 of 30 rules')).toBeVisible();
    await expect(memberRows(page)).toHaveCount(2);
    await snapshot(page, SCREENSHOT_DIR, 'ruleset-rules-filtered');
    // a member row links to its engine's details page
    await memberRows(page).first().click();
    await page.waitForURL((url) => url.pathname.startsWith('/rules/sigma/'), { timeout: 15000 });
  });

  test('a non-matching filter shows the empty banner', async ({ page }) => {
    await openRuleset(page, ids.ruleset);
    const omni = page.getByPlaceholder('Filter rules...');
    await omni.click();
    await omni.fill('no-such-rule-xyz');
    await omni.press('Enter');
    await expect(page.getByText('No Matching Rules Found')).toBeVisible();
  });

  test('rule details pages keep the default associations graph', async ({ page }) => {
    await loginViaUI(page);
    await page.goto(`/rules/yara/${ids.firstYara}`);
    // the override is ruleset-only: a yara rule page still renders the graph card
    await expect(page.getByText('Associations', { exact: true })).toBeVisible({ timeout: 30000 });
  });
});
