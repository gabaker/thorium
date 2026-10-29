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

const MOCK_IMAGE_BANNED = {
  ...MOCK_IMAGE,
  name: 'banned-image',
  bans: {
    'ban-1': { id: 'ban-1', time_banned: '2026-10-01T12:00:00Z', ban_kind: { Generic: { msg: 'The image registry is gone' } } },
  },
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

/// Counts the columns of the balanced section grid in an expanded image's details
async function sectionColumns(page: Page, name: string): Promise<number> {
  return imageItem(page, name)
    .locator('[data-testid="accordion-body"] b', { hasText: /^Resources$/ })
    .evaluate((heading) => {
      // the section cards sit in columns that are the children of the nearest grid container
      let node: HTMLElement | null = heading as HTMLElement;
      while (node && getComputedStyle(node).display !== 'grid') node = node.parentElement;
      return node ? node.children.length : 0;
    });
}

test.describe('Image details', () => {
  test.beforeEach(async ({ page }) => {
    await setupMockAuth(page);
  });

  test('a banned image shows a warning icon and its ban details', async ({ page }) => {
    await setupImageMocks(page, [MOCK_IMAGE, MOCK_IMAGE_BANNED]);
    await page.goto('/images');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    // only the banned image carries the warning icon in its header status
    await expect(imageItem(page, 'banned-image').locator('.accordion-item-status svg')).toHaveCount(1);
    await expect(imageItem(page, 'test-image').locator('.accordion-item-status svg')).toHaveCount(0);

    await imageItem(page, 'banned-image').locator('[data-testid="accordion-header"]').click();
    const body = imageItem(page, 'banned-image').locator('[data-testid="accordion-body"]');
    await expect(body.getByText('Ban: The image registry is gone. Banned on')).toBeVisible();
  });

  test('an External image shows None badges and leaves out the sections it does not use', async ({ page }) => {
    await setupImageMocks(page);
    await page.goto('/images');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await imageItem(page, 'external-tool').locator('[data-testid="accordion-header"]').click();
    const body = imageItem(page, 'external-tool').locator('[data-testid="accordion-body"]');
    await expect(body.locator('b', { hasText: /^Resources$/ })).toBeVisible();
    // the image sets none of its optional fields, so their grey badges say none
    expect(await body.locator('.bg-grey', { hasText: /^none$/ }).count()).toBeGreaterThanOrEqual(3);
    // an External image is not run by Thorium, so it has no output collection, dependencies, or child filters
    for (const section of ['Output Collection', 'Dependencies', 'Child Filters']) {
      await expect(body.locator('b', { hasText: new RegExp(`^${section}$`) })).toHaveCount(0);
    }
  });

  test('the detail sections fill two columns on a wide window and one on a narrow one', async ({ page }) => {
    await setupImageMocks(page);
    await page.setViewportSize({ width: 1400, height: 900 });
    await page.goto('/images');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });
    await imageItem(page, 'test-image').locator('[data-testid="accordion-header"]').click();
    await expect.poll(() => sectionColumns(page, 'test-image')).toBe(2);

    await page.setViewportSize({ width: 800, height: 900 });
    await expect.poll(() => sectionColumns(page, 'test-image')).toBe(1);
  });
});
