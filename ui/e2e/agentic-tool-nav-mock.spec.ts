import { test, Page } from '@playwright/test';

import { expect, fulfillJson, mockEntity, mockInstanceSummary } from './agentic-mocks';
import { setupMockAuth } from './helpers';

// Mocked tests: moving between tool ids inside the app never keeps showing the previous tool under the new id.

/** The id of a tool that exists. */
const TOOL_ID = '00000000-0000-0000-0000-00000000ee01';
/** The id of a tool version, which is an entity but not a tool. */
const INSTANCE_ID = '00000000-0000-0000-0000-00000000ee02';
/** An id no entity has. */
const MISSING_ID = '00000000-0000-0000-0000-00000000ee03';

/**
 * Serve one tool, one of its versions, and a 404 for every other id.
 *
 * @param page - The page whose requests are mocked.
 */
async function mockTool(page: Page) {
  const version = mockInstanceSummary(INSTANCE_ID, '1.0.0');
  await setupMockAuth(page);
  await page.route('**/api/pipelines/list/**', (route) => fulfillJson(route, { details: [] }));
  await page.route(`**/api/entities/tools/${TOOL_ID}/instances`, (route) => fulfillJson(route, [version]));
  await page.route('**/api/entities/*', (route) => {
    const id = new URL(route.request().url()).pathname.split('/').pop() ?? '';
    if (id === TOOL_ID) {
      return fulfillJson(
        route,
        mockEntity('Tool', TOOL_ID, 'nav-tool', {
          kind: 'Image',
          latest: version.id,
          latest_version: version.version,
          latest_spec_hash: version.spec_hash,
          modified: '2026-10-01T00:00:00Z',
          instances: [version],
        }),
      );
    }
    if (id === INSTANCE_ID) {
      return fulfillJson(
        route,
        mockEntity('ToolInstance', id, 'nav-tool@1.0.0', {
          tool: TOOL_ID,
          version: version.version,
          previous: null,
          spec_hash: version.spec_hash,
          deprecated: false,
          spec: { Image: { marker: 'spec-of-1.0.0' } },
        }),
      );
    }
    return fulfillJson(route, { error: `entity ${id} not found` }, 404);
  });
}

/**
 * Move to a route without reloading the page, as a link inside the app does.
 *
 * @param page - The page to move.
 * @param path - The route to move to.
 */
async function pushRoute(page: Page, path: string) {
  await page.evaluate((to) => {
    window.history.pushState({}, '', to);
    window.dispatchEvent(new PopStateEvent('popstate'));
  }, path);
}

test.describe('tool details when the id changes in the app', () => {
  test('going back to a missing id shows not found, not the tool just viewed', async ({ page }) => {
    await mockTool(page);
    await page.goto(`/tools/${MISSING_ID}`);
    await expect(page.getByTestId('entity-not-found-title')).toHaveText('Tool not found');
    await pushRoute(page, `/tools/${TOOL_ID}`);
    await expect(page.getByTestId('tool-title')).toHaveText('nav-tool');
    await page.goBack();
    await expect(page).toHaveURL(new RegExp(`${MISSING_ID}$`));
    await expect(page.getByTestId('entity-not-found-title')).toHaveText('Tool not found');
    await expect(page.getByTestId('tool-title')).toHaveCount(0);
    await expect(page.getByTestId('delete-tool')).toHaveCount(0);
  });

  test('moving from a tool to a tool version id says the id is a tool version', async ({ page }) => {
    await mockTool(page);
    await page.goto(`/tools/${TOOL_ID}`);
    await expect(page.getByTestId('tool-title')).toHaveText('nav-tool');
    await pushRoute(page, `/tools/${INSTANCE_ID}`);
    await expect(page.getByTestId('entity-not-found-title')).toHaveText('Tool not found');
    await expect(page.getByTestId('entity-not-found')).toContainText(`${INSTANCE_ID} is a tool version, not a tool`);
    await expect(page.getByTestId('tool-title')).toHaveCount(0);
    await expect(page.getByTestId('delete-tool')).toHaveCount(0);
  });
});
