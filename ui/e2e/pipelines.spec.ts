import { test, expect, Page } from '@playwright/test';
import path from 'path';
import { snapshot, MOCK_USER, waitForEditor } from './helpers';

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

const MOCK_PIPELINE = {
  group: 'system',
  name: 'test-pipeline',
  creator: 'test',
  order: [['tool-a', 'tool-b'], 'tool-c'],
  sla: 604800,
  description: 'A test pipeline for E2E testing',
  triggers: {},
};

const MOCK_PIPELINE_WITH_TRIGGER = {
  group: 'system',
  name: 'triggered-pipeline',
  creator: 'test',
  order: ['scanner'],
  sla: 86400,
  description: 'Pipeline with event trigger',
  triggers: {
    'new-sample-trigger': 'NewSample',
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

async function setupPipelineMocks(page: Page, pipelines = [MOCK_PIPELINE, MOCK_PIPELINE_WITH_TRIGGER]) {
  await page.route('**/api/groups/details/**', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ details: [MOCK_GROUP] }) }),
  );
  await page.route('**/api/pipelines/list/**/details/**', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ details: pipelines }) }),
  );
  await page.route('**/api/pipelines/', (route) => {
    if (route.request().method() === 'POST') {
      return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
    }
    return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
  });
  await page.route('**/api/pipelines/*/*', (route) => {
    if (route.request().method() === 'DELETE') {
      return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
    }
    if (route.request().method() === 'PATCH') {
      return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
    }
    return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
  });
}

test.describe('Pipelines Page', () => {
  test.beforeEach(async ({ page }) => {
    await setupMockAuth(page);
    await setupPipelineMocks(page);
  });

  test('pipeline list renders with correct count', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    const badge = page.locator('.count-badge');
    await expect(badge).toContainText('2');

    const items = page.locator('.accordion-item');
    await expect(items).toHaveCount(2);

    await snapshot(page, SCREENSHOT_DIR, 'pipelines-list');
  });

  test('pipeline names and groups display correctly', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await expect(page.locator('.accordion-item-name .text').first()).toContainText('test-pipeline');
    await expect(page.locator('.accordion-item-ownership i').first()).toContainText('system');
  });

  test('expand pipeline shows details', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await page.locator('.accordion-header').first().click();
    await page.waitForTimeout(500);

    await expect(page.locator('.bg-blue').first()).toContainText('test');
    await expect(page.locator('text=A test pipeline for E2E testing')).toBeVisible();
    await expect(page.locator('text=604800')).toBeVisible();

    await snapshot(page, SCREENSHOT_DIR, 'pipeline-details-expanded');
  });

  test('pipeline with NewSample trigger displays trigger type', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await page.locator('.accordion-header').nth(1).click();
    await page.waitForTimeout(500);

    await expect(page.locator('text=new-sample-trigger')).toBeVisible();
    await expect(page.locator('text=NewSample')).toBeVisible();
  });

  test('create pipeline button opens modal', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await page.locator('.ok-btn:has-text("+")').click();
    await page.waitForTimeout(500);

    const modal = page.locator('.modal');
    await expect(modal).toBeVisible();
    await expect(modal.locator('.modal-title')).toContainText('Create New Pipeline');

    await waitForEditor(page);

    await snapshot(page, SCREENSHOT_DIR, 'pipeline-create-modal');
  });

  test('create pipeline modal has format toggle', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await page.locator('.ok-btn:has-text("+")').click();
    await page.waitForTimeout(500);

    const yamlBtn = page.locator('button:has-text("YAML")');
    const jsonBtn = page.locator('button:has-text("JSON")');
    await expect(yamlBtn).toBeVisible();
    await expect(jsonBtn).toBeVisible();
  });

  test('edit button enters editor mode', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await page.locator('.accordion-header').first().click();
    await page.waitForTimeout(500);

    await page.locator('button:has-text("Edit")').first().click();
    await page.waitForTimeout(500);

    await waitForEditor(page);
    await expect(page.locator('button:has-text("Cancel")').first()).toBeVisible();
    await expect(page.locator('button:has-text("Update")').first()).toBeVisible();

    await snapshot(page, SCREENSHOT_DIR, 'pipeline-edit-mode');
  });

  test('cancel edit returns to view mode', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await page.locator('.accordion-header').first().click();
    await page.waitForTimeout(500);

    await page.locator('button:has-text("Edit")').first().click();
    await page.waitForTimeout(500);
    await waitForEditor(page);

    await page.locator('button:has-text("Cancel")').first().click();
    await page.waitForTimeout(300);

    await expect(page.locator('.cm-editor')).toHaveCount(0);
    await expect(page.locator('text=A test pipeline for E2E testing')).toBeVisible();
  });

  test('delete pipeline shows confirmation modal', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await page.locator('.accordion-header').first().click();
    await page.waitForTimeout(500);

    await page.locator('button:has-text("Delete")').first().click();
    await page.waitForTimeout(300);

    const modal = page.locator('.modal');
    await expect(modal).toBeVisible();
    await expect(modal).toContainText('Confirm deletion');
    await expect(modal).toContainText('test-pipeline');

    await snapshot(page, SCREENSHOT_DIR, 'pipeline-delete-confirm');
  });

  test('confirm delete calls API', async ({ page }) => {
    let deleteRequested = false;
    await page.route('**/api/pipelines/system/test-pipeline', (route) => {
      if (route.request().method() === 'DELETE') {
        deleteRequested = true;
        return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
      }
      return route.fallback();
    });

    await page.goto('/pipelines');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await page.locator('.accordion-header').first().click();
    await page.waitForTimeout(500);

    await page.locator('button:has-text("Delete")').first().click();
    await page.waitForTimeout(300);

    await page.locator('.danger-btn:has-text("Confirm")').click();
    await page.waitForTimeout(500);

    expect(deleteRequested).toBe(true);
  });

  test('format toggle switches between YAML and JSON in create modal', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('.accordion', { timeout: 10000 });

    await page.locator('.ok-btn:has-text("+")').click();
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
});
