import { test, Page } from '@playwright/test';

import { expect, fulfillJson, mockEntity, mockEntityList, mockInstanceSummary } from './agentic-mocks';
import { MOCK_USER, setupMockAuth } from './helpers';

const TOOL_ID = '00000000-0000-0000-0000-00000000ee01';
const INSTANCE_ID = 'inst-1.0.0';

/** Build the details of a group with the given roles. */
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
 * Sign in as a plain user who can edit in `minerva` and `vesta` but only monitors `juno`, and serve a tool
 * submitted by `europa` (in `minerva` and `juno` unless other groups are given).
 *
 * @param page - The page whose requests are mocked.
 * @param description - The tool's description.
 * @param groups - The tool's groups.
 */
async function mockSharedTool(page: Page, description: string | null = null, groups = ['minerva', 'juno']) {
  await setupMockAuth(page);
  await page.route('**/api/users/whoami', (route) =>
    fulfillJson(route, { ...MOCK_USER, username: 'ganymede', role: 'User', groups: ['minerva', 'juno', 'vesta'] }),
  );
  await page.route('**/api/groups/*/details', (route) => {
    const name = new URL(route.request().url()).pathname.split('/').slice(-2)[0];
    return fulfillJson(route, name === 'juno' ? mockGroup('juno', { monitors: ['ganymede'] }) : mockGroup(name, { users: ['ganymede'] }));
  });
  await page.route('**/api/pipelines/list/**', (route) => fulfillJson(route, { details: [] }));
  const version = mockInstanceSummary(INSTANCE_ID, '1.0.0');
  const tool = {
    ...mockEntity(
      'Tool',
      TOOL_ID,
      'shared-tool',
      {
        kind: 'Image',
        latest: INSTANCE_ID,
        latest_version: version.version,
        latest_spec_hash: version.spec_hash,
        modified: '2026-10-01T00:00:00Z',
        instances: [version],
      },
      groups,
    ),
    description,
    submitter: 'europa',
  };
  await mockEntityList(page, [tool]);
  await page.route(`**/api/entities/tools/${TOOL_ID}/instances`, (route) => fulfillJson(route, [version]));
  await page.route('**/api/entities/*', (route) => {
    const id = new URL(route.request().url()).pathname.split('/').pop() ?? '';
    if (id === TOOL_ID) return fulfillJson(route, tool);
    if (id === INSTANCE_ID) {
      return fulfillJson(route, {
        ...mockEntity('ToolInstance', INSTANCE_ID, 'shared-tool@1.0.0', {
          tool: TOOL_ID,
          version: version.version,
          previous: null,
          spec_hash: version.spec_hash,
          deprecated: false,
          spec: { Image: {} },
        }),
        submitter: 'europa',
        groups,
        tags: { ToolKind: { Image: ['minerva'] }, ToolVersion: { '1.0.0': ['minerva'] } },
      });
    }
    return fulfillJson(route, { error: `entity ${id} not found` }, 404);
  });
}

test.describe('tool actions follow the user roles', () => {
  test('a monitor of one group sees every mutating control disabled with the group named', async ({ page }) => {
    await mockSharedTool(page);
    await page.goto(`/tools/${TOOL_ID}`);
    await expect(page.getByTestId('spec-view')).toBeVisible();
    for (const id of ['deprecate-1.0.0', 'publish-submit', 'edit-save', 'delete-tool', 'delete-version-1.0.0']) {
      await expect(page.getByTestId(id)).toBeDisabled();
    }
    await expect(page.getByTestId('edit-save')).toHaveAttribute('title', /juno/);
    await expect(page.getByTestId('edit-blocked')).toContainText('juno');
    await expect(page.getByTestId('edit-blocked')).not.toContainText('`');
    await expect(page.getByTestId('edit-description')).toBeDisabled();
    await expect(page.getByTestId('edit-add-group')).toBeDisabled();
    await expect(page.getByTestId('publish-version')).toBeDisabled();
    await expect(page.getByTestId('publish-spec')).toBeDisabled();
    await expect(page.getByTestId('edit-unchanged')).toHaveCount(0);
    await expect(page.getByTestId('delete-tool')).toHaveAttribute('title', /submitter or a manager/);
    // the tags show in a fixed order, though the API returns ToolKind before ToolVersion
    await expect(page.getByTestId('version-tags')).toContainText(/ToolVersion=1\.0\.0\s*ToolKind=Image/);
  });

  test("a member can edit but not delete another user's version, and can only add groups where they edit", async ({ page }) => {
    await mockSharedTool(page, null, ['minerva']);
    await page.goto(`/tools/${TOOL_ID}`);
    await expect(page.getByTestId('spec-view')).toBeVisible();
    await expect(page.getByTestId('tool-read-only')).toHaveCount(0);
    await expect(page.getByTestId('deprecate-1.0.0')).toBeEnabled();
    await expect(page.getByTestId('delete-version-1.0.0')).toBeDisabled();
    await expect(page.getByTestId('delete-version-1.0.0')).toHaveAttribute('title', /submitter or a manager or owner of minerva/);
    // the user only monitors juno, so the tool can't gain it
    const select = page.getByTestId('edit-add-group');
    await expect(select.locator('option[value="vesta"]')).toHaveCount(1);
    await expect(select.locator('option[value="juno"]')).toHaveCount(0);
    await select.selectOption('vesta');
    await expect(page.getByTestId('edit-add-group-note')).toContainText(
      'Every version of this tool gains vesta, so members of vesta can see and run it.',
    );
    await expect(page.getByTestId('edit-add-group-note')).not.toContainText('`');
    await expect(page.getByTestId('edit-add-group-note').locator('code').first()).toHaveText('vesta');
  });

  test('the list keeps Create enabled for a user who can edit in a group and shows a one-line description', async ({ page }) => {
    await mockSharedTool(page, '---\n\n`shared-tool` extracts things. See [docs](https://example.test).\n\n---\n##### Usage\n\nRun it.');
    await page.goto('/tools');
    await expect(page.getByTestId('create-tool')).toBeEnabled();
    await expect(page.getByTestId('create-tool')).toHaveText('+ New tool');
    const row = page.getByTestId('tool-row-shared-tool');
    await expect(row).toContainText('shared-tool extracts things. See docs.');
    await expect(row).not.toContainText('##### Usage');
  });

  test('a missing tool shows a title and a way back to the list', async ({ page }) => {
    await mockSharedTool(page);
    await page.goto('/tools/00000000-0000-0000-0000-00000000ee99');
    await expect(page.getByTestId('entity-not-found-title')).toHaveText(/Tool not found/i);
    await page.getByRole('link', { name: 'Back to tools' }).click();
    await expect(page).toHaveURL(/\/tools$/);
  });
});
