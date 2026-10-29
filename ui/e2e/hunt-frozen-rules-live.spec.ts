import { test, expect, Page } from '@playwright/test';
import path from 'path';
import {
  authenticate,
  cancelHuntViaAPI,
  createEntityWithMetadata,
  createGroupViaAPI,
  createHuntViaAPI,
  deleteEntity,
  deleteGroupViaAPI,
  loginViaUI,
  snapshot,
  TEST_PASS,
  TEST_USER,
} from './helpers';

// Live spec for the hunt details page's Frozen Rules panel against a real Thorium
// instance (THORIUM_API_URL): the findings section replaces the Associations graph on
// hunts only and shows the Hits tab by default, the Frozen Rules tab swaps it to the
// frozen rules panel, which drains and pages the immutable snapshot, the omnibar's free
// text narrows it by name or id, and a rule deleted after the freeze renders id-only and
// linkless. Seeds into a dedicated throwaway group like hunts-live.spec.ts; the group
// holds no files, so the hunt completes without scanning and never writes hits (or
// toolbox trigger noise).
const SCREENSHOT_DIR = path.join(import.meta.dirname, 'screenshots');

// unique suffix so repeated runs never collide
const RUN = Date.now();
const GROUP = `e2ehfr${RUN}`;
// 30 yara rules: two pages at the default 25-per-page limit
const RULE_COUNT = 30;
const HUNT = `e2e-hfr-hunt-${RUN}`;

// the frozen rule rows render as details links named after the seeded rules
const ruleRows = (page: Page) => page.getByRole('link', { name: new RegExp(`e2e-hfr-yara-\\d+-${RUN}`) });

// open the seeded hunt's details page; the findings section lands on the Hits tab
const openHunt = async (page: Page, id: string) => {
  await loginViaUI(page);
  await page.goto(`/hunt/${id}`);
  await expect(page.getByTestId('hunt-hits')).toBeVisible({ timeout: 30000 });
};

// switch the findings section to the Frozen Rules tab and wait for the snapshot to
// drain (regex name: the tab's count badge joins its accessible name)
const openFrozenRules = async (page: Page, id: string) => {
  await openHunt(page, id);
  await page.getByRole('tab', { name: /Frozen Rules/ }).click();
  await expect(page.getByText(`${RULE_COUNT} of ${RULE_COUNT} rules`)).toBeVisible({ timeout: 30000 });
};

// the section's flows build on the same seeded hunt, so run serially
test.describe.configure({ mode: 'serial' });

test.describe('Hunt frozen rules section — live UI flows', () => {
  let token: string;
  const ids = { hunt: '', rules: [] as string[], deletedRule: '' };

  test.beforeAll(async () => {
    token = await authenticate(TEST_USER, TEST_PASS);
    // a dedicated group with no files bounds the hunt's corpus to nothing: the hunt
    // freezes its rule list and completes without scanning anything
    expect(await createGroupViaAPI(token, GROUP)).toBe(true);
    // seed enough rules to force a second page at the default 25-per-page limit
    for (let i = 0; i < RULE_COUNT; i++) {
      const num = String(i).padStart(2, '0');
      const id = await createEntityWithMetadata(
        token,
        `e2e-hfr-yara-${num}-${RUN}`,
        'YaraRule',
        [GROUP],
        [
          { key: 'metadata[yara_rule]', value: `rule e2e_hfr_yara_${num}_${RUN} { strings: $m = "e2e_hfr_${RUN}_${num}" condition: $m }` },
          { key: 'metadata[yara_applies_to][]', value: 'Samples' },
        ],
      );
      ids.rules.push(id);
    }
    // the hunt freezes every seeded rule into its immutable snapshot
    ids.hunt = await createHuntViaAPI(token, HUNT, [GROUP], ids.rules);
  });

  test.afterAll(async () => {
    // best-effort cleanup: cancel the hunt first (a 400 on a terminal hunt is fine),
    // then the hunt, the surviving rules, and finally the group
    if (ids.hunt) {
      await cancelHuntViaAPI(token, ids.hunt).catch(() => {});
      await deleteEntity(token, ids.hunt).catch(() => {});
    }
    for (const id of ids.rules) {
      // the deleted-row flow already removed one rule mid-suite
      if (id === ids.deletedRule) continue;
      await deleteEntity(token, id).catch(() => {});
    }
    await deleteGroupViaAPI(token, GROUP).catch(() => {});
  });

  test('hits are the default tab and the frozen rules tab pages the snapshot', async ({ page }) => {
    await openHunt(page, ids.hunt);
    // the findings section owns the bottom of the page: the Hits tab by default, no
    // associations graph, and the frozen rules panel not even mounted until its tab
    // is clicked
    await expect(page.getByTestId('hunt-hits')).toBeVisible();
    await expect(page.getByTestId('hunt-frozen-rules')).toHaveCount(0);
    await expect(page.getByText('Associations', { exact: true })).toHaveCount(0);
    // this corpus-less hunt never writes hits, so the default tab shows the empty banner
    await expect(page.getByText('No Hits Found')).toBeVisible();
    // the Frozen Rules tab swaps the panels in place (its badge carries the snapshot
    // size, so the accessible name needs the regex match)
    await page.getByRole('tab', { name: /Frozen Rules/ }).click();
    await expect(page.getByTestId('hunt-frozen-rules')).toBeVisible();
    await expect(page.getByText(`${RULE_COUNT} of ${RULE_COUNT} rules`)).toBeVisible({ timeout: 30000 });
    // the filter bar ends with the time picker, like every other filter bar
    await expect(page.getByRole('button', { name: 'All Time' })).toBeVisible();
    // page one holds the default 25 rows, name-sorted so the 00 rule leads
    await expect(ruleRows(page)).toHaveCount(25);
    await expect(ruleRows(page).first()).toHaveText(new RegExp(`e2e-hfr-yara-00-${RUN}`));
    await snapshot(page, SCREENSHOT_DIR, 'hunt-frozen-rules-section');
    // the remaining five lines live on the second page
    await page.getByText('Next', { exact: true }).click();
    await expect(ruleRows(page)).toHaveCount(5);
    await page.getByText('Back', { exact: true }).click();
    await expect(ruleRows(page)).toHaveCount(25);
    // tabbing back to hits only hides the drained panel: returning needs no
    // re-drain; count the drain requests instead of racing a timeout (a local
    // re-drain also finishes in well under any workable time bound)
    let drains = 0;
    page.on('request', (request) => {
      if (request.url().includes(`/entities/hunt/${ids.hunt}/rules`)) {
        drains += 1;
      }
    });
    await page.getByRole('tab', { name: 'Hits' }).click();
    await expect(page.getByTestId('hunt-hits')).toBeVisible();
    await page.getByRole('tab', { name: /Frozen Rules/ }).click();
    await expect(page.getByText(`${RULE_COUNT} of ${RULE_COUNT} rules`)).toBeVisible({ timeout: 2000 });
    // the panel stayed mounted so the tab round trip issued no snapshot reads
    expect(drains).toBe(0);
  });

  test('omnibar free text narrows the snapshot and rows link to the rule pages', async ({ page }) => {
    await openFrozenRules(page, ids.hunt);
    const omni = page.getByPlaceholder('Filter rules...');
    await omni.click();
    await omni.fill(`e2e-hfr-yara-07-${RUN}`);
    await omni.press('Enter');
    // the substring filter leaves exactly one line (and the count line says so)
    await expect(page.getByText(`1 of ${RULE_COUNT} rules`)).toBeVisible();
    await expect(ruleRows(page)).toHaveCount(1);
    await snapshot(page, SCREENSHOT_DIR, 'hunt-frozen-rules-filtered');
    // a hydrated line links to its engine's details page
    await ruleRows(page).first().click();
    await page.waitForURL((url) => url.pathname.startsWith('/rules/yara/'), { timeout: 15000 });
  });

  test('a non-matching filter shows the empty banner', async ({ page }) => {
    await openFrozenRules(page, ids.hunt);
    const omni = page.getByPlaceholder('Filter rules...');
    await omni.click();
    await omni.fill('no-such-rule-xyz');
    await omni.press('Enter');
    await expect(page.getByText('No Matching Rules Found')).toBeVisible();
  });

  test('a rule deleted after the freeze renders id-only and linkless', async ({ page }) => {
    // delete one frozen rule: the snapshot is immutable, so its line survives id-only
    ids.deletedRule = ids.rules[ids.rules.length - 1];
    await deleteEntity(token, ids.deletedRule);
    // the count is unchanged after a reload — the frozen list never shrinks
    await openFrozenRules(page, ids.hunt);
    // find the deleted line by its id through the omnibar (free text matches ids too)
    const omni = page.getByPlaceholder('Filter rules...');
    await omni.click();
    await omni.fill(ids.deletedRule);
    await omni.press('Enter');
    await expect(page.getByText(`1 of ${RULE_COUNT} rules`)).toBeVisible();
    // the line renders its id with the muted marker, and never as a link; the
    // omnibar clause chip echoes the id too, so target the row's name cell by
    // matching the element whose entire text is exactly the id
    await expect(page.getByText('(deleted or hidden)')).toBeVisible();
    await expect(page.locator('div').filter({ hasText: new RegExp(`^${ids.deletedRule}$`) })).toBeVisible();
    await expect(page.locator(`a:has-text("${ids.deletedRule}")`)).toHaveCount(0);
    await snapshot(page, SCREENSHOT_DIR, 'hunt-frozen-rules-deleted');
  });

  test('rule details pages keep the default associations graph', async ({ page }) => {
    await loginViaUI(page);
    await page.goto(`/rules/yara/${ids.rules[0]}`);
    // the override is hunt/ruleset-only: a yara rule page still renders the graph card
    await expect(page.getByText('Associations', { exact: true })).toBeVisible({ timeout: 30000 });
  });
});
