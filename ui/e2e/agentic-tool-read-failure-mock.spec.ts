import { test, Page } from '@playwright/test';

import { expect, fulfillJson, mockEntity, mockInstanceSummary } from './agentic-mocks';
import { setupMockAuth } from './helpers';

// Mocked tests: a failed read of the tool list or of one tool is reported as a failure, not as an empty list or a missing tool.

/** The id of a tool whose reads fail until a test lets them succeed. */
const TOOL_ID = '00000000-0000-0000-0000-00000000ef01';
/** The id of the tool's only version. */
const INSTANCE_ID = '00000000-0000-0000-0000-00000000ef02';

/**
 * Serve one tool with root tags and one version, answering its reads with a server error while the returned switch is off.
 *
 * @param page - The page whose requests are mocked.
 * @returns A function that lets the tool's reads succeed from then on.
 */
async function mockFlakyTool(page: Page): Promise<() => void> {
  const version = mockInstanceSummary(INSTANCE_ID, '1.0.0');
  let failing = true;
  await setupMockAuth(page);
  await page.route('**/api/pipelines/list/**', (route) => fulfillJson(route, { details: [] }));
  await page.route(`**/api/entities/tools/${TOOL_ID}/instances`, (route) => fulfillJson(route, [version]));
  await page.route('**/api/entities/*', (route) => {
    const id = new URL(route.request().url()).pathname.split('/').pop() ?? '';
    if (id === TOOL_ID) {
      if (failing) return fulfillJson(route, { error: 'injected server failure' }, 500);
      return fulfillJson(route, {
        ...mockEntity('Tool', TOOL_ID, 'flaky-tool', {
          kind: 'Image',
          latest: version.id,
          latest_version: version.version,
          latest_spec_hash: version.spec_hash,
          modified: '2026-10-01T00:00:00Z',
          instances: [version],
        }),
        tags: { submitter: { europa: [] } },
      });
    }
    if (id === INSTANCE_ID) {
      return fulfillJson(
        route,
        mockEntity('ToolInstance', id, 'flaky-tool@1.0.0', {
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
  return () => {
    failing = false;
  };
}

test.describe('a failed tool read', () => {
  test('says the tool could not be loaded and a retry shows the tool with labelled tags', async ({ page }) => {
    const recover = await mockFlakyTool(page);
    await page.goto(`/tools/${TOOL_ID}`);
    await expect(page.getByTestId('entity-not-found-title')).toHaveText('Tool could not be loaded');
    await expect(page.getByTestId('entity-not-found')).not.toContainText('does not exist');
    await expect(page.getByTestId('entity-not-found')).toContainText('injected server failure');
    recover();
    await page.getByTestId('entity-retry').click();
    await expect(page.getByTestId('tool-title')).toHaveText('flaky-tool');
    await expect(page.getByTestId('tool-tags')).toContainText('Tool tags');
  });

  test('a malformed tool id is reported as not found, with no retry', async ({ page }) => {
    await setupMockAuth(page);
    await page.route('**/api/entities/*', (route) =>
      fulfillJson(route, { error: 'Invalid URL: Cannot parse `id` with value `not-a-uuid`' }, 400),
    );
    await page.goto('/tools/not-a-uuid');
    await expect(page.getByTestId('entity-not-found-title')).toHaveText('Tool not found');
    await expect(page.getByTestId('entity-not-found')).toContainText('Cannot parse');
    await expect(page.getByTestId('entity-retry')).toHaveCount(0);
  });

  test('a failed tool list shows its error without a zero count or an empty-state message', async ({ page }) => {
    await setupMockAuth(page);
    await page.route('**/api/entities/details/**', (route) => fulfillJson(route, { error: 'injected list failure' }, 500));
    await page.goto('/tools');
    await expect(page.getByText('injected list failure').first()).toBeVisible();
    await expect(page.getByTestId('no-tools')).toHaveCount(0);
    await expect(page.getByTestId('tool-count')).toHaveCount(0);
  });
});
