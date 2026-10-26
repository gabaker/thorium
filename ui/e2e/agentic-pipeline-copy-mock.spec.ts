import { test, Page } from '@playwright/test';

import { expect, fulfillJson, MOCK_GROUP, mockEntity } from './agentic-mocks';
import { MOCK_USER, setupMockAuth } from './helpers';

const TOOL = mockEntity('Tool', '00000000-0000-0000-0000-00000000cc01', 'scanner', { kind: 'Image', latest: null, modified: '' });
/** The name the pipeline list gives the copy of SOURCE */
const COPY_NAME = 'auto-scan-copy';
const SOURCE = {
  group: MOCK_GROUP,
  name: 'auto-scan',
  creator: 'test',
  order: ['scan'],
  sla: 86400,
  stage_tools: { scan: TOOL.id },
  triggers: { 'on-new-sample': 'NewSample' },
};

/**
 * Open the create page with the source pipeline in the router state, as the list's Copy action does
 * (the state carries the copy's name, already suffixed).
 *
 * @param page - A signed in page.
 */
async function openCopy(page: Page) {
  await page.goto('/');
  await expect(page.getByText(`@${MOCK_USER.username}`)).toBeVisible();
  await page.evaluate(
    (source) => {
      window.history.pushState({ usr: source, key: 'copy', idx: 1 }, '', '/create/pipeline');
      window.dispatchEvent(new PopStateEvent('popstate', { state: window.history.state }));
    },
    { ...SOURCE, name: COPY_NAME },
  );
}

test.describe('pipeline copy', () => {
  test.beforeEach(async ({ page }) => {
    await setupMockAuth(page);
  });

  test("a copy keeps the source pipeline's triggers", async ({ page }) => {
    await page.route('**/api/entities/details/**', (route) => fulfillJson(route, { data: [TOOL] }));
    let created: Record<string, unknown> | null = null;
    await page.route('**/api/pipelines/', (route) => {
      created = route.request().postDataJSON() as Record<string, unknown>;
      return route.fulfill({ status: 204 });
    });
    await openCopy(page);
    await expect(page.getByTestId('stage-tool-0-0')).toHaveValue(TOOL.id);
    // the name is the copy name the list chose, with no second suffix
    await expect(page.getByTestId('pipeline-name')).toHaveValue(COPY_NAME);
    // each stage control is announced by name, with its step and stage positions
    const step = 1;
    const stage = 1;
    await expect(page.getByRole('textbox', { name: `Stage name, step ${step} stage ${stage}` })).toBeVisible();
    await expect(page.getByRole('combobox', { name: `Tool, step ${step} stage ${stage}` })).toHaveValue(TOOL.id);
    await expect(page.getByRole('combobox', { name: `Tool version, step ${step} stage ${stage}` })).toBeVisible();
    await expect(page.getByRole('button', { name: `Remove stage ${stage} of step ${step}` })).toBeVisible();
    await expect(page.getByTestId('copied-triggers')).toContainText('on-new-sample');
    await page.getByTestId('create-submit').click();
    await expect.poll(() => created).not.toBeNull();
    expect(created!['triggers']).toEqual(SOURCE.triggers);
  });

  test('a stage edit made while the tools load survives the copy seeding', async ({ page }) => {
    // hold the tool list until the user has edited a stage
    let release: () => void = () => undefined;
    const held = new Promise<void>((resolve) => (release = resolve));
    await page.route('**/api/entities/details/**', async (route) => {
      await held;
      return fulfillJson(route, { data: [TOOL] });
    });
    await openCopy(page);
    await expect(page.getByTestId('stage-name-0-0')).toBeVisible();
    await page.waitForTimeout(300);
    await page.getByTestId('stage-name-0-0').fill('mine');
    release();
    await page.waitForTimeout(1000);
    await expect(page.getByTestId('stage-name-0-0')).toHaveValue('mine');
  });

  test('a failed tool list is shown', async ({ page }) => {
    await page.route('**/api/entities/details/**', (route) => fulfillJson(route, { error: 'tool list unavailable' }, 500));
    await openCopy(page);
    await expect(page.getByTestId('tools-error')).toBeVisible();
    // the copy still starts from the source pipeline's stages
    await expect(page.getByTestId('stage-name-0-0')).toHaveValue('scan');
  });

  test("a copy from a group the user can't create in leaves Create disabled until a listed group is chosen", async ({ page }) => {
    const other = 'listed-group';
    // the tool is in both groups, so the stage stays bound when the group changes
    const sharedTool = { ...TOOL, groups: [MOCK_GROUP, other] };
    // the user only monitors the source's group and may create in the other one
    const members = (combined: string[]) => ({ combined, direct: combined, metagroups: [] });
    const details = (name: string) => ({
      name,
      owners: members([]),
      managers: members([]),
      analysts: [],
      users: members(name === other ? [MOCK_USER.username] : []),
      monitors: members(name === other ? [] : [MOCK_USER.username]),
      allowed: {},
    });
    await page.route('**/api/users/whoami', (route) => fulfillJson(route, { ...MOCK_USER, role: 'Analyst', groups: [MOCK_GROUP, other] }));
    await page.route('**/api/groups/*/details', (route) => {
      const name = new URL(route.request().url()).pathname.split('/').slice(-2)[0];
      return fulfillJson(route, details(name));
    });
    await page.route('**/api/entities/details/**', (route) => fulfillJson(route, { data: [sharedTool] }));
    let created: Record<string, unknown> | null = null;
    await page.route('**/api/pipelines/', (route) => {
      created = route.request().postDataJSON() as Record<string, unknown>;
      return route.fulfill({ status: 204 });
    });
    await openCopy(page);
    // the hidden source group is not selected, and Create waits for a listed group
    await expect(page.getByTestId('pipeline-group')).toHaveValue('');
    await expect(page.getByTestId('pipeline-problem')).toHaveText('Choose a group.');
    await expect(page.getByTestId('create-submit')).toBeDisabled();
    await page.getByTestId('pipeline-group').selectOption(other);
    await expect(page.getByTestId('create-submit')).toBeEnabled();
    await page.getByTestId('create-submit').click();
    await expect.poll(() => created).not.toBeNull();
    expect(created!['group']).toBe(other);
  });
});
