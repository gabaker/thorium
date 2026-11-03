import { test, Page } from '@playwright/test';

import { expect, fulfillJson, mockEntity, mockEntityList } from './agentic-mocks';
import { MOCK_USER, setupMockAuth } from './helpers';

const GROUP = MOCK_USER.groups[0];
const SET_A = '00000000-0000-0000-0000-00000000c001';
const SET_B = '00000000-0000-0000-0000-00000000c002';
const SET_INSTANCE = '00000000-0000-0000-0000-00000000c003';
const AGENT_FOLLOWS = '00000000-0000-0000-0000-00000000c011';
const AGENT_PINS = '00000000-0000-0000-0000-00000000c012';
const AGENT_UNUSED = '00000000-0000-0000-0000-00000000c013';
const WORKFLOW = '00000000-0000-0000-0000-00000000c021';

const toolSetMeta = { tools: [], selector: null, read_only: false, auto_instance: true, highest_revision: 0 };
const setA = mockEntity('ToolSet', SET_A, 'twin-set', toolSetMeta);
const setB = mockEntity('ToolSet', SET_B, 'twin-set', toolSetMeta);
const setInstance = mockEntity('ToolSetInstance', SET_INSTANCE, 'twin-set#r1', {
  tool_set: SET_A,
  tool_set_groups: [GROUP],
  revision: 1,
  previous: null,
  status: 'Ready',
  warnings: [],
  trigger: 'Manual',
  triggered_by: MOCK_USER.username,
  read_only: false,
  selector: null,
  members: [],
  excluded: [],
  digest: 'digest',
});
const follows = mockEntity('Agent', AGENT_FOLLOWS, 'follows-agent', { config: { models: [] }, tool_sets: [setA] });
const pins = mockEntity('Agent', AGENT_PINS, 'pins-agent', { config: { models: [] }, tool_sets: [setInstance] });
const unused = mockEntity('Agent', AGENT_UNUSED, 'unused-agent', { config: { models: [] }, tool_sets: [setB] });
const workflow = mockEntity('WorkflowCatalog', WORKFLOW, 'used-wf', {
  entry_agent: AGENT_FOLLOWS,
  auto_instance: false,
  limits: {},
  default_models: [],
  model_auth: { levels: [] },
  agents: [follows, pins],
  highest_revision: 0,
});
const ALL = [setA, setB, setInstance, follows, pins, unused, workflow];

/**
 * Sign in a mocked user and serve the toolsets, agents, and workflow, and each of them by id.
 *
 * @param page - The page whose requests are mocked.
 */
async function setup(page: Page) {
  await setupMockAuth(page);
  await mockEntityList(page, ALL);
  await page.route('**/api/entities/*', (route) => {
    const id = new URL(route.request().url()).pathname.split('/').pop() ?? '';
    const found = ALL.find((entity) => entity.id === id);
    return found ? fulfillJson(route, found) : fulfillJson(route, { error: `entity ${id} not found` }, 404);
  });
}

test.describe('toolset, agent, and workflow graph pages', () => {
  test('blocks creating a toolset with only half a selector', async ({ page }) => {
    await setup(page);
    await page.goto('/toolsets/create');
    await page.getByTestId('toolset-name').fill('half-selector');
    await page.getByTestId(`groups-${GROUP}`).check();
    await expect(page.getByTestId('submit-toolset')).toBeEnabled();
    await page.getByTestId('selector-key').fill('team');
    await expect(page.getByTestId('selector-error')).toContainText('selector value');
    await expect(page.getByTestId('submit-toolset')).toBeDisabled();
    await page.getByTestId('selector-value').fill('blue');
    await expect(page.getByTestId('selector-error')).toHaveCount(0);
    await expect(page.getByTestId('submit-toolset')).toBeEnabled();
    await page.getByTestId('selector-key').fill('');
    await expect(page.getByTestId('selector-error')).toContainText('selector key');
    await page.getByTestId('selector-value').fill('');
    await expect(page.getByTestId('submit-toolset')).toBeEnabled();
  });

  test('shows the workflows that use an agent and the agents that use a toolset', async ({ page }) => {
    await setup(page);
    await page.goto(`/agents/${AGENT_FOLLOWS}`);
    await expect(page.getByTestId('agent-used-by').getByRole('link', { name: 'used-wf' })).toBeVisible();
    await page.goto(`/agents/${AGENT_UNUSED}`);
    await expect(page.getByTestId('agent-used-by')).toHaveText('none');
    // an agent that follows the toolset and one that pins an instance of it both use it; one using the twin does not
    await page.goto(`/toolsets/${SET_A}`);
    const usedBy = page.getByTestId('toolset-used-by');
    await expect(usedBy.getByRole('link', { name: 'follows-agent' })).toBeVisible();
    await expect(usedBy.getByRole('link', { name: 'pins-agent' })).toBeVisible();
    await expect(usedBy.getByRole('link', { name: 'unused-agent' })).toHaveCount(0);
  });

  test('counts the workflows of each agent in the agent list', async ({ page }) => {
    await setup(page);
    await page.goto('/agents');
    await expect(page.getByTestId('agent-used-by-follows-agent')).toHaveText('used by 1 workflow');
    await expect(page.getByTestId('agent-used-by-unused-agent')).toHaveText('used by 0 workflows');
  });

  test('tells same-named toolsets apart in the list', async ({ page }) => {
    await setup(page);
    await page.goto('/toolsets');
    await expect(page.getByTestId('short-id')).toHaveCount(2);
    await expect(page.getByTestId('toolset-table')).toContainText('#00000000');
  });

  test('gives the workflow agents table a header row and marks the entry agent', async ({ page }) => {
    await setup(page);
    await page.goto(`/workflows/${WORKFLOW}`);
    const table = page.getByTestId('workflow-agents');
    await expect(table.getByRole('columnheader', { name: 'Agent' })).toBeVisible();
    await expect(table.getByRole('row', { name: /follows-agent/ })).toContainText('entry');
    await expect(table.getByRole('row', { name: /pins-agent/ })).toContainText('member');
  });
});
