import { test, expect, Page } from '@playwright/test';
import path from 'path';
import { mockEntity, mockEntityList } from './agentic-mocks';
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

// the Image tools that can back a stage added on the diagram
const MOCK_GROUP_TOOLS = ['tool-a', 'tool-b', 'tool-c', 'scanner', 'extra-tool', 'analyzer'].map((name, index) =>
  mockEntity('Tool', `00000000-0000-0000-0000-00000000dd0${index}`, name, { kind: 'Image', latest: null, modified: '' }),
);

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
  await mockEntityList(page, MOCK_GROUP_TOOLS);
}

test.describe('Pipelines Page', () => {
  test.beforeEach(async ({ page }) => {
    await setupMockAuth(page);
    await setupPipelineMocks(page);
  });

  test('pipeline list renders with correct count', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    const badge = page.locator('[data-testid="pipeline-count"]');
    await expect(badge).toContainText('2');

    const items = page.locator('[data-testid="accordion-item"]');
    await expect(items).toHaveCount(2);

    await snapshot(page, SCREENSHOT_DIR, 'pipelines-list');
  });

  test('shows a no-results banner when there are no pipelines', async ({ page }) => {
    // re-register the pipeline mocks with an empty list (last route registered wins)
    await setupPipelineMocks(page, []);
    await page.goto('/pipelines');

    await expect(page.getByText('No Pipelines Found')).toBeVisible();
    await expect(page.locator('[data-testid="accordion-item"]')).toHaveCount(0);
    await snapshot(page, SCREENSHOT_DIR, 'pipelines-no-results');
  });

  test('pipeline names and groups display correctly', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await expect(page.locator('.accordion-item-name .text').first()).toContainText('test-pipeline');
    await expect(page.locator('.accordion-item-subtitle i').first()).toContainText('system');
  });

  test('expand pipeline shows details', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForTimeout(500);

    // the details show the creator's name under a Creator heading
    await expect(page.getByText('Creator', { exact: true })).toBeVisible();
    await expect(page.getByText('test', { exact: true }).first()).toBeVisible();
    await expect(page.locator('text=A test pipeline for E2E testing')).toBeVisible();
    await expect(page.locator('text=604800')).toBeVisible();

    await snapshot(page, SCREENSHOT_DIR, 'pipeline-details-expanded');
  });

  test('pipeline with NewSample trigger displays trigger type', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').nth(1).click();
    await page.waitForTimeout(500);

    await expect(page.locator('text=new-sample-trigger')).toBeVisible();
    await expect(page.locator('text=NewSample')).toBeVisible();
  });

  test('edit button enters edit mode with form/editor toggle', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForTimeout(500);

    await page.locator('[data-testid="header-btn-edit"]').first().click();
    await page.waitForTimeout(500);

    await expect(page.locator('[data-testid="header-btn-discard"]').first()).toBeVisible();
    await expect(page.locator('[data-testid="header-btn-accept"]').first()).toBeVisible();

    // edit defaults to the form view; switching to the editor view shows the code editor
    await page.locator('button:has-text("Editor")').first().click();
    await waitForEditor(page);

    await snapshot(page, SCREENSHOT_DIR, 'pipeline-edit-mode');
  });

  test('cancel edit returns to view mode', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForTimeout(500);

    await page.locator('[data-testid="header-btn-edit"]').first().click();
    await page.waitForTimeout(500);
    await expect(page.locator('[data-testid="header-btn-discard"]').first()).toBeVisible();

    await page.locator('[data-testid="header-btn-discard"]').first().click();
    await page.waitForTimeout(300);

    await expect(page.locator('.cm-editor')).toHaveCount(0);
    await expect(page.locator('text=A test pipeline for E2E testing')).toBeVisible();
  });

  test('edit form section labels stack tightly above their fields at 1024px', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 900 });
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForTimeout(500);

    await page.locator('[data-testid="header-btn-edit"]').first().click();
    await expect(page.locator('[data-testid="header-btn-discard"]').first()).toBeVisible();

    // below the xl breakpoint each section stacks its label over its fields; the label's
    // fixed-width basis must not turn into a tall blank block
    for (const label of ['Order', 'Triggers']) {
      const labelCol = page.getByText(label, { exact: true }).first().locator('xpath=ancestor::*[contains(@class, "EditMiddle")][1]');
      const box = await labelCol.boundingBox();
      expect(box).not.toBeNull();
      expect(box!.height).toBeLessThan(60);
    }
  });

  test('triggers hover tip stays inside the viewport at 1024px', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 900 });
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });
    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForTimeout(500);
    await page.locator('[data-testid="header-btn-edit"]').first().click();
    await expect(page.locator('[data-testid="header-btn-discard"]').first()).toBeVisible();

    // hover the add-trigger button inside the full-width triggers box
    const addTrigger = page.getByRole('button', { name: 'Add trigger' }).first();
    await addTrigger.scrollIntoViewIfNeeded();
    await addTrigger.hover();
    const tip = page.locator('.tip-fixed', { hasText: 'Automatic triggers' });
    await expect(tip).toBeVisible();

    // the tip must not run past either edge of the viewport
    const box = await tip.boundingBox();
    expect(box).not.toBeNull();
    expect(box!.x).toBeGreaterThanOrEqual(0);
    expect(box!.x + box!.width).toBeLessThanOrEqual(1024);
  });

  test('triggers hover tip stays in view when the triggers box is taller than the viewport', async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 700 });
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });
    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForTimeout(500);
    await page.locator('[data-testid="header-btn-edit"]').first().click();
    await expect(page.locator('[data-testid="header-btn-discard"]').first()).toBeVisible();

    // grow the triggers box well past the viewport height
    const addTrigger = page.getByRole('button', { name: 'Add trigger' }).first();
    for (let i = 0; i < 4; i++) {
      await addTrigger.click();
    }
    // park the add button at the bottom edge of the viewport so there is little room below it
    await addTrigger.evaluate((el) => el.scrollIntoView({ block: 'end' }));
    await addTrigger.hover();
    const tip = page.locator('.tip-fixed', { hasText: 'Automatic triggers' });
    await expect(tip).toBeVisible();

    // the tip must be fully visible vertically and horizontally
    const box = await tip.boundingBox();
    expect(box).not.toBeNull();
    expect(box!.y).toBeGreaterThanOrEqual(0);
    expect(box!.y + box!.height).toBeLessThanOrEqual(700);
    expect(box!.x).toBeGreaterThanOrEqual(0);
    expect(box!.x + box!.width).toBeLessThanOrEqual(1440);
  });

  test('delete pipeline shows confirmation modal', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForTimeout(500);

    await page.locator('[data-testid="header-btn-delete"]').first().click();
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
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForTimeout(500);

    await page.locator('[data-testid="header-btn-delete"]').first().click();
    await page.waitForTimeout(300);

    await page.locator('.danger-btn:has-text("Confirm")').click();
    await page.waitForTimeout(500);

    expect(deleteRequested).toBe(true);
  });

  test('pipeline order flow renders with correct nodes', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForSelector('.react-flow', { timeout: 5000 });
    await page.waitForTimeout(500);

    const firstBody = page.locator('[data-testid="accordion-item"]').first().locator('[data-testid="accordion-body"]');
    const imageNodes = firstBody.locator('.react-flow__node-imageStep');
    await expect(imageNodes).toHaveCount(3);

    const terminalNodes = firstBody.locator('.react-flow__node-terminal');
    await expect(terminalNodes).toHaveCount(2);
  });

  test('pipeline order flow displays correct image names', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForSelector('.react-flow__node', { timeout: 5000 });
    await page.waitForTimeout(500);

    const nodeTexts = await page.locator('.react-flow__node-imageStep').allTextContents();
    expect(nodeTexts).toContain('tool-a');
    expect(nodeTexts).toContain('tool-b');
    expect(nodeTexts).toContain('tool-c');
  });

  test('pipeline order flow renders edges between steps', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForSelector('.react-flow__node', { timeout: 5000 });
    await page.waitForTimeout(500);

    const edges = page.locator('.react-flow__edge');
    await expect(edges).toHaveCount(6);
  });

  test('pipeline order flow is properly sized within container', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForSelector('.react-flow__node', { timeout: 5000 });
    await page.waitForTimeout(800);

    const flowContainer = page.locator('.react-flow').first();
    const containerBox = await flowContainer.boundingBox();
    expect(containerBox).not.toBeNull();
    expect(containerBox!.width).toBeGreaterThan(100);
    expect(containerBox!.height).toBeGreaterThan(50);

    const firstNode = page.locator('.react-flow__node-imageStep').first();
    const nodeBox = await firstNode.boundingBox();
    expect(nodeBox).not.toBeNull();

    expect(nodeBox!.x).toBeGreaterThanOrEqual(containerBox!.x);
    expect(nodeBox!.y).toBeGreaterThanOrEqual(containerBox!.y);

    await snapshot(page, SCREENSHOT_DIR, 'pipeline-order-flow');
  });

  test('pipeline order flow survives edit round-trip', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    const firstBody = page.locator('[data-testid="accordion-item"]').first().locator('[data-testid="accordion-body"]');
    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForSelector('.react-flow__node', { timeout: 5000 });

    await page.locator('[data-testid="header-btn-edit"]').first().click();
    await page.waitForTimeout(500);

    // the editor view replaces the order diagram with the code editor
    await page.locator('button:has-text("Editor")').first().click();
    await waitForEditor(page);
    await expect(firstBody.locator('.react-flow')).toHaveCount(0);

    await page.locator('[data-testid="header-btn-discard"]').first().click();
    await page.waitForSelector('.react-flow__node', { timeout: 5000 });
    await page.waitForTimeout(500);

    const imageNodes = firstBody.locator('.react-flow__node-imageStep');
    await expect(imageNodes).toHaveCount(3);
  });

  test('single-step pipeline order flow renders one node', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').nth(1).click();
    await page.waitForTimeout(1000);

    const secondBody = page.locator('[data-testid="accordion-item"]').nth(1).locator('[data-testid="accordion-body"]');
    await expect(secondBody).toBeVisible({ timeout: 5000 });

    const imageNodes = secondBody.locator('.react-flow__node-imageStep');
    await expect(imageNodes).toHaveCount(1);

    const nodeText = await imageNodes.first().textContent();
    expect(nodeText).toContain('scanner');

    const terminalNodes = secondBody.locator('.react-flow__node-terminal');
    await expect(terminalNodes).toHaveCount(2);

    const edges = secondBody.locator('.react-flow__edge');
    await expect(edges).toHaveCount(2);

    await snapshot(page, SCREENSHOT_DIR, 'pipeline-single-step-flow');
  });

  test('right-click on empty pane shows context menu with Insert stage', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForSelector('.react-flow', { timeout: 5000 });
    await page.waitForTimeout(500);

    const flow = page.locator('.react-flow').first();
    await flow.click({ button: 'right', position: { x: 50, y: 50 } });
    await page.waitForTimeout(300);

    const contextMenu = page.locator('div:has-text("Insert stage")').filter({ hasText: 'Insert stage' }).first();
    await expect(contextMenu).toBeVisible();
  });

  test('right-click on image node shows context menu with Insert and Remove options', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForSelector('.react-flow__node-imageStep', { timeout: 5000 });
    await page.waitForTimeout(500);

    const node = page.locator('.react-flow__node-imageStep').first();
    await node.click({ button: 'right' });
    await page.waitForTimeout(300);

    await expect(page.locator('text=Insert stage')).toBeVisible();
    await expect(page.locator('text=Remove stage')).toBeVisible();
  });

  test('context menu dismissed on Escape', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForSelector('.react-flow', { timeout: 5000 });
    await page.waitForTimeout(500);

    const flow = page.locator('.react-flow').first();
    await flow.click({ button: 'right', position: { x: 50, y: 50 } });
    await page.waitForTimeout(300);
    await expect(page.locator('text=Insert stage')).toBeVisible();

    await page.keyboard.press('Escape');
    await page.waitForTimeout(300);
    await expect(page.locator('text=Insert stage')).not.toBeVisible();
  });

  test('Insert stage opens select dropdown with the group tools', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForSelector('.react-flow', { timeout: 5000 });
    await page.waitForTimeout(500);

    const flow = page.locator('.react-flow').first();
    await flow.click({ button: 'right', position: { x: 50, y: 50 } });
    await page.waitForTimeout(300);

    await page.locator('text=Insert stage').click();
    await page.waitForTimeout(300);

    // the select lists the group's Image tools, each to follow at latest; a tool can back several stages
    const selectMenu = page.locator('[class*="menu"]');
    await expect(selectMenu.first()).toBeVisible();
    const optionTexts = await page.locator('[class*="option"]').allTextContents();
    expect(optionTexts).toContain('extra-tool · follow latest');
    expect(optionTexts).toContain('analyzer · follow latest');
    expect(optionTexts).toContain('tool-a · follow latest');

    await snapshot(page, SCREENSHOT_DIR, 'pipeline-insert-image-select');
  });

  test('selecting a tool from dropdown adds a stage to the diagram and shows Apply bar', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForSelector('.react-flow__node-imageStep', { timeout: 5000 });
    await page.waitForTimeout(500);

    const firstBody = page.locator('[data-testid="accordion-item"]').first().locator('[data-testid="accordion-body"]');

    // Start with 3 image nodes
    await expect(firstBody.locator('.react-flow__node-imageStep')).toHaveCount(3);

    const flow = page.locator('.react-flow').first();
    await flow.click({ button: 'right', position: { x: 50, y: 50 } });
    await page.waitForTimeout(300);

    await page.locator('text=Insert stage').click();
    await page.waitForTimeout(300);

    // Click an available option
    await page.locator('[class*="option"]', { hasText: 'extra-tool · follow latest' }).click();
    await page.waitForTimeout(500);

    // Should now have 4 image nodes
    await expect(firstBody.locator('.react-flow__node-imageStep')).toHaveCount(4);

    // Apply/Discard bar should appear
    await expect(page.locator('button:has-text("Apply")')).toBeVisible();
    await expect(page.locator('button:has-text("Discard")')).toBeVisible();

    await snapshot(page, SCREENSHOT_DIR, 'pipeline-image-inserted');
  });

  test('Remove stage removes a node and shows Apply bar', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForSelector('.react-flow__node-imageStep', { timeout: 5000 });
    await page.waitForTimeout(500);

    const firstBody = page.locator('[data-testid="accordion-item"]').first().locator('[data-testid="accordion-body"]');
    await expect(firstBody.locator('.react-flow__node-imageStep')).toHaveCount(3);

    // Right-click on a node to get Remove stage option
    const node = firstBody.locator('.react-flow__node-imageStep').first();
    await node.click({ button: 'right' });
    await page.waitForTimeout(300);

    await page.locator('text=Remove stage').click();
    await page.waitForTimeout(500);

    // Should now have 2 image nodes
    await expect(firstBody.locator('.react-flow__node-imageStep')).toHaveCount(2);

    // Apply/Discard bar should appear
    await expect(page.locator('button:has-text("Apply")')).toBeVisible();

    await snapshot(page, SCREENSHOT_DIR, 'pipeline-image-removed');
  });

  test('Discard reverts pending diagram changes', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForSelector('.react-flow__node-imageStep', { timeout: 5000 });
    await page.waitForTimeout(500);

    const firstBody = page.locator('[data-testid="accordion-item"]').first().locator('[data-testid="accordion-body"]');
    await expect(firstBody.locator('.react-flow__node-imageStep')).toHaveCount(3);

    // Remove an image
    const node = firstBody.locator('.react-flow__node-imageStep').first();
    await node.click({ button: 'right' });
    await page.waitForTimeout(300);
    await page.locator('text=Remove stage').click();
    await page.waitForTimeout(500);
    await expect(firstBody.locator('.react-flow__node-imageStep')).toHaveCount(2);

    // Click Discard
    await page.locator('button:has-text("Discard")').click();
    await page.waitForTimeout(500);

    // Should be back to 3 nodes
    await expect(firstBody.locator('.react-flow__node-imageStep')).toHaveCount(3);
    await expect(page.locator('button:has-text("Apply")')).not.toBeVisible();
  });

  test('double-click on empty pane opens the tool select dropdown', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForSelector('.react-flow', { timeout: 5000 });
    await page.waitForTimeout(500);

    const flow = page.locator('.react-flow').first();
    await flow.dblclick({ position: { x: 50, y: 50 } });
    await page.waitForTimeout(300);

    // Select dropdown should be visible
    const selectMenu = page.locator('[class*="menu"]');
    await expect(selectMenu.first()).toBeVisible();
  });

  test('select dropdown dismissed on Escape', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForSelector('.react-flow', { timeout: 5000 });
    await page.waitForTimeout(500);

    const flow = page.locator('.react-flow').first();
    await flow.dblclick({ position: { x: 50, y: 50 } });
    await page.waitForTimeout(300);

    const selectMenu = page.locator('[class*="menu"]');
    await expect(selectMenu.first()).toBeVisible();

    await page.keyboard.press('Escape');
    await page.waitForTimeout(300);

    await expect(selectMenu).not.toBeVisible();
  });

  test('edit mode order note describes adding, inserting, removing, and reordering stages', async ({ page }) => {
    await page.goto('/pipelines');
    await page.waitForSelector('[data-testid="accordion"]', { timeout: 10000 });

    await page.locator('[data-testid="accordion-header"]').first().click();
    await page.waitForSelector('.react-flow__node-imageStep', { timeout: 5000 });
    await page.locator('[data-testid="header-btn-edit"]').first().click();

    await expect(page.locator('text=Double-click to add a stage backed by a tool').first()).toBeVisible();
  });
});
