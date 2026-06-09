import { test, expect, Page } from '@playwright/test';
import path from 'path';
import { snapshot, MOCK_USER } from './helpers';

// The search omnibar is rendered on the Home page ('/'), not a dedicated /search route.
const SCREENSHOT_DIR = path.join(import.meta.dirname, 'screenshots');

// A single Elastic result document. `id` is `<sha256>-<group>`; the UI renders a link whose
// text is the sha256 portion (everything before the first '-').
const MOCK_RESULT = {
  id: 'abc123def456-system',
  index: 'thorium_sample_results',
  highlight: { name: 'matched-on-name' },
};
const RESULT_SHA = 'abc123def456';

async function setupSearchMocks(page: Page, results: Array<Record<string, unknown>> = [MOCK_RESULT]) {
  await page.route('**/api/users/whoami', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(MOCK_USER) }),
  );
  await page.route('**/api/search/**', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ data: results, cursor: null }) }),
  );
  // Catch-all for any other API call (e.g. tag counts). Registered last so it is matched first;
  // it falls back to the specific handlers above for whoami/search.
  await page.route('**/api/**', (route) => {
    const url = route.request().url();
    if (url.includes('/users/whoami') || url.includes('/api/search')) return route.fallback();
    return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
  });
  await page.context().addCookies([{ name: 'THORIUM_TOKEN', value: MOCK_USER.token, domain: 'localhost', path: '/' }]);
}

test.describe('Search Omnibar', () => {
  test.beforeEach(async ({ page }) => {
    await setupSearchMocks(page);
  });

  test('renders the omnibar and shows no results until a clause is added', async ({ page }) => {
    await page.goto('/');
    await expect(page.getByPlaceholder('Search...')).toBeVisible();
    // The results header (and any clauses) only render once at least one clause exists.
    await expect(page.getByText('SHA256')).toHaveCount(0);
    await expect(page.getByTitle('delete clause')).toHaveCount(0);
    await snapshot(page, SCREENSHOT_DIR, 'search-empty');
  });

  test('typing a text query adds a clause, updates the URL, and renders results', async ({ page }) => {
    await page.goto('/');
    const input = page.getByPlaceholder('Search...');
    await input.click();
    await input.fill('malware');
    await input.press('Enter');

    // a clause chip is added immediately
    await expect(page.getByTitle('delete clause')).toHaveCount(1);
    // Search debounces (~500ms) before writing the query to the URL
    await expect(page).toHaveURL(/[?&]query=malware/);
    // the results header and the mocked result link render
    await expect(page.getByText('SHA256')).toBeVisible();
    await expect(page.getByRole('link', { name: RESULT_SHA })).toBeVisible();
    await snapshot(page, SCREENSHOT_DIR, 'search-results');
  });

  test('loads an existing query param into the omnibar on navigation', async ({ page }) => {
    await page.goto(`/?query=${RESULT_SHA}`);
    // the query is reflected back into a clause chip...
    await expect(page.getByTitle('delete clause')).toHaveCount(1);
    // ...the URL keeps the query param...
    await expect(page).toHaveURL(new RegExp(`[?&]query=${RESULT_SHA}`));
    // ...and results render for it
    await expect(page.getByRole('link', { name: RESULT_SHA })).toBeVisible();
  });

  test('clearing the clause removes results and the query param', async ({ page }) => {
    await page.goto(`/?query=${RESULT_SHA}`);
    await expect(page.getByRole('link', { name: RESULT_SHA })).toBeVisible();

    await page.getByTitle('delete clause').click();

    await expect(page.getByTitle('delete clause')).toHaveCount(0);
    await expect(page.getByText('SHA256')).toHaveCount(0);
    await expect(page).not.toHaveURL(/query=/);
  });
});
