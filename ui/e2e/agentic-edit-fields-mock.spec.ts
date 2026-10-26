import { test, Page } from '@playwright/test';

import { expect, fulfillJson, mockEntity, mockEntityList } from './agentic-mocks';
import { setupMockAuth } from './helpers';

const WORKFLOW_ID = '00000000-0000-0000-0000-00000000fe01';
const TOOLSET_ID = '00000000-0000-0000-0000-00000000fe02';
const WORKFLOW = mockEntity('WorkflowCatalog', WORKFLOW_ID, 'triage', {
  entry_agent: null,
  auto_instance: false,
  limits: {},
  default_models: [],
  model_auth: { levels: [] },
  agents: [],
  modified: '2026-10-01T00:00:00Z',
  latest_instance: null,
  newest_instance: null,
  highest_revision: 0,
});
const TOOLSET = mockEntity('ToolSet', TOOLSET_ID, 'static', {
  read_only: false,
  selector: null,
  auto_instance: false,
  tools: [],
  modified: '2026-10-01T00:00:00Z',
  latest_instance: null,
  highest_revision: 0,
});

/**
 * Serve one workflow and one toolset that stay unchanged, and accept every update.
 *
 * @param page - The page whose requests are mocked.
 */
async function mockUnchanged(page: Page) {
  await setupMockAuth(page);
  await mockEntityList(page, []);
  await page.route('**/api/entities/*', (route) => {
    if (route.request().method() === 'PATCH') return route.fulfill({ status: 204 });
    const id = new URL(route.request().url()).pathname.split('/').pop();
    return fulfillJson(route, id === WORKFLOW_ID ? WORKFLOW : TOOLSET);
  });
}

test.describe('unsaved edits survive a save of another field', () => {
  test('the workflow keeps typed default models when the description is saved', async ({ page }) => {
    await mockUnchanged(page);
    await page.goto(`/workflows/${WORKFLOW_ID}`);
    await page.getByTestId('edit-models').fill('model-a\nmodel-b');
    await page.getByTestId('edit-alive').fill('7');
    await page.getByTestId('edit-description').fill('new text');
    await page.getByTestId('save-description').click();
    await expect(page.getByText('Description saved')).toBeVisible();
    await expect(page.getByTestId('edit-models')).toHaveValue('model-a\nmodel-b');
    await expect(page.getByTestId('edit-alive')).toHaveValue('7');
  });

  test('the workflow resets only the field that was saved', async ({ page }) => {
    await mockUnchanged(page);
    await page.goto(`/workflows/${WORKFLOW_ID}`);
    await page.getByTestId('edit-models').fill('model-a');
    await page.getByTestId('edit-description').fill('new text');
    await page.getByTestId('save-models').click();
    await expect(page.getByText('Default models saved')).toBeVisible();
    // the saved field shows what is stored again, while the unsaved description is kept
    await expect(page.getByTestId('edit-models')).toHaveValue('');
    await expect(page.getByTestId('edit-description')).toHaveValue('new text');
  });

  test('the workflow keeps typed text when a toggle is clicked', async ({ page }) => {
    await mockUnchanged(page);
    await page.goto(`/workflows/${WORKFLOW_ID}`);
    await page.getByTestId('edit-models').fill('model-a');
    await page.getByTestId('toggle-auto-instance').click();
    await expect(page.getByText('Auto instance changed')).toBeVisible();
    await expect(page.getByTestId('edit-models')).toHaveValue('model-a');
  });

  test('the workflow reload resets every field', async ({ page }) => {
    await mockUnchanged(page);
    await page.goto(`/workflows/${WORKFLOW_ID}`);
    await page.getByTestId('edit-models').fill('model-a');
    await page.getByTestId('edit-description').fill('new text');
    await page.getByTestId('reload').click();
    await expect(page.getByTestId('edit-models')).toHaveValue('');
    await expect(page.getByTestId('edit-description')).toHaveValue('');
  });

  test('the toolset keeps a typed selector when the description is saved or a toggle is clicked', async ({ page }) => {
    await mockUnchanged(page);
    await page.goto(`/toolsets/${TOOLSET_ID}`);
    await page.getByTestId('edit-selector-key').fill('team');
    await page.getByTestId('edit-selector-value').fill('blue');
    await page.getByTestId('edit-description').fill('new text');
    await page.getByTestId('save-description').click();
    await expect(page.getByText('Description saved')).toBeVisible();
    await expect(page.getByTestId('edit-selector-key')).toHaveValue('team');
    await expect(page.getByTestId('edit-selector-value')).toHaveValue('blue');
    await page.getByTestId('toggle-read-only').click();
    await expect(page.getByText('Read only changed')).toBeVisible();
    await expect(page.getByTestId('edit-selector-key')).toHaveValue('team');
  });

  test('the toolset keeps a typed description when the selector is saved', async ({ page }) => {
    await mockUnchanged(page);
    await page.goto(`/toolsets/${TOOLSET_ID}`);
    await page.getByTestId('edit-description').fill('new text');
    await page.getByTestId('edit-selector-key').fill('team');
    await page.getByTestId('edit-selector-value').fill('blue');
    await page.getByTestId('save-selector').click();
    await expect(page.getByText('Selector saved')).toBeVisible();
    await expect(page.getByTestId('edit-description')).toHaveValue('new text');
  });
});
