import { test, Page } from '@playwright/test';

import { expect, fulfillJson, mockEntity, mockEntityList } from './agentic-mocks';
import { setupMockAuth } from './helpers';

const WORKFLOW_ID = '00000000-0000-0000-0000-00000000fe01';
const TOOLSET_ID = '00000000-0000-0000-0000-00000000fe02';
const AGENT_ID = '00000000-0000-0000-0000-00000000fe10';
const REASON = 'entry agent `a-very-long-agent-name-with-no-harness` has no harness to run in and so the build could not finish';
const AGENT = mockEntity('Agent', AGENT_ID, 'lead', {});
const INVALID = { Invalid: { errors: [REASON] } };

/**
 * Build a workflow with one agent.
 *
 * @param entry - The entry agent id, or null for none.
 * @returns The workflow entity.
 */
function workflow(entry: string | null) {
  return mockEntity('WorkflowCatalog', WORKFLOW_ID, 'wf-settings', {
    entry_agent: entry,
    auto_instance: false,
    limits: {},
    default_models: [{ model_id: 'kept', provider: 'OpenAi' }],
    model_auth: { levels: [] },
    agents: [AGENT],
    modified: '2026-10-01T00:00:00Z',
    latest_instance: null,
    newest_instance: null,
    highest_revision: 1,
  });
}

/** A toolset with an invalid first instance. */
const TOOLSET = mockEntity('ToolSet', TOOLSET_ID, 'ts-settings', {
  read_only: false,
  selector: { key: 'family', value: 'demo' },
  auto_instance: false,
  tools: [],
  modified: '2026-10-01T00:00:00Z',
  latest_instance: null,
  highest_revision: 1,
});

/**
 * Serve a workflow, a toolset, and an invalid instance of each, and record every update request body.
 *
 * @param page - The page whose requests are mocked.
 * @param entry - The workflow's entry agent id, or null.
 * @returns The update bodies seen so far.
 */
async function mockRoots(page: Page, entry: string | null): Promise<string[]> {
  const bodies: string[] = [];
  await setupMockAuth(page);
  const instances = [
    mockEntity('WorkflowInstance', '00000000-0000-0000-0000-00000000fe21', 'wf-settings#r1', {
      workflow: WORKFLOW_ID,
      status: INVALID,
      warnings: [],
      trigger: 'Manual',
      triggered_by: 'tester',
    }),
    mockEntity('ToolSetInstance', '00000000-0000-0000-0000-00000000fe22', 'ts-settings#r1', {
      tool_set: TOOLSET_ID,
      revision: 1,
      status: INVALID,
      warnings: [],
      trigger: 'Manual',
      triggered_by: 'tester',
      members: [],
      excluded: [],
      digest: 'v1:abc',
    }),
  ];
  await mockEntityList(page, [AGENT, ...instances]);
  await page.route('**/api/entities/*', (route) => {
    if (route.request().method() === 'PATCH') {
      bodies.push(route.request().postData() ?? '');
      return route.fulfill({ status: 204 });
    }
    const id = new URL(route.request().url()).pathname.split('/').pop();
    return fulfillJson(route, id === WORKFLOW_ID ? workflow(entry) : TOOLSET);
  });
  return bodies;
}

test.describe('workflow and toolset settings', () => {
  test('the entry agent select starts at the current entry and clear needs one', async ({ page }) => {
    await mockRoots(page, AGENT_ID);
    await page.goto(`/workflows/${WORKFLOW_ID}`);
    await expect(page.getByTestId('entry-select')).toHaveValue(AGENT_ID);
    await expect(page.getByTestId('set-entry')).toBeDisabled();
    await expect(page.getByTestId('clear-entry')).toBeEnabled();
  });

  test('clear entry agent is disabled while the workflow has none', async ({ page }) => {
    await mockRoots(page, null);
    await page.goto(`/workflows/${WORKFLOW_ID}`);
    await expect(page.getByTestId('clear-entry')).toBeDisabled();
    await page.getByTestId('entry-select').selectOption(AGENT_ID);
    await expect(page.getByTestId('set-entry')).toBeEnabled();
  });

  test('saves the sub-agent limit, default models, and model auth', async ({ page }) => {
    const bodies = await mockRoots(page, AGENT_ID);
    await page.goto(`/workflows/${WORKFLOW_ID}`);
    await page.getByLabel('Max alive sub-agents').fill('0');
    await expect(page.getByTestId('save-alive')).toBeDisabled();
    await page.getByLabel('Max alive sub-agents').fill('4');
    await page.getByTestId('save-alive').click();
    await expect(page.getByText('Limits saved')).toBeVisible();
    expect(bodies[0]).toContain('{"max_alive_sub_agents":4}');
    await page.getByTestId('edit-models').fill('kept\nextra');
    await page.getByTestId('save-models').click();
    await expect(page.getByText('Default models saved')).toBeVisible();
    // the stored matcher of an id that stays is sent whole
    expect(bodies[1]).toContain('"provider":"OpenAi"');
    expect(bodies[1]).toContain('"model_id":"extra"');
    await page.getByTestId('edit-auth-Group').check();
    await page.getByTestId('save-auth').click();
    await expect(page.getByText('Model auth levels saved')).toBeVisible();
    expect(bodies[2]).toContain('{"levels":["Group"]}');
  });

  test('the toolset description and selector fields have labels', async ({ page }) => {
    await mockRoots(page, null);
    await page.goto(`/toolsets/${TOOLSET_ID}`);
    await expect(page.getByLabel('Description')).toBeVisible();
    await expect(page.getByLabel('Selector key')).toHaveValue('family');
    await expect(page.getByLabel('Selector value')).toHaveValue('demo');
  });

  for (const [path, table] of [
    [`/workflows/${WORKFLOW_ID}`, 'workflow-instances'],
    [`/toolsets/${TOOLSET_ID}`, 'toolset-instances'],
  ]) {
    test(`a long invalid reason wraps inside the ${table} panel at 1024`, async ({ page }) => {
      await page.setViewportSize({ width: 1024, height: 800 });
      await mockRoots(page, null);
      await page.goto(path);
      const reason = page.getByTestId('instance-status-reason').first();
      await expect(reason).toContainText('has no harness');
      const panel = await page.getByTestId(table).locator('xpath=ancestor::section').boundingBox();
      const cell = await reason.boundingBox();
      const tableBox = await page.getByTestId(table).boundingBox();
      expect(panel && cell && tableBox).toBeTruthy();
      // the reason and the table stay inside the panel's right edge
      expect(cell!.x + cell!.width).toBeLessThanOrEqual(panel!.x + panel!.width);
      expect(tableBox!.x + tableBox!.width).toBeLessThanOrEqual(panel!.x + panel!.width);
    });
  }

  test('the workflow instance name stays on one line at 1024', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 800 });
    await mockRoots(page, null);
    await page.goto(`/workflows/${WORKFLOW_ID}`);
    const link = page.getByTestId('workflow-instances').getByRole('link', { name: 'wf-settings#r1', exact: true });
    await expect(link).toBeVisible();
    // the name can never break at its hyphen, even though the other columns wrap
    const whiteSpace = await link.evaluate((node) => getComputedStyle(node.firstElementChild ?? node).whiteSpace);
    expect(whiteSpace).toBe('nowrap');
    // one line of text is far shorter than two
    const box = await link.boundingBox();
    expect(box?.height ?? 999).toBeLessThan(30);
  });
});
