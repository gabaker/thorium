import { test, expect, Page } from '@playwright/test';
import { MOCK_USER, setupMockAuth } from './helpers';

/**
 * The /hunt-status admin gate: the API-side is_admin! checks are the real
 * boundary (pinned by the api 401 matrix tests); these cover the cosmetic
 * UI layer — a non-admin is redirected off the page and never sees its nav
 * entry, while an admin reaches the page. Both run against a mocked whoami
 * so no live instance is needed.
 */

/**
 * Mock auth as a given role, stubbing every other API call with an empty body.
 *
 * @param page - The page to install the mocks on.
 * @param role - The role whoami should report.
 */
async function mockAuthAs(page: Page, role: string): Promise<void> {
  const user = { ...MOCK_USER, role };
  await page.route('**/api/users/whoami', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(user) }),
  );
  await page.route('**/api/**', (route) => {
    if (route.request().url().includes('/users/whoami')) return route.fallback();
    return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
  });
  await page.context().addCookies([
    {
      name: 'THORIUM_TOKEN',
      value: MOCK_USER.token,
      domain: 'localhost',
      path: '/',
    },
  ]);
}

test.describe('hunt status admin gate', () => {
  test('non-admin is redirected off /hunt-status', async ({ page }) => {
    await mockAuthAs(page, 'User');
    await page.goto('/hunt-status');
    // RequireAdmin bounces non-admins to the root route
    await page.waitForURL((url) => url.pathname === '/', { timeout: 15000 });
    await expect(page).not.toHaveURL(/hunt-status/);
  });

  test('non-admin never sees the Hunt Status nav entry', async ({ page }) => {
    await mockAuthAs(page, 'User');
    await page.setViewportSize({ width: 1400, height: 900 });
    await page.goto('/');
    await page.waitForLoadState('networkidle');
    // assert on DOM presence, not visibility: collapsed sidebar children are
    // only visibility:hidden for every role, so a not-visible check would pass
    // even if the adminOnly gate broke; a non-admin must not get the Admin
    // category (or any link into it) rendered at all
    await expect(page.getByTestId('category-Admin')).toHaveCount(0);
    await expect(page.locator('a[href="/hunt-status"]')).toHaveCount(0);
  });

  test('admin gets the Admin nav category with the Hunt Status entry', async ({ page }) => {
    await setupMockAuth(page);
    await page.setViewportSize({ width: 1400, height: 900 });
    await page.goto('/');
    await page.waitForLoadState('networkidle');
    // the positive control for the gate: an admin's sidebar renders the Admin
    // category and its Hunt Status link (collapsed children stay in the DOM)
    await expect(page.getByTestId('category-Admin')).toHaveCount(1);
    await expect(page.locator('a[href="/hunt-status"]')).toHaveCount(1);
  });

  test('admin reaches /hunt-status and sees the page scaffolding', async ({ page }) => {
    await setupMockAuth(page);
    await page.goto('/hunt-status');
    await page.waitForLoadState('networkidle');
    // the admin stays on the page and the stat tiles render their placeholders
    // (the page title is a styled div, so assert on real page content instead)
    await expect(page).toHaveURL(/hunt-status/);
    await expect(page.getByText('Active hunts vs global quota')).toBeVisible();
    // the stats charts render their collecting state against the stubbed API
    await expect(page.getByRole('heading', { name: 'Scans per second' })).toBeVisible();
    await expect(page.getByText('Collecting snapshots…').first()).toBeVisible();
  });
});
