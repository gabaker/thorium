import { test, Page } from '@playwright/test';

import { expect, fulfillJson, MOCK_GROUP } from './agentic-mocks';
import { setupMockAuth } from './helpers';

/**
 * Mock the group list and fail the legacy list read with a 500.
 *
 * @param page - A signed in page.
 * @param listRoute - The glob of the list route that fails.
 */
async function failList(page: Page, listRoute: string) {
  await page.route('**/api/groups/details/**', (route) =>
    fulfillJson(route, { details: [{ name: MOCK_GROUP, owners: {}, managers: {}, analysts: {}, users: {}, monitors: {} }] }),
  );
  await page.route(listRoute, (route) => fulfillJson(route, { error: 'uiloop boom' }, 500));
}

test.describe('legacy list read errors', () => {
  test.beforeEach(async ({ page }) => {
    await setupMockAuth(page);
  });

  test('a failed pipeline list read shows the error, not an empty list', async ({ page }) => {
    await failList(page, '**/api/pipelines/list/**');
    await page.goto('/pipelines');
    await expect(page.getByTestId('list-error')).toContainText('uiloop boom');
    await expect(page.getByText('No Pipelines Found')).toHaveCount(0);
    // a count of 0 would claim the list is empty
    await expect(page.getByTestId('pipeline-count')).toHaveCount(0);
  });

  test('a failed image list read shows the error, not an empty list', async ({ page }) => {
    await failList(page, '**/api/images/**');
    await page.goto('/images');
    await expect(page.getByTestId('list-error')).toContainText('uiloop boom');
    await expect(page.getByText('No Images Found')).toHaveCount(0);
    // a count of 0 would claim the list is empty
    await expect(page.getByTestId('image-count')).toHaveCount(0);
  });

  test('a failed tool list on create pipeline sits apart from the info banner above it', async ({ page }) => {
    await page.route('**/api/entities/details/**', (route) => fulfillJson(route, { error: 'uiloop boom' }, 500));
    await page.goto('/create/pipeline');
    const banner = page.getByTestId('tools-error').locator('..');
    await expect(banner).toContainText('uiloop boom');
    // the banner keeps a gap from the banner above it
    const gap = await banner.evaluate((el) => {
      const previous = el.previousElementSibling as HTMLElement;
      return el.getBoundingClientRect().top - previous.getBoundingClientRect().bottom;
    });
    expect(gap).toBeGreaterThan(4);
  });
});
