import { test } from '@playwright/test';
import type { Page } from '@playwright/test';

import { expect, fulfillJson, mockEntity } from './agentic-mocks';
import { setupMockAuth } from './helpers';

// Mocked tests: the toolset, agent, workflow and workflow instance pages report a failed read as a failure with a retry,
// start clean when moving between ids, and show their list, instance and action errors without literal backticks.

/** An id nothing is served for. */
const MISSING_ID = '00000000-0000-0000-0000-000000000000';
/** The id of the toolset the pages are tested with. */
const TOOLSET_ID = '00000000-0000-0000-0000-00000000a301';
/** The id of the agent the pages are tested with. */
const AGENT_ID = '00000000-0000-0000-0000-00000000a302';
/** The id of the workflow the pages are tested with. */
const WORKFLOW_ID = '00000000-0000-0000-0000-00000000a303';
/** The id of the workflow instance the pages are tested with. */
const INSTANCE_ID = '00000000-0000-0000-0000-00000000a304';

/** The entities served by id. */
const ENTITIES = [
  mockEntity('ToolSet', TOOLSET_ID, 'uiloop-set', {
    read_only: false,
    selector: null,
    tools: [],
    auto_instance: false,
    highest_revision: 0,
    latest_instance: null,
  }),
  mockEntity('Agent', AGENT_ID, 'uiloop-agent', {
    config: { models: [] },
    harness: null,
    latest_tools: [],
    pinned_tools: [],
    tool_sets: [],
    sub_agents: [],
  }),
  mockEntity('WorkflowCatalog', WORKFLOW_ID, 'uiloop-flow', {
    entry_agent: null,
    auto_instance: false,
    limits: {},
    default_models: [],
    model_auth: { levels: [] },
    agents: [],
    modified: '2026-10-01T00:00:00Z',
  }),
  mockEntity('WorkflowInstance', INSTANCE_ID, 'uiloop-flow#r1', {
    workflow: WORKFLOW_ID,
    workflow_groups: [],
    revision: 1,
    previous: null,
    status: 'Ready',
    warnings: [],
    trigger: 'Manual',
    triggered_by: 'tester',
    entry_agent: null,
    limits: { max_alive_sub_agents: 2 },
    default_models: [],
    agents: [],
    tools: [],
    digest: 'v1:abc',
  }),
];

/** The detail pages: each one's route, the id served, the kind label it shows when missing, and the test id of its title. */
const PAGES = [
  { route: '/toolsets', id: TOOLSET_ID, what: 'Toolset', title: 'toolset-title' },
  { route: '/agents', id: AGENT_ID, what: 'Agent', title: 'agent-title' },
  { route: '/workflows', id: WORKFLOW_ID, what: 'Workflow', title: 'workflow-title' },
  { route: '/workflows/instances', id: INSTANCE_ID, what: 'Workflow instance', title: 'instance-title' },
];

/**
 * Serve the entities by id, answering every entity read with a server error while the returned switch is on.
 *
 * @param page - The page whose requests are mocked.
 * @returns A function that sets whether reads fail.
 */
async function mockEntities(page: Page): Promise<(failing: boolean) => void> {
  let failing = false;
  await setupMockAuth(page);
  await page.route('**/api/entities/details/**', (route) => fulfillJson(route, { data: [] }));
  await page.route('**/api/entities/*', (route) => {
    const id = new URL(route.request().url()).pathname.split('/').pop() ?? '';
    if (failing) return fulfillJson(route, { error: 'injected server failure' }, 500);
    const found = ENTITIES.find((entity) => entity.id === id);
    return found ? fulfillJson(route, found) : fulfillJson(route, { error: `Entity ${id} not found` }, 404);
  });
  return (value) => {
    failing = value;
  };
}

test.describe('graph pages and a failed read', () => {
  for (const { route, id, what, title } of PAGES) {
    test(`${route}/<id> says the ${what.toLowerCase()} could not be loaded and a retry shows it`, async ({ page }) => {
      const setFailing = await mockEntities(page);
      setFailing(true);
      await page.goto(`${route}/${id}`);
      await expect(page.getByTestId('entity-not-found-title')).toHaveText(`${what} could not be loaded`);
      await expect(page.getByTestId('entity-not-found')).not.toContainText('does not exist');
      await expect(page.getByTestId('entity-not-found')).toContainText('injected server failure');
      setFailing(false);
      await page.getByTestId('entity-retry').click();
      await expect(page.getByTestId(title)).toBeVisible();
    });

    test(`${route}/<id> reports a missing id as not found with no retry`, async ({ page }) => {
      await mockEntities(page);
      await page.goto(`${route}/${MISSING_ID}`);
      await expect(page.getByTestId('entity-not-found-title')).toHaveText(`${what} not found`);
      await expect(page.getByTestId('entity-retry')).toHaveCount(0);
    });

    test(`${route}/<id> drops the error of a missing id when the app moves back to a real one`, async ({ page }) => {
      await mockEntities(page);
      await page.goto(`${route}/${id}`);
      await expect(page.getByTestId(title)).toBeVisible();
      // an in-app move, as back and forward make
      const move = (to: string) =>
        page.evaluate((path) => {
          window.history.pushState({}, '', path);
          window.dispatchEvent(new PopStateEvent('popstate'));
        }, to);
      await move(`${route}/${MISSING_ID}`);
      await expect(page.getByTestId('entity-not-found-title')).toHaveText(`${what} not found`);
      await move(`${route}/${id}`);
      await expect(page.getByTestId(title)).toBeVisible();
      await expect(page.getByText('not found')).toHaveCount(0);
    });
  }
});

test.describe('graph lists and a failed read', () => {
  for (const { route, empty } of [
    { route: '/toolsets', empty: 'toolset-empty' },
    { route: '/agents', empty: 'agent-empty' },
    { route: '/workflows', empty: 'workflow-empty' },
  ]) {
    test(`${route} shows the list error without an empty-state message`, async ({ page }) => {
      await setupMockAuth(page);
      await page.route('**/api/entities/details/**', (route) => fulfillJson(route, { error: 'injected list failure' }, 500));
      await page.goto(route);
      await expect(page.getByText('injected list failure').first()).toBeVisible();
      await expect(page.getByTestId(empty)).toHaveCount(0);
    });
  }
});

test.describe('graph page actions', () => {
  test('a toolset with no instances says so', async ({ page }) => {
    await mockEntities(page);
    await page.goto(`/toolsets/${TOOLSET_ID}`);
    await expect(page.getByTestId('no-instances')).toHaveText('No instances yet.');
  });

  for (const { route, id, kind } of [
    { route: '/toolsets', id: TOOLSET_ID, kind: 'toolsets' },
    { route: '/workflows', id: WORKFLOW_ID, kind: 'workflows' },
  ]) {
    test(`${route}/<id> shows a refused instance request with its backticked names as code`, async ({ page }) => {
      await mockEntities(page);
      await page.route(`**/api/entities/${kind}/${id}/instance`, (r) =>
        fulfillJson(r, { error: 'You cannot request `instances` here' }, 403),
      );
      await page.goto(`${route}/${id}`);
      await page.getByTestId('request-instance').click();
      const banner = page.getByText('You cannot request').first();
      await expect(banner).toBeVisible();
      await expect(banner).not.toContainText('`');
      await expect(banner.locator('span, code').filter({ hasText: /^instances$/ })).toHaveCount(1);
    });
  }

  test('a Ready workflow instance shows its limits and a Run action that opens the launch page', async ({ page }) => {
    await mockEntities(page);
    await page.goto(`/workflows/instances/${INSTANCE_ID}`);
    await expect(page.getByTestId('instance-max-alive')).toHaveText('2');
    await page.getByTestId('run-workflow').click();
    await expect(page).toHaveURL(new RegExp(`/launch\\?workflow=${WORKFLOW_ID}`));
  });
});
