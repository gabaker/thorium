import { test, expect, Page } from '@playwright/test';
import path from 'path';
import { snapshot, MOCK_USER, waitForEditor, setEditorContent } from './helpers';

const SCREENSHOT_DIR = path.join(import.meta.dirname, 'screenshots');

const MOCK_GROUP = {
  name: 'system',
  owners: { combined: ['test'], direct: ['test'], metagroups: [] },
  managers: { combined: [], direct: [], metagroups: [] },
  analysts: [],
  users: { combined: [], direct: [], metagroups: [] },
  monitors: { combined: [], direct: [], metagroups: [] },
  description: 'System group',
  allowed: {
    files: true,
    repos: true,
    tags: true,
    images: true,
    pipelines: true,
    reactions: true,
    results: true,
    comments: true,
    entities: true,
  },
};

const MOCK_IMAGE = {
  group: 'system',
  name: 'test-image',
  creator: 'test',
  scaler: 'K8s',
  image: 'thorium/test-image:latest',
  timeout: 300,
  display_type: 'JSON',
  resources: { cpu: 1000, memory: 512, ephemeral_storage: 1024 },
  args: { output: 'Append', output_files: 'None' },
  dependencies: {},
  output_collection: { handler: 'Files', files: { results: '/tmp/results' } },
  description: 'A test image for E2E testing',
  collect_logs: true,
  generator: false,
  triggers: {},
};

const MOCK_IMAGE_EXTERNAL = {
  group: 'system',
  name: 'external-tool',
  creator: 'test',
  scaler: 'External',
  image: 'thorium/external:latest',
  timeout: 600,
  display_type: 'JSON',
  description: 'External scaler image',
};

async function setupMockAuth(page: Page) {
  await page.route('**/api/users/whoami', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(MOCK_USER) }),
  );
  await page.context().addCookies([
    {
      name: 'THORIUM_TOKEN',
      value: MOCK_USER.token,
      domain: 'localhost',
      path: '/',
    },
  ]);
}

async function setupImageMocks(page: Page, images = [MOCK_IMAGE, MOCK_IMAGE_EXTERNAL]) {
  await page.route('**/api/groups/details/**', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ details: [MOCK_GROUP] }) }),
  );
  await page.route('**/api/groups/', (route) => {
    const url = route.request().url();
    if (url.includes('/details/')) return route.fallback();
    return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ names: ['system'] }) });
  });
  await page.route('**/api/images/**/details/**', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ details: images }) }),
  );
  await page.route('**/api/images/**/', (route) => {
    const url = route.request().url();
    if (url.includes('/details/')) return route.fallback();
    if (route.request().method() === 'GET') {
      return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ names: images.map((i) => i.name) }) });
    }
    return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
  });
  await page.route('**/api/images/*/*', (route) => {
    if (route.request().method() === 'DELETE') {
      return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
    }
    if (route.request().method() === 'PATCH') {
      return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
    }
    if (route.request().method() === 'GET') {
      return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(MOCK_IMAGE) });
    }
    return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
  });
}

/// Finds the accordion item for the image with the given name
function imageItem(page: Page, name: string) {
  return page.locator('[data-testid="accordion-item"]').filter({ hasText: name });
}

test.describe('Image Browsing Page', () => {
  test.beforeEach(async ({ page }) => {
    await setupMockAuth(page);
    await setupImageMocks(page);
  });

  test('image list renders with correct count', async ({ page }) => {
    await page.goto('/images');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    const badge = page.locator('[data-testid="image-count"]');
    await expect(badge).toContainText('2');

    const items = page.locator('[data-testid="accordion-item"]');
    await expect(items).toHaveCount(2);

    // every image is marked legacy
    await expect(page.locator('[data-testid="legacy-badge"]')).toHaveCount(2);

    await snapshot(page, SCREENSHOT_DIR, 'images-list');
  });

  test('image names and groups display correctly', async ({ page }) => {
    await page.goto('/images');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    // the list is sorted by name, so the external image comes first
    await expect(page.locator('.accordion-item-name .text').first()).toContainText('external-tool');
    await expect(page.locator('.accordion-item-name .text').nth(1)).toContainText('test-image');
    await expect(page.locator('.accordion-item-subtitle i').first()).toContainText('system');
  });

  test('expand image shows details', async ({ page }) => {
    await page.goto('/images');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await imageItem(page, 'test-image').locator('[data-testid="accordion-header"]').click();
    await page.waitForTimeout(500);

    // the details show the creator and scaler under their headings
    const body = imageItem(page, 'test-image').locator('[data-testid="accordion-body"]');
    await expect(body.getByText('Creator', { exact: true })).toBeVisible();
    await expect(body.getByText('test', { exact: true }).first()).toBeVisible();
    await expect(body.getByText('K8s', { exact: true })).toBeVisible();
    await expect(body.getByText('A test image for E2E testing')).toBeVisible();

    await snapshot(page, SCREENSHOT_DIR, 'image-details-expanded');
  });

  test('delete image shows confirmation modal', async ({ page }) => {
    await page.goto('/images');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await imageItem(page, 'test-image').locator('[data-testid="header-btn-delete"]').click();
    await page.waitForTimeout(300);

    const modal = page.locator('.modal');
    await expect(modal).toBeVisible();
    await expect(modal).toContainText('Confirm deletion');
    await expect(modal).toContainText('test-image');

    await snapshot(page, SCREENSHOT_DIR, 'image-delete-confirm');
  });

  test('confirm delete calls API', async ({ page }) => {
    let deleteRequested = false;
    await page.route('**/api/images/system/test-image', (route) => {
      if (route.request().method() === 'DELETE') {
        deleteRequested = true;
        return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
      }
      return route.fallback();
    });

    await page.goto('/images');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await imageItem(page, 'test-image').locator('[data-testid="header-btn-delete"]').click();
    await page.waitForTimeout(300);

    await page.locator('.danger-btn:has-text("Confirm")').click();
    await page.waitForTimeout(500);

    expect(deleteRequested).toBe(true);
  });
});
