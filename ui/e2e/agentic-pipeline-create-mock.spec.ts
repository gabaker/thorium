import { test } from '@playwright/test';

import { expect, fulfillJson, mockEntity, mockEntityList } from './agentic-mocks';
import { MOCK_USER, setupMockAuth } from './helpers';

// the user is in two groups and each group has its own Image tool
const GROUP_A = 'group-a';
const GROUP_B = 'group-b';
const TOOL_A = mockEntity('Tool', '00000000-0000-0000-0000-00000000aa01', 'atool', { kind: 'Image', latest: null, modified: '' }, [
  GROUP_A,
]);
const TOOL_B = mockEntity('Tool', '00000000-0000-0000-0000-00000000bb01', 'btool', { kind: 'Image', latest: null, modified: '' }, [
  GROUP_B,
]);

// a tool that belongs to both groups
const TOOL_AB = mockEntity('Tool', '00000000-0000-0000-0000-00000000ab01', 'shared', { kind: 'Image', latest: null, modified: '' }, [
  GROUP_A,
  GROUP_B,
]);

test.describe('pipeline create form', () => {
  test.beforeEach(async ({ page }) => {
    await setupMockAuth(page);
    await page.route('**/api/users/whoami', (route) => fulfillJson(route, { ...MOCK_USER, groups: [GROUP_A, GROUP_B] }));
    await mockEntityList(page, [TOOL_A, TOOL_B]);
  });

  test("offers only the chosen group's tools", async ({ page }) => {
    await page.goto('/create/pipeline');
    await page.getByTestId('pipeline-group').selectOption(GROUP_A);
    const options = page.getByTestId('stage-tool-0-0').locator('option');
    await expect(options).toHaveText(['Choose a tool', 'atool']);
  });

  test('a group change clears the chosen stage tools', async ({ page }) => {
    await page.goto('/create/pipeline');
    await page.getByTestId('pipeline-name').fill('cross-group');
    await page.getByTestId('pipeline-group').selectOption(GROUP_B);
    await page.getByTestId('stage-name-0-0').fill('s1');
    await page.getByTestId('stage-tool-0-0').selectOption(TOOL_B.id);
    await expect(page.getByTestId('create-submit')).toBeEnabled();
    await page.getByTestId('pipeline-group').selectOption(GROUP_A);
    await expect(page.getByTestId('stage-name-0-0')).toHaveValue('s1');
    await expect(page.getByTestId('stage-tool-0-0')).toHaveValue('');
    await expect(page.getByTestId('create-submit')).toBeDisabled();
  });

  test('a group change keeps the stage tools that belong to the new group', async ({ page }) => {
    await mockEntityList(page, [TOOL_A, TOOL_B, TOOL_AB]);
    await page.goto('/create/pipeline');
    await page.getByTestId('pipeline-name').fill('keeps-shared');
    await page.getByTestId('stage-name-0-0').fill('s1');
    await page.getByTestId('stage-tool-0-0').selectOption(TOOL_AB.id);
    await page.getByTestId('pipeline-group').selectOption(GROUP_A);
    await expect(page.getByTestId('stage-tool-0-0')).toHaveValue(TOOL_AB.id);
    await expect(page.getByTestId('create-submit')).toBeEnabled();
  });
});

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

test.describe('pipeline create form roles', () => {
  test('offers only the groups the user may create in', async ({ page }) => {
    await setupMockAuth(page);
    await page.route('**/api/users/whoami', (route) =>
      fulfillJson(route, { ...MOCK_USER, username: 'io', role: 'Analyst', groups: [GROUP_A, GROUP_B] }),
    );
    await page.route('**/api/groups/*/details', (route) => {
      const name = new URL(route.request().url()).pathname.split('/').slice(-2)[0];
      return fulfillJson(route, name === GROUP_B ? mockGroup(name, { monitors: ['io'] }) : mockGroup(name, { users: ['io'] }));
    });
    await mockEntityList(page, [TOOL_A, TOOL_B]);
    await page.goto('/create/pipeline');
    await expect(page.getByTestId('pipeline-group').locator('option')).toHaveText(['Choose a group', GROUP_A]);
    await expect(page.getByTestId('create-blocked')).toHaveCount(0);
  });

  test('a global User is told the role is missing and cannot submit', async ({ page }) => {
    await setupMockAuth(page);
    await page.route('**/api/users/whoami', (route) =>
      fulfillJson(route, { ...MOCK_USER, username: 'ganymede', role: 'User', groups: [GROUP_A] }),
    );
    await page.route('**/api/groups/*/details', (route) => {
      const name = new URL(route.request().url()).pathname.split('/').slice(-2)[0];
      return fulfillJson(route, mockGroup(name, { users: ['ganymede'] }));
    });
    await mockEntityList(page, [TOOL_A]);
    await page.goto('/create/pipeline');
    await expect(page.getByTestId('create-blocked')).toContainText("You can't create a pipeline");
    await expect(page.getByTestId('create-blocked')).toContainText('Analyst, Admin, or Developer');
    await expect(page.getByTestId('create-submit')).toBeDisabled();
  });

  test('an analyst who can create in no group is told why and cannot submit', async ({ page }) => {
    await setupMockAuth(page);
    await page.route('**/api/users/whoami', (route) =>
      fulfillJson(route, { ...MOCK_USER, username: 'io', role: 'Analyst', groups: [GROUP_A] }),
    );
    await page.route('**/api/groups/*/details', (route) => {
      const name = new URL(route.request().url()).pathname.split('/').slice(-2)[0];
      return fulfillJson(route, mockGroup(name, { monitors: ['io'] }));
    });
    await mockEntityList(page, [TOOL_A]);
    await page.goto('/create/pipeline');
    await expect(page.getByTestId('create-blocked')).toContainText('user, manager, or owner role');
    await expect(page.getByTestId('create-submit')).toBeDisabled();
  });
});
