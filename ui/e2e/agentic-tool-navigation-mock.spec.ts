import { test, Page } from '@playwright/test';

import { expect, fulfillJson, MOCK_GROUP, mockEntity, mockInstanceSummary } from './agentic-mocks';
import { MOCK_USER, setupMockAuth } from './helpers';

const FIRST_ID = '00000000-0000-0000-0000-00000000ef01';
const SECOND_ID = '00000000-0000-0000-0000-00000000ef02';

/**
 * Serve two tools whose reads are slow, where the first tool's version read fails after the page has moved on.
 *
 * @param page - The page whose requests are mocked.
 */
async function mockTwoTools(page: Page) {
  const tools = [
    { id: FIRST_ID, name: 'first-tool', version: summary(FIRST_ID, '1.0.0') },
    { id: SECOND_ID, name: 'second-tool', version: summary(SECOND_ID, '2.0.0') },
  ];
  await setupMockAuth(page);
  await page.route('**/api/pipelines/list/**', (route) => fulfillJson(route, { details: [] }));
  await page.route('**/api/groups/details/**', (route) => fulfillJson(route, { details: [] }));
  for (const tool of tools) {
    await page.route(`**/api/entities/tools/${tool.id}/instances`, (route) => fulfillJson(route, [tool.version]));
  }
  await page.route('**/api/entities/*', async (route) => {
    const id = new URL(route.request().url()).pathname.split('/').pop() ?? '';
    const tool = tools.find((entry) => entry.id === id);
    if (tool) {
      // the second tool is slow to load so the page is still showing the first tool's content while it loads
      if (tool.id === SECOND_ID) await new Promise((resolve) => setTimeout(resolve, 1500));
      return fulfillJson(
        route,
        mockEntity('Tool', tool.id, tool.name, {
          kind: 'Image',
          latest: tool.version.id,
          latest_version: tool.version.version,
          instances: [tool.version],
        }),
      );
    }
    const owner = tools.find((entry) => entry.version.id === id);
    if (owner?.id === FIRST_ID) {
      // the first tool's version read fails after the user has moved to the second tool
      await new Promise((resolve) => setTimeout(resolve, 800));
      return fulfillJson(route, { error: 'injected failure for the first tool spec' }, 500);
    }
    return fulfillJson(
      route,
      mockEntity('ToolInstance', id, `${owner?.name ?? 'tool'}@${owner?.version.version ?? ''}`, {
        tool: owner?.id ?? '',
        version: owner?.version.version ?? '',
        previous: null,
        spec_hash: owner?.version.spec_hash ?? '',
        deprecated: false,
        spec: { Image: { marker: 'spec' } },
      }),
    );
  });
}

/**
 * Build the summary of a tool's only version.
 *
 * @param toolId - The tool's id.
 * @param version - The version label.
 */
function summary(toolId: string, version: string) {
  return mockInstanceSummary(`${toolId}-inst`, version);
}

test.describe('tool page navigation', () => {
  test("another tool's page starts clean and does not show the previous tool's read error", async ({ page }) => {
    await mockTwoTools(page);
    await page.goto(`/tools/${FIRST_ID}`);
    await expect(page.getByTestId('tool-title')).toHaveText('first-tool');
    // move to the second tool inside the app while the first tool's version read is still outstanding
    await page.evaluate((id) => {
      window.history.pushState({}, '', `/tools/${id}`);
      window.dispatchEvent(new PopStateEvent('popstate'));
    }, SECOND_ID);
    // the first tool's content is gone while the second loads
    await expect(page.getByTestId('tool-title')).toHaveCount(0);
    await expect(page.getByTestId('tool-title')).toHaveText('second-tool');
    // wait past the moment the first tool's read fails
    await page.waitForTimeout(1500);
    await expect(page.getByText('injected failure for the first tool spec')).toHaveCount(0);
  });
});
