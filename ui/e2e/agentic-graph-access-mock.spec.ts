import { test, Page } from '@playwright/test';

import { expect, fulfillJson, mockEntity, mockEntityList } from './agentic-mocks';
import { MOCK_USER, setupMockAuth } from './helpers';

const WORKFLOW_ID = '00000000-0000-0000-0000-00000000ef01';
const AGENT_ID = '00000000-0000-0000-0000-00000000ef02';
const TOOLSET_ID = '00000000-0000-0000-0000-00000000ef03';

/** Build the details of a group where the given users hold the user or monitor role. */
function mockGroup(name: string, roles: { users?: string[]; monitors?: string[] }) {
  const users = (combined: string[] = []) => ({ combined, direct: combined, metagroups: [] });
  return {
    name,
    owners: users(),
    managers: users(),
    analysts: [],
    users: users(roles.users),
    monitors: users(roles.monitors),
    allowed: {
      files: true,
      repos: true,
      tags: true,
      images: true,
      pipelines: true,
      reactions: true,
      results: true,
      comments: true,
      entities: true,
    },
  };
}

/**
 * Sign in as a plain user who can edit in `minerva` but only monitors `juno`, and serve a workflow, an agent,
 * and a toolset in `minerva` that another user submitted.
 *
 * @param page - The page whose requests are mocked.
 */
async function mockSharedGraph(page: Page) {
  await setupMockAuth(page);
  await page.route('**/api/users/whoami', (route) =>
    fulfillJson(route, { ...MOCK_USER, username: 'ganymede', role: 'User', groups: ['minerva', 'juno'] }),
  );
  await page.route('**/api/groups/*/details', (route) => {
    const name = new URL(route.request().url()).pathname.split('/').slice(-2)[0];
    return fulfillJson(route, name === 'juno' ? mockGroup('juno', { monitors: ['ganymede'] }) : mockGroup(name, { users: ['ganymede'] }));
  });
  const shared = (entity: { kind: string }) => ({ ...entity, submitter: 'europa' });
  const workflow = shared(
    mockEntity(
      'WorkflowCatalog',
      WORKFLOW_ID,
      'shared-wf',
      { entry_agent: null, auto_instance: false, limits: {}, default_models: [], model_auth: { levels: [] }, agents: [] },
      ['minerva'],
    ),
  );
  const agent = shared(mockEntity('Agent', AGENT_ID, 'shared-agent', { config: { models: [] } }, ['minerva']));
  const toolset = shared(
    mockEntity(
      'ToolSet',
      TOOLSET_ID,
      'shared-set',
      { tools: [], selector: null, read_only: false, auto_instance: false, highest_revision: 0 },
      ['minerva'],
    ),
  );
  await mockEntityList(page, [workflow, agent, toolset]);
  await page.route('**/api/entities/*', (route) => {
    const id = new URL(route.request().url()).pathname.split('/').pop() ?? '';
    const found = [workflow, agent, toolset].find((entity) => (entity as { id: string }).id === id);
    return found ? fulfillJson(route, found) : fulfillJson(route, { error: `entity ${id} not found` }, 404);
  });
}

test.describe('workflow, agent, and toolset actions follow the user roles', () => {
  for (const [path, testId, title] of [
    [`/workflows/${WORKFLOW_ID}`, 'delete-workflow', 'workflow-title'],
    [`/agents/${AGENT_ID}`, 'delete-agent', 'agent-title'],
    [`/toolsets/${TOOLSET_ID}`, 'delete-toolset', 'toolset-title'],
  ]) {
    test(`${testId} is disabled with its reason for a member who did not submit it`, async ({ page }) => {
      await mockSharedGraph(page);
      await page.goto(path);
      await expect(page.getByTestId(title)).toBeVisible();
      await expect(page.getByTestId(testId)).toBeDisabled();
      await expect(page.getByTestId(testId)).toHaveAttribute('title', /submitter or a manager or owner of minerva/);
    });
  }

  for (const [path, nameId] of [
    ['/toolsets/create', 'toolset-name'],
    ['/agents/create', 'agent-name'],
    ['/workflows/create', 'workflow-name'],
  ]) {
    test(`${path} offers only the groups the user can create in`, async ({ page }) => {
      await mockSharedGraph(page);
      await page.goto(path);
      await expect(page.getByTestId(nameId)).toBeVisible();
      await expect(page.getByTestId('groups-minerva')).toBeVisible();
      await expect(page.getByTestId('groups-juno')).toHaveCount(0);
    });
  }

  test('a toolset without explicit tools says so under the table', async ({ page }) => {
    await mockSharedGraph(page);
    await page.goto(`/toolsets/${TOOLSET_ID}`);
    await expect(page.getByTestId('no-tools')).toHaveText('No explicit tools.');
  });
});
