import { test, Page } from '@playwright/test';

import { expect, fulfillJson, MOCK_GROUP, mockEntity, mockEntityList, mockInstanceSummary } from './agentic-mocks';
import { MOCK_USER, setupMockAuth } from './helpers';

// the API leaves out empty lists (`skip_serializing_if = "Vec::is_empty"`), so these mocks omit them too
const EMPTY_TOOL_ID = '00000000-0000-0000-0000-0000000000e1';
const LIVE_TOOL_ID = '00000000-0000-0000-0000-0000000000a1';
const LIVE_INSTANCE_ID = '00000000-0000-0000-0000-0000000000a2';
const RUN_ID = '00000000-0000-0000-0000-0000000000c1';
const SHA256 = 'a'.repeat(64);
const RENDER_ERROR = 'An error occurred while rendering';

/** A tool whose last version was deleted: the API sends no `instances` key at all. */
const EMPTY_TOOL = mockEntity('Tool', EMPTY_TOOL_ID, 'emptied', {
  kind: 'Image',
  latest: null,
  latest_version: null,
  latest_spec_hash: null,
  modified: '2026-10-01T00:00:00Z',
});

/** A tool with one live version. */
const LIVE_TOOL = mockEntity('Tool', LIVE_TOOL_ID, 'scanner', {
  kind: 'Image',
  latest: LIVE_INSTANCE_ID,
  latest_version: { SemVer: '1.0.0' },
  latest_spec_hash: 'hash',
  modified: '2026-10-01T00:00:00Z',
  instances: [mockInstanceSummary(LIVE_INSTANCE_ID, '1.0.0')],
});

/**
 * Serve both tools from the entity routes the tools pages read.
 *
 * @param page - The page whose requests are mocked.
 */
async function mockTools(page: Page) {
  await setupMockAuth(page);
  await mockEntityList(page, [EMPTY_TOOL, LIVE_TOOL]);
  await page.route(`**/api/entities/${EMPTY_TOOL_ID}`, (route) => fulfillJson(route, EMPTY_TOOL));
  await page.route(`**/api/entities/tools/${EMPTY_TOOL_ID}/instances`, (route) => fulfillJson(route, []));
  await page.route(`**/api/pipelines/list/**`, (route) => fulfillJson(route, { details: [] }));
}

test.describe('agentic pages with the API wire shapes', () => {
  test('the tools list shows a tool with no versions', async ({ page }) => {
    await mockTools(page);
    await page.goto('/tools');
    await expect(page.getByTestId('tool-row-scanner')).toBeVisible();
    await expect(page.getByTestId('tool-row-emptied')).toBeVisible();
    await expect(page.getByText(RENDER_ERROR)).toHaveCount(0);
  });

  test('the details page of a tool with no versions renders', async ({ page }) => {
    await mockTools(page);
    await page.goto(`/tools/${EMPTY_TOOL_ID}`);
    await expect(page.getByTestId('tool-title')).toHaveText('emptied');
    await expect(page.getByTestId('tool-latest')).toContainText('no live version');
    await expect(page.getByText(RENDER_ERROR)).toHaveCount(0);
  });

  test('a pipeline copy opens while a visible tool has no versions', async ({ page }) => {
    await mockTools(page);
    await page.goto('/');
    await expect(page.getByText(`@${MOCK_USER.username}`)).toBeVisible();
    // open the create page with a source pipeline in the router state, as the list's Copy action does
    await page.evaluate(
      ({ group, tool }) => {
        const source = { group, name: 'source', order: ['scan'], sla: 3600, stage_tools: { scan: tool }, triggers: {} };
        window.history.pushState({ usr: source, key: 'copy', idx: 1 }, '', '/create/pipeline');
        window.dispatchEvent(new PopStateEvent('popstate', { state: window.history.state }));
      },
      { group: MOCK_GROUP, tool: LIVE_TOOL_ID },
    );
    await expect(page.getByTestId('stage-name-0-0')).toHaveValue('scan');
    await expect(page.getByTestId('stage-tool-0-0')).toHaveValue(LIVE_TOOL_ID);
    await expect(page.getByText(RENDER_ERROR)).toHaveCount(0);
  });

  test('the run details page shows a result that has no files', async ({ page }) => {
    await setupMockAuth(page);
    const provenance = { agents: [], tools: [] };
    const run = {
      id: RUN_ID,
      group: MOCK_GROUP,
      creator: 'test',
      pipeline: 'scanner',
      status: 'Completed',
      current_stage: 1,
      current_stage_progress: 0,
      current_stage_length: 0,
      args: {},
      sla: '2099-01-01T00:00:00Z',
      jobs: [],
      tags: [],
      sub_reactions: 0,
      completed_sub_reactions: 0,
      generators: [],
      samples: [SHA256],
      ephemeral: [],
      parent_ephemeral: {},
      repos: [],
      has_cache: false,
      source: { Tool: { tool_instance: LIVE_INSTANCE_ID, provenance } },
      plan: { stages: [[{ name: 'scanner', tool_instance: LIVE_INSTANCE_ID }]] },
    };
    await page.route(`**/api/reactions/${MOCK_GROUP}/${RUN_ID}`, (route) => fulfillJson(route, run));
    await page.route(`**/api/reactions/logs/**`, (route) => fulfillJson(route, []));
    // a JSON-only result: the API sends no `files` key
    await page.route(`**/api/files/results/${SHA256}**`, (route) =>
      fulfillJson(route, { results: { scanner: [{ id: 'r1', result: { verdict: 'clean' } }] } }),
    );
    await page.goto(`/runs/${MOCK_GROUP}/${RUN_ID}`);
    await expect(page.getByTestId('run-result-json')).toContainText('clean');
    await expect(page.getByTestId('run-result-files')).toHaveCount(0);
    await expect(page.getByText(RENDER_ERROR)).toHaveCount(0);
  });
});
