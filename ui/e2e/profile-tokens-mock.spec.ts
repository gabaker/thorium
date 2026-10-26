import { test, expect } from '@playwright/test';
import type { Page } from '@playwright/test';
import { MOCK_USER } from './helpers';

// Mocked test: the profile page's scoped token section (user tokens with their controls, run tokens
// read-only) and the Refresh button, which re-fetches whoami and the token list without reloading the
// page. A directory in SHOT_DIR makes the test also save screenshots of the page at two widths.

const SHOT_DIR = process.env.SHOT_DIR;

const USER_TOKEN = {
  name: 'ci-readonly',
  owner: 'test',
  role: 'User',
  groups: ['system', 'research', 'malware-team', 'incident-response'],
  token: 'user-token-value',
  token_expiration: '2099-01-01T00:00:00Z',
  expires: null,
};

const RUN_TOKEN = {
  name: 'run-0a1b2c3d',
  owner: 'test',
  role: 'User',
  groups: ['system'],
  token: 'run-token-value',
  token_expiration: '2099-01-01T00:00:00Z',
  expires: '2099-02-01T00:00:00Z',
  run: { reaction: '0a1b2c3d-1111-2222-3333-444455556666', tool_instances: ['t-1', 't-2', 't-3'] },
};

// Stand up whoami and the scoped token list, counting how often each is fetched.
async function setupMock(page: Page, tokens: object[], user: object = MOCK_USER) {
  const counts = { whoami: 0, tokens: 0 };
  await page.route('**/api/**', (route) => route.fulfill({ status: 200, contentType: 'application/json', body: '{}' }));
  await page.route('**/api/users/whoami', (route) => {
    counts.whoami += 1;
    return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(user) });
  });
  await page.route('**/api/users/tokens/', (route) => {
    counts.tokens += 1;
    return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(tokens) });
  });
  await page.context().addCookies([{ name: 'THORIUM_TOKEN', value: MOCK_USER.token, domain: 'localhost', path: '/' }]);
  return counts;
}

test.describe('Profile scoped tokens (mocked)', () => {
  test('lists run tokens read-only next to the user tokens', async ({ page }) => {
    await setupMock(page, [USER_TOKEN, RUN_TOKEN]);
    await page.goto('/profile');
    const userTable = page.getByRole('table', { name: 'Scoped tokens' });
    await expect(userTable.getByText('ci-readonly')).toBeVisible();
    await expect(userTable.getByRole('button', { name: 'Activate' })).toBeVisible();
    await expect(userTable.getByRole('button', { name: 'Edit' })).toBeVisible();
    await expect(userTable.getByRole('button', { name: 'Delete' })).toBeVisible();
    const runTable = page.getByRole('table', { name: 'Run tokens' });
    await expect(runTable.getByText('run-0a1b2c3d')).toBeVisible();
    await expect(runTable.getByText('0a1b2c3d-1111-2222-3333-444455556666')).toBeVisible();
    await expect(runTable.getByRole('button')).toHaveCount(0);
    await expect(userTable.getByText('run-0a1b2c3d')).toHaveCount(0);
    if (SHOT_DIR) {
      for (const width of [1440, 1024]) {
        await page.setViewportSize({ width, height: 1400 });
        await page.screenshot({ path: `${SHOT_DIR}/profile-${width}.png`, fullPage: true });
      }
    }
  });

  test('Refresh re-fetches whoami and the token list in place', async ({ page }) => {
    const counts = await setupMock(page, [USER_TOKEN]);
    await page.goto('/profile');
    await expect(page.getByText('ci-readonly')).toBeVisible();
    const before = { ...counts };
    // a marker on the window survives only if the page is not reloaded
    await page.evaluate(() => {
      (window as unknown as { marker: string }).marker = 'kept';
    });
    await page.getByRole('button', { name: 'Refresh' }).click();
    await expect.poll(() => counts.whoami).toBeGreaterThan(before.whoami);
    await expect.poll(() => counts.tokens).toBeGreaterThan(before.tokens);
    await expect(page.getByRole('button', { name: 'Refresh' })).toBeEnabled();
    expect(await page.evaluate(() => (window as unknown as { marker: string }).marker)).toBe('kept');
  });

  test('shows the empty message when there are no scoped tokens', async ({ page }) => {
    await setupMock(page, []);
    await page.goto('/profile');
    await expect(page.getByText('No scoped tokens yet.')).toBeVisible();
  });

  test('the scoped token table fits its panel at 1024 pixels wide with both expiry columns visible', async ({ page }) => {
    await setupMock(page, [USER_TOKEN]);
    await page.setViewportSize({ width: 1024, height: 768 });
    await page.goto('/profile');
    const table = page.getByRole('table', { name: 'Scoped tokens' });
    await expect(table.getByText('ci-readonly')).toBeVisible();
    // the frame has nothing to scroll sideways, so no column hides behind the sticky actions
    const spill = await table.evaluate((el) => (el.parentElement?.scrollWidth ?? 0) - (el.parentElement?.clientWidth ?? 0));
    expect(spill).toBeLessThanOrEqual(0);
    await expect(table.getByRole('columnheader', { name: /Token expiry/ })).toBeInViewport({ ratio: 1 });
  });

  test('a long group name and a token name with no separators still fit the table in its frame', async ({ page }) => {
    const longToken = {
      ...USER_TOKEN,
      name: 'uiloopnoseparatorsscopedtokennamexxxxxxxx',
      role: { Developer: { k8s: true, bare_metal: true, windows: true, external: true, kvm: true } },
      groups: ['juno', 'mars', 'minerva', 'static', 'uiloop-long-group-name'],
    };
    await setupMock(page, [longToken]);
    for (const width of [1024, 1440]) {
      await page.setViewportSize({ width, height: 768 });
      await page.goto('/profile');
      const table = page.getByRole('table', { name: 'Scoped tokens' });
      await expect(table.getByText(longToken.name)).toBeVisible();
      // the frame has nothing to scroll sideways, so no column hides behind the sticky actions
      const spill = await table.evaluate((el) => (el.parentElement?.scrollWidth ?? 0) - (el.parentElement?.clientWidth ?? 0));
      expect(spill, `table spill at ${width}px`).toBeLessThanOrEqual(0);
      await expect(table.getByRole('columnheader', { name: /Token expiry/ })).toBeInViewport({ ratio: 1 });
    }
  });

  test('clicking a group label ticks its checkbox in the new token form', async ({ page }) => {
    await setupMock(page, []);
    await page.goto('/profile');
    await page.getByRole('button', { name: 'New scoped token' }).click();
    const dialog = page.getByRole('dialog');
    await dialog.getByText('system', { exact: true }).click();
    await expect(dialog.getByLabel('system')).toBeChecked();
  });

  test('the new token form offers the Developer role only to a user who can use it', async ({ page }) => {
    await setupMock(page, [], { ...MOCK_USER, role: 'User' });
    await page.goto('/profile');
    await page.getByRole('button', { name: 'New scoped token' }).click();
    const roles = page.getByRole('dialog').getByRole('combobox');
    await expect(roles).toBeVisible();
    await expect(roles.getByRole('option', { name: 'Developer' })).toHaveCount(0);
  });
});
