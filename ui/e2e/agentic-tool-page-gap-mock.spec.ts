import { test, Page } from '@playwright/test';

import { expect, fulfillJson, mockEntity, mockInstanceSummary } from './agentic-mocks';
import { setupMockAuth } from './helpers';

// Mocked tests: an idle tool page adds no blank space of its own between the title and the Overview card.

/** The id of a tool that exists. */
const TOOL_ID = '00000000-0000-0000-0000-00000000ef01';
/** The id of a tool version. */
const INSTANCE_ID = '00000000-0000-0000-0000-00000000ef02';

/**
 * Serve one tool with one version.
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
        mockEntity('Tool', TOOL_ID, 'gap-tool', {
          kind: 'Image',
          latest: version.id,
          latest_version: version.version,
          latest_spec_hash: version.spec_hash,
          modified: '2026-10-01T00:00:00Z',
          instances: [version],
        }),
      );
    }
    return fulfillJson(
      route,
      mockEntity('ToolInstance', id, 'gap-tool@1.0.0', {
        tool: TOOL_ID,
        version: version.version,
        previous: null,
        spec_hash: version.spec_hash,
        deprecated: false,
        spec: { Image: {} },
      }),
    );
  });
}

test.describe('tool page spacing', () => {
  test('no empty feedback block sits between the title and the Overview card', async ({ page }) => {
    await mockTool(page);
    await page.goto(`/tools/${TOOL_ID}`);
    await expect(page.getByTestId('tool-title')).toHaveText('gap-tool');
    // the element right above the Overview card is the page head, never a zero-height banner wrapper
    const overview = page.locator('section', { has: page.getByRole('heading', { name: 'Overview', exact: true }) }).first();
    await expect(overview).toBeVisible();
    const aboveHeight = await overview.evaluate((el) => el.previousElementSibling?.getBoundingClientRect().height ?? 0);
    expect(aboveHeight).toBeGreaterThan(0);
    await expect(page.getByTestId('tool-notice')).toHaveCount(0);
  });
});
