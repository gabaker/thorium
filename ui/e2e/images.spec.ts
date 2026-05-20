import { test, expect, Page } from '@playwright/test';
import path from 'path';
import { snapshot } from './helpers';

const SCREENSHOT_DIR = path.join(import.meta.dirname, 'screenshots');

const MOCK_USER = {
  username: 'test',
  role: 'Admin',
  email: 'test@thorium.dev',
  groups: ['system'],
  token: 'mock-token-for-image-test',
  token_expiration: '2099-01-01T00:00:00Z',
  settings: { theme: 'Dark' },
  local: true,
  verified: true,
};

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

async function waitForEditor(page: Page) {
  await page.waitForSelector('.cm-editor', { timeout: 10000 });
  await page.waitForTimeout(500);
}

test.describe('Image Browsing Page', () => {
  test.beforeEach(async ({ page }) => {
    await setupMockAuth(page);
    await setupImageMocks(page);
  });

  test('image list renders with correct count', async ({ page }) => {
    await page.goto('/images');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    const badge = page.locator('.count-badge');
    await expect(badge).toContainText('2');

    const items = page.locator('.accordion-item');
    await expect(items).toHaveCount(2);

    await snapshot(page, SCREENSHOT_DIR, 'images-list');
  });

  test('image names and groups display correctly', async ({ page }) => {
    await page.goto('/images');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await expect(page.locator('.accordion-item-name .text').first()).toContainText('test-image');
    await expect(page.locator('.accordion-item-ownership i').first()).toContainText('system');
  });

  test('expand image shows details', async ({ page }) => {
    await page.goto('/images');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await page.locator('.accordion-header').first().click();
    await page.waitForTimeout(500);

    await expect(page.locator('text=test')).toBeVisible();
    await expect(page.locator('text=K8s')).toBeVisible();

    await snapshot(page, SCREENSHOT_DIR, 'image-details-expanded');
  });

  test('edit button enters edit mode with form', async ({ page }) => {
    await page.goto('/images');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await page.locator('.accordion-header').first().click();
    await page.waitForTimeout(500);

    await page.locator('button:has-text("Edit")').first().click();
    await page.waitForTimeout(500);

    await expect(page.locator('button:has-text("Cancel")').first()).toBeVisible();
    await expect(page.locator('button:has-text("Update")').first()).toBeVisible();

    await snapshot(page, SCREENSHOT_DIR, 'image-edit-form-mode');
  });

  test('view mode toggle appears in edit mode', async ({ page }) => {
    await page.goto('/images');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await page.locator('.accordion-header').first().click();
    await page.waitForTimeout(500);

    await page.locator('button:has-text("Edit")').first().click();
    await page.waitForTimeout(500);

    const formBtn = page.locator('button:has-text("Form")');
    const editorBtn = page.locator('button:has-text("Editor")');
    await expect(formBtn).toBeVisible();
    await expect(editorBtn).toBeVisible();
  });

  test('switching to editor mode shows code editor', async ({ page }) => {
    await page.goto('/images');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await page.locator('.accordion-header').first().click();
    await page.waitForTimeout(500);

    await page.locator('button:has-text("Edit")').first().click();
    await page.waitForTimeout(500);

    await page.locator('button:has-text("Editor")').click();
    await page.waitForTimeout(500);

    await waitForEditor(page);

    const yamlBtn = page.locator('button:has-text("YAML")');
    await expect(yamlBtn).toBeVisible();

    await snapshot(page, SCREENSHOT_DIR, 'image-edit-editor-mode');
  });

  test('cancel edit returns to view mode', async ({ page }) => {
    await page.goto('/images');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await page.locator('.accordion-header').first().click();
    await page.waitForTimeout(500);

    await page.locator('button:has-text("Edit")').first().click();
    await page.waitForTimeout(500);

    await page.locator('button:has-text("Cancel")').first().click();
    await page.waitForTimeout(300);

    await expect(page.locator('button:has-text("Edit")').first()).toBeVisible();
    await expect(page.locator('button:has-text("Cancel")')).toHaveCount(0);
  });

  test('delete image shows confirmation modal', async ({ page }) => {
    await page.goto('/images');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await page.locator('.accordion-header').first().click();
    await page.waitForTimeout(500);

    await page.locator('button:has-text("Delete")').first().click();
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
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await page.locator('.accordion-header').first().click();
    await page.waitForTimeout(500);

    await page.locator('button:has-text("Delete")').first().click();
    await page.waitForTimeout(300);

    await page.locator('.danger-btn:has-text("Confirm")').click();
    await page.waitForTimeout(500);

    expect(deleteRequested).toBe(true);
  });

  test('copy image navigates to create page', async ({ page }) => {
    await page.goto('/images');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await page.locator('.accordion-header').first().click();
    await page.waitForTimeout(500);

    await page.locator('button:has-text("Copy")').first().click();
    await page.waitForTimeout(500);

    await expect(page).toHaveURL(/\/create\/image/);
  });

  test('create image button navigates to create page', async ({ page }) => {
    await page.goto('/images');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await page.locator('.ok-btn:has-text("+")').click();
    await page.waitForTimeout(500);

    await expect(page).toHaveURL(/\/create\/image/);
  });
});

test.describe('Image Create Page', () => {
  test.beforeEach(async ({ page }) => {
    await setupMockAuth(page);
    await setupImageMocks(page);
  });

  test('create page renders with form view by default', async ({ page }) => {
    await page.goto('/create/image');
    await page.waitForSelector('h3:has-text("Create An Image")', { timeout: 10000 });

    const formBtn = page.locator('button:has-text("Form")');
    const editorBtn = page.locator('button:has-text("Editor")');
    await expect(formBtn).toBeVisible();
    await expect(editorBtn).toBeVisible();

    await snapshot(page, SCREENSHOT_DIR, 'image-create-form');
  });

  test('toggle to editor mode shows code editor', async ({ page }) => {
    await page.goto('/create/image');
    await page.waitForSelector('h3:has-text("Create An Image")', { timeout: 10000 });

    await page.locator('button:has-text("Editor")').click();
    await page.waitForTimeout(500);

    await waitForEditor(page);

    const yamlBtn = page.locator('button:has-text("YAML")');
    await expect(yamlBtn).toBeVisible();

    await snapshot(page, SCREENSHOT_DIR, 'image-create-editor');
  });

  test('editor mode has format toggle', async ({ page }) => {
    await page.goto('/create/image');
    await page.waitForSelector('h3:has-text("Create An Image")', { timeout: 10000 });

    await page.locator('button:has-text("Editor")').click();
    await page.waitForTimeout(500);

    const yamlBtn = page.locator('button:has-text("YAML")');
    const jsonBtn = page.locator('button:has-text("JSON")');
    await expect(yamlBtn).toBeVisible();
    await expect(jsonBtn).toBeVisible();
  });

  test('format toggle switches between YAML and JSON', async ({ page }) => {
    await page.goto('/create/image');
    await page.waitForSelector('h3:has-text("Create An Image")', { timeout: 10000 });

    await page.locator('button:has-text("Editor")').click();
    await page.waitForTimeout(500);
    await waitForEditor(page);

    const editorContent = await page.locator('.cm-content').textContent();
    expect(editorContent).toContain('group');

    await page.locator('button:has-text("JSON")').click();
    await page.waitForTimeout(500);
    await waitForEditor(page);

    const jsonContent = await page.locator('.cm-content').textContent();
    expect(jsonContent).toContain('{');
  });

  test('cancel button navigates back', async ({ page }) => {
    await page.goto('/create/image');
    await page.waitForSelector('h3:has-text("Create An Image")', { timeout: 10000 });

    const currentUrl = page.url();
    await page.locator('button:has-text("Cancel")').click();
    await page.waitForTimeout(500);

    expect(page.url()).not.toBe(currentUrl);
  });

  test('create and cancel buttons are visible', async ({ page }) => {
    await page.goto('/create/image');
    await page.waitForSelector('h3:has-text("Create An Image")', { timeout: 10000 });

    await expect(page.locator('button:has-text("Create")')).toBeVisible();
    await expect(page.locator('button:has-text("Cancel")')).toBeVisible();
  });
});
