import { test, Page } from '@playwright/test';

import { expect, fulfillJson, MOCK_GROUP, mockEntity, mockEntityList, mockInstanceSummary } from './agentic-mocks';
import { setupMockAuth } from './helpers';

const TOOL_ID = '00000000-0000-0000-0000-00000000fd01';
const OLD_ID = '00000000-0000-0000-0000-00000000fd02';
const NEW_ID = '00000000-0000-0000-0000-00000000fd03';
const HARNESS_ID = '00000000-0000-0000-0000-00000000fd04';
const HARNESS_V1 = '00000000-0000-0000-0000-00000000fd05';
const SET_ID = '00000000-0000-0000-0000-00000000fd06';
const SET_R1 = '00000000-0000-0000-0000-00000000fd07';
const SET_R2 = '00000000-0000-0000-0000-00000000fd08';
const CREATED_ID = '00000000-0000-0000-0000-00000000fd09';
const WORKFLOW_ID = '00000000-0000-0000-0000-00000000fd0a';

/** An agentic tool with a current and an older version. */
const TOOL = mockEntity('Tool', TOOL_ID, 'floss', {
  kind: 'Agentic',
  latest: NEW_ID,
  latest_version: { SemVer: '2.0.0' },
  latest_spec_hash: null,
  modified: '',
  instances: [mockInstanceSummary(NEW_ID, '2.0.0'), mockInstanceSummary(OLD_ID, '1.0.0')],
});

/** A harness with one version. */
const HARNESS = mockEntity('Tool', HARNESS_ID, 'runner', {
  kind: 'Harness',
  latest: HARNESS_V1,
  latest_version: { SemVer: '1.0.0' },
  latest_spec_hash: null,
  modified: '',
  instances: [mockInstanceSummary(HARNESS_V1, '1.0.0')],
});

/** The older floss version as a toolset lists a pinned member. */
const PINNED_OLD = mockEntity('ToolInstance', OLD_ID, 'floss@1.0.0', {
  tool: TOOL_ID,
  version: { SemVer: '1.0.0' },
  previous: null,
  spec_hash: 'h',
  deprecated: false,
  spec: {},
});

/**
 * Build a toolset instance.
 *
 * @param id - The instance id.
 * @param revision - The revision.
 * @param excluded - The tools left out of it.
 * @returns The instance entity.
 */
function setInstance(id: string, revision: number, excluded: unknown[] = []) {
  return mockEntity('ToolSetInstance', id, `pins#r${revision}`, {
    tool_set: SET_ID,
    revision,
    status: 'Ready',
    warnings: [],
    trigger: 'ToolSetUpdated',
    triggered_by: 'tester',
    members: [],
    excluded,
    digest: `v1:${revision}`,
  });
}

/** A read only toolset with one tool at latest and one pinned. */
const SET = mockEntity('ToolSet', SET_ID, 'pins', {
  read_only: true,
  selector: null,
  auto_instance: false,
  tools: [TOOL, PINNED_OLD],
  modified: '2026-10-01T00:00:00Z',
  latest_instance: SET_R2,
  highest_revision: 2,
});

/** A workflow whose only instance was built after the workflow changed. */
const WORKFLOW = mockEntity('WorkflowCatalog', WORKFLOW_ID, 'wf-trigger', {
  entry_agent: null,
  auto_instance: false,
  limits: {},
  default_models: [],
  model_auth: { levels: [] },
  agents: [],
  modified: '2026-10-01T00:00:00Z',
  latest_instance: null,
  newest_instance: null,
  highest_revision: 1,
});

/**
 * Serve the tools, the toolset, its instances, and a workflow, and record every create and update body.
 *
 * @param page - The page whose requests are mocked.
 * @returns The bodies sent so far.
 */
async function mockPins(page: Page): Promise<string[]> {
  const bodies: string[] = [];
  await setupMockAuth(page);
  const effects = {
    tool: 'dumper-id',
    tool_instance: null,
    name: 'dumper',
    inclusion: 'Selector',
    reason: 'NotReadOnly',
    missing_groups: [],
  };
  const workflowInstance = mockEntity('WorkflowInstance', '00000000-0000-0000-0000-00000000fd0b', 'wf-trigger#r1', {
    workflow: WORKFLOW_ID,
    revision: 1,
    status: 'Ready',
    warnings: [],
    trigger: 'WorkflowUpdated',
    triggered_by: 'tester',
  });
  await mockEntityList(page, [TOOL, HARNESS, SET, setInstance(SET_R1, 1), setInstance(SET_R2, 2, [effects]), workflowInstance]);
  await page.route('**/api/entities/*', (route) => {
    const method = route.request().method();
    // a create is a POST to the entity list, and the answer names the new entity
    if (method === 'POST') {
      bodies.push(route.request().postData() ?? '');
      return fulfillJson(route, { id: CREATED_ID });
    }
    if (method === 'PATCH') {
      bodies.push(route.request().postData() ?? '');
      return route.fulfill({ status: 204 });
    }
    const id = new URL(route.request().url()).pathname.split('/').pop();
    return fulfillJson(route, id === WORKFLOW_ID ? WORKFLOW : SET);
  });
  return bodies;
}

test.describe('pinned and latest links', () => {
  test('toolset create sends a pinned tool as its tool instance', async ({ page }) => {
    const bodies = await mockPins(page);
    await page.goto('/toolsets/create');
    await page.getByTestId('toolset-name').fill('pinned-set');
    await page.getByTestId(`groups-${MOCK_GROUP}`).check();
    await page.getByTestId('tools-floss').check();
    const pin = page.getByTestId('tool-pins-floss');
    await expect(pin.locator('option').first()).toHaveText('latest (now 2.0.0)');
    await pin.selectOption({ label: 'pinned 1.0.0' });
    await page.getByTestId('submit-toolset').click();
    await expect.poll(() => bodies.length).toBe(1);
    expect(bodies[0]).toContain(OLD_ID);
    expect(bodies[0]).not.toContain(TOOL_ID);
  });

  test('agent create pins the harness and a toolset instance and keeps a tool at latest', async ({ page }) => {
    const bodies = await mockPins(page);
    await page.goto('/agents/create');
    await page.getByTestId('agent-name').fill('pinned-agent');
    await page.getByTestId(`groups-${MOCK_GROUP}`).check();
    await page.getByTestId('agent-harness').selectOption(HARNESS_ID);
    await page.getByTestId('harness-pins-runner').selectOption({ label: 'pinned 1.0.0' });
    await page.getByTestId('agent-tools-floss').check();
    await page.getByTestId('agent-toolsets-pins').check();
    const setPin = page.getByTestId('toolset-pins-pins');
    await expect(setPin.locator('option').first()).toHaveText('latest (now r2)');
    await setPin.selectOption({ label: 'pinned r1' });
    await page.getByTestId('submit-agent').click();
    await expect.poll(() => bodies.length).toBe(1);
    expect(bodies[0]).toContain(HARNESS_V1);
    expect(bodies[0]).not.toContain(HARNESS_ID);
    expect(bodies[0]).toContain(TOOL_ID);
    expect(bodies[0]).toContain(SET_R1);
    expect(bodies[0]).not.toContain(SET_ID);
  });

  test('toolset details says which explicit tools are pinned and words why tools were left out', async ({ page }) => {
    const bodies = await mockPins(page);
    await page.goto(`/toolsets/${SET_ID}`);
    const tools = page.getByTestId('toolset-tools');
    await expect(tools.locator('thead')).toContainText('Version');
    await expect(tools.locator('tbody tr').nth(0)).toContainText('latest → 2.0.0');
    await expect(tools.locator('tbody tr').nth(1)).toContainText('pinned 1.0.0');
    // the pinned member's link opens its tool
    await expect(tools.locator('tbody tr').nth(1).getByRole('link')).toHaveAttribute('href', `/tools/${TOOL_ID}`);
    const excluded = page.getByTestId('toolset-excluded');
    await expect(excluded.locator('thead')).toContainText('Reason');
    await expect(excluded).toContainText('declares effects, toolset is read only');
    await expect(excluded).not.toContainText('NotReadOnly');
    // floss is already a member, so it isn't offered for adding
    await expect(page.getByTestId('add-tools-floss')).toHaveCount(0);
    expect(bodies).toEqual([]);
  });

  test('the workflow instances table words the trigger', async ({ page }) => {
    await mockPins(page);
    await page.goto(`/workflows/${WORKFLOW_ID}`);
    await expect(page.getByTestId('workflow-instances')).toContainText('Workflow changed');
    await expect(page.getByTestId('workflow-instances')).not.toContainText('WorkflowUpdated');
  });
});
