import { test, Page } from '@playwright/test';

import { expect, fulfillJson, mockEntity, mockEntityList } from './agentic-mocks';
import { setupMockAuth } from './helpers';

const WORKFLOW_ID = '00000000-0000-0000-0000-00000000ff01';
const TOOLSET_ID = '00000000-0000-0000-0000-00000000ff02';
const AGENTS = ['agent-a', 'agent-b'].map((name, index) => mockEntity('Agent', `00000000-0000-0000-0000-00000000ff1${index}`, name, {}));
const TOOLS = ['tool-a', 'tool-b'].map((name, index) =>
  mockEntity('Tool', `00000000-0000-0000-0000-00000000ff2${index}`, name, { kind: 'Agentic', latest: null, modified: '' }),
);
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
 * Serve one workflow, one toolset, and their candidate members, and answer a member update with the given status.
 *
 * @param page - The page whose requests are mocked.
 * @param status - The status the entity update answers with.
 */
async function mockMembers(page: Page, status: number) {
  await setupMockAuth(page);
  await mockEntityList(page, [...AGENTS, ...TOOLS]);
  await page.route('**/api/entities/*', (route) => {
    if (route.request().method() === 'PATCH') {
      return status === 204 ? route.fulfill({ status }) : fulfillJson(route, { error: 'not allowed to edit this entity' }, status);
    }
    const id = new URL(route.request().url()).pathname.split('/').pop();
    return fulfillJson(route, id === WORKFLOW_ID ? WORKFLOW : TOOLSET);
  });
}

test.describe('member selections on a failed apply', () => {
  test('the workflow keeps its agent selections when the change is refused', async ({ page }) => {
    await mockMembers(page, 403);
    await page.goto(`/workflows/${WORKFLOW_ID}`);
    await page.getByTestId('add-agents-agent-a').check();
    await page.getByTestId('add-agents-agent-b').check();
    await page.getByTestId('apply-agents').click();
    await expect(page.getByText('not allowed to edit this entity')).toBeVisible();
    await expect(page.getByTestId('add-agents-agent-a')).toBeChecked();
    await expect(page.getByTestId('add-agents-agent-b')).toBeChecked();
    await expect(page.getByTestId('apply-agents')).toBeEnabled();
  });

  test('the workflow clears its agent selections once the change is applied', async ({ page }) => {
    await mockMembers(page, 204);
    await page.goto(`/workflows/${WORKFLOW_ID}`);
    await page.getByTestId('add-agents-agent-a').check();
    await page.getByTestId('apply-agents').click();
    await expect(page.getByText('Agents changed')).toBeVisible();
    await expect(page.getByTestId('add-agents-agent-a')).not.toBeChecked();
  });

  test('the toolset keeps its tool selections when the change is refused', async ({ page }) => {
    await mockMembers(page, 403);
    await page.goto(`/toolsets/${TOOLSET_ID}`);
    await page.getByTestId('add-tools-tool-a').check();
    await page.getByTestId('apply-tools').click();
    await expect(page.getByText('not allowed to edit this entity')).toBeVisible();
    await expect(page.getByTestId('add-tools-tool-a')).toBeChecked();
    await expect(page.getByTestId('apply-tools')).toBeEnabled();
  });
});
