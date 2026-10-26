import { test } from '@playwright/test';

import { expect, fulfillJson, mockEntity, mockEntityList, mockInstanceSummary } from './agentic-mocks';
import { setupMockAuth } from './helpers';

// a retired tool: its only version is deprecated, so the API sends no latest
const INSTANCE_ID = '00000000-0000-0000-0000-0000000aa001';
const RETIRED = mockEntity('Tool', '00000000-0000-0000-0000-0000000aa000', 'retired', {
  kind: 'Image',
  latest: null,
  latest_version: null,
  latest_spec_hash: null,
  modified: '2026-10-01T00:00:00Z',
  instances: [mockInstanceSummary(INSTANCE_ID, '1.0.0', true)],
});

test.describe('tool launch form', () => {
  test('a tool with every version deprecated shows no version chosen and runs the one picked', async ({ page }) => {
    await setupMockAuth(page);
    await mockEntityList(page, [RETIRED]);
    let ran = '';
    await page.route('**/api/tools/instances/*/runs', (route) => {
      ran = new URL(route.request().url()).pathname;
      return fulfillJson(route, { id: '00000000-0000-0000-0000-0000000aa0ff' });
    });
    await page.goto('/launch?tab=tool');
    await page.getByTestId(`run-tool-${RETIRED.id}`).check();
    const version = page.getByTestId('run-tool-version');
    await expect(version).toHaveValue('');
    await expect(version.locator('option:checked')).toHaveText('Choose a version');
    await version.selectOption(INSTANCE_ID);
    await page.getByTestId('run-samples').fill('c'.repeat(64));
    await page.getByTestId('run-submit').click();
    await expect.poll(() => ran).toContain(INSTANCE_ID);
    await expect(page.getByTestId('launch-error')).toHaveCount(0);
  });

  test('a failed tool list is shown above the picker and is not taken for a launch error', async ({ page }) => {
    await setupMockAuth(page);
    await page.route('**/api/entities/details/**', (route) => fulfillJson(route, { error: 'tool list unavailable' }, 500));
    await page.goto('/launch?tab=tool');
    await expect(page.getByTestId('tools-error')).toBeVisible();
    await expect(page.getByTestId('launch-error')).toHaveCount(0);
  });

  test('a tool list cut off at the page cap says so', async ({ page }) => {
    await setupMockAuth(page);
    // every page of the listing claims there is a next page, so the read stops at the cap
    await page.route('**/api/entities/details/**', (route) => fulfillJson(route, { data: [RETIRED], cursor: 'next' }));
    await page.goto('/launch?tab=tool');
    await expect(page.getByTestId('tools-error')).toContainText('showing only the first');
    await expect(page.getByTestId('launch-error')).toHaveCount(0);
  });
});

test.describe('workflow launch form', () => {
  test('a failed workflow list is shown above the picker and is not taken for a launch error', async ({ page }) => {
    await setupMockAuth(page);
    await page.route('**/api/entities/details/**', (route) => fulfillJson(route, { error: 'workflow list unavailable' }, 500));
    await page.goto('/launch?tab=workflow');
    await expect(page.getByTestId('workflows-error')).toBeVisible();
    await expect(page.getByTestId('launch-error')).toHaveCount(0);
  });

  test('a workflow list cut off at the page cap says so', async ({ page }) => {
    await setupMockAuth(page);
    const workflow = mockEntity('WorkflowCatalog', '00000000-0000-0000-0000-0000000aa100', 'triage', { latest_instance: null });
    // every page of the listing claims there is a next page, so the read stops at the cap
    await page.route('**/api/entities/details/**', (route) => fulfillJson(route, { data: [workflow], cursor: 'next' }));
    await page.goto('/launch?tab=workflow');
    await expect(page.getByTestId('workflows-error')).toContainText('showing only the first');
    await expect(page.getByTestId('launch-error')).toHaveCount(0);
  });
});
