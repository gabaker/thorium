import { test, Page } from '@playwright/test';

import { expect, fulfillJson, MOCK_GROUP, mockEntity, mockEntityList, mockInstanceSummary } from './agentic-mocks';
import { MOCK_USER, setupMockAuth } from './helpers';

const TOOL_ID = '00000000-0000-0000-0000-00000000ee01';

/** The mocked API's record of the requests a test made. */
interface Calls {
  /** The raw multipart body of every `PATCH /entities/{id}` sent to the tool */
  patches: string[];
  /** The raw multipart body of every `POST /entities/` */
  creates: string[];
}

/**
 * Serve one tool in the mocked user's group and record the writes made to it.
 *
 * @param page - The page whose requests are mocked.
 * @param description - The tool's starting description.
 * @returns The record of requests made.
 */
async function mockEditableTool(page: Page, description: string | null = 'first description'): Promise<Calls> {
  const calls: Calls = { patches: [], creates: [] };
  const versions = [mockInstanceSummary('inst-1.0.0', '1.0.0')];
  const groups = [MOCK_GROUP];
  let current = description;
  const tool = () => ({
    ...mockEntity(
      'Tool',
      TOOL_ID,
      'edit-tool',
      { kind: 'Image', latest: versions[0].id, latest_version: versions[0].version, instances: versions, modified: '2026-10-01T00:00:00Z' },
      groups,
    ),
    description: current,
  });
  await setupMockAuth(page);
  // the user is an admin in two groups, so the tool can be added to the second
  await page.route('**/api/users/whoami', (route) => fulfillJson(route, { ...MOCK_USER, groups: [MOCK_GROUP, 'other'] }));
  await page.route('**/api/pipelines/list/**', (route) => fulfillJson(route, { details: [] }));
  await page.route(`**/api/entities/tools/${TOOL_ID}/instances`, (route) => fulfillJson(route, versions));
  await page.route('**/api/groups/details/**', (route) =>
    fulfillJson(route, { details: [MOCK_GROUP, 'other'].map((name) => groupDetails(name, MOCK_USER.username)) }),
  );
  await page.route('**/api/groups/*/details', (route) =>
    fulfillJson(route, groupDetails(new URL(route.request().url()).pathname.split('/').slice(-2)[0], MOCK_USER.username)),
  );
  await page.route('**/api/entities/*', (route) => {
    const id = new URL(route.request().url()).pathname.split('/').pop() ?? '';
    if (id === TOOL_ID && route.request().method() === 'PATCH') {
      const body = route.request().postData() ?? '';
      calls.patches.push(body);
      // like the API, refuse to add a group the tool is already in
      const added = /name="add_groups\[\]"\r\n\r\n([^\r]+)/.exec(body)?.[1];
      if (added && groups.includes(added)) return fulfillJson(route, { error: `Entity already contains groups: ["${added}"]` }, 400);
      if (added) groups.push(added);
      if (body.includes('name="clear_description"')) current = null;
      const next = /name="description"\r\n\r\n([^\r]*)/.exec(body)?.[1];
      if (next !== undefined) current = next;
      return route.fulfill({ status: 204 });
    }
    if (id === TOOL_ID) return fulfillJson(route, tool());
    return fulfillJson(
      route,
      mockEntity('ToolInstance', id, 'edit-tool@1.0.0', {
        tool: TOOL_ID,
        version: versions[0].version,
        previous: null,
        spec_hash: versions[0].spec_hash,
        deprecated: false,
        spec: { Image: { marker: 'spec' } },
      }),
    );
  });
  return calls;
}

/**
 * Build the details of a group as the group list serves them.
 *
 * @param name - The group's name.
 * @param user - The user placed in the group's `users` role.
 * @param role - The role bucket to place the user in.
 * @returns The group.
 */
function groupDetails(name: string, user: string, role: 'users' | 'monitors' = 'users') {
  const bucket = (members: string[]) => ({ combined: members, direct: members, metagroups: [] });
  return {
    name,
    owners: bucket([]),
    managers: bucket([]),
    analysts: [],
    users: bucket(role === 'users' ? [user] : []),
    monitors: bucket(role === 'monitors' ? [user] : []),
    allowed: { entities: true },
  };
}

test.describe('tool edit root', () => {
  test('a second save does not send the group that was already added', async ({ page }) => {
    const calls = await mockEditableTool(page);
    await page.goto(`/tools/${TOOL_ID}`);
    await page.getByTestId('edit-add-group').selectOption('other');
    await page.getByTestId('edit-save').click();
    await expect(page.getByTestId('tool-notice')).toHaveText('Tool updated');
    await expect(page.getByTestId('tool-groups')).toContainText('other');
    // nothing is left to save, so the button waits for a change
    await expect(page.getByTestId('edit-add-group')).toHaveValue('');
    await expect(page.getByTestId('edit-save')).toBeDisabled();
    await page.getByTestId('edit-description').fill('second description');
    await page.getByTestId('edit-save').click();
    await expect(page.getByTestId('tool-notice')).toHaveText('Tool updated');
    expect(calls.patches).toHaveLength(2);
    expect(calls.patches[1]).not.toContain('add_groups[]');
  });

  test('a version row keeps its text cells on one line at a narrow width', async ({ page }) => {
    await mockEditableTool(page);
    await page.setViewportSize({ width: 1024, height: 800 });
    await page.goto(`/tools/${TOOL_ID}`);
    const row = page.getByTestId('version-row-1.0.0');
    await expect(row).toBeVisible();
    // the version, status, spec hash, created time and instance id each fit on one line; only the action buttons may wrap
    const cells = row.locator('td');
    for (let index = 0; index < 5; index++) {
      // the height of the cell's content: one line of text or badges is well under two lines' worth
      const height = await cells.nth(index).evaluate((el) => {
        const range = document.createRange();
        range.selectNodeContents(el);
        return range.getBoundingClientRect().height;
      });
      expect(height, `cell ${index}`).toBeLessThan(32);
    }
  });

  test('blanking the description clears it, and an unchanged form cannot be saved', async ({ page }) => {
    const calls = await mockEditableTool(page);
    await page.goto(`/tools/${TOOL_ID}`);
    await expect(page.getByTestId('edit-save')).toBeDisabled();
    await page.getByTestId('edit-description').fill('');
    await page.getByTestId('edit-save').click();
    await expect(page.getByTestId('tool-notice')).toHaveText('Tool updated');
    expect(calls.patches).toHaveLength(1);
    expect(calls.patches[0]).toContain('name="clear_description"');
    await expect(page.getByTestId('edit-save')).toBeDisabled();
  });

  test('a result shows beside the button that was used', async ({ page }) => {
    await mockEditableTool(page);
    await page.setViewportSize({ width: 1280, height: 500 });
    await page.goto(`/tools/${TOOL_ID}`);
    await page.getByTestId('edit-description').fill('another');
    await page.getByTestId('edit-save').click();
    const notice = page.getByTestId('tool-notice');
    await expect(notice).toBeVisible();
    // the notice sits below the save button, not under the page title
    const save = await page.getByTestId('edit-save').boundingBox();
    const shown = await notice.boundingBox();
    expect(shown?.y ?? 0).toBeGreaterThan(save?.y ?? Infinity);
    await expect(notice).toBeInViewport();
  });

  test('a user who only monitors a group of the tool cannot change it', async ({ page }) => {
    await mockEditableTool(page);
    await page.route('**/api/users/whoami', (route) => fulfillJson(route, { ...MOCK_USER, role: 'User', groups: [MOCK_GROUP, 'other'] }));
    await page.route('**/api/groups/details/**', (route) =>
      fulfillJson(route, {
        details: [groupDetails(MOCK_GROUP, MOCK_USER.username, 'monitors'), groupDetails('other', MOCK_USER.username)],
      }),
    );
    await page.route('**/api/groups/*/details', (route) => {
      const name = new URL(route.request().url()).pathname.split('/').slice(-2)[0];
      return fulfillJson(route, groupDetails(name, MOCK_USER.username, name === MOCK_GROUP ? 'monitors' : 'users'));
    });
    await page.goto(`/tools/${TOOL_ID}`);
    await expect(page.getByTestId('tool-read-only')).toBeVisible();
    // the user submitted the tool, and the API lets a submitter delete what they submitted
    await expect(page.getByTestId('delete-tool')).toBeEnabled();
    await expect(page.getByTestId('deprecate-1.0.0')).toBeDisabled();
    await expect(page.getByTestId('publish-submit')).toBeDisabled();
    await expect(page.getByTestId('edit-save')).toBeDisabled();
  });

  test('a group the user only monitors is not offered to add the tool to', async ({ page }) => {
    await mockEditableTool(page);
    await page.route('**/api/users/whoami', (route) =>
      fulfillJson(route, { ...MOCK_USER, role: 'User', groups: [MOCK_GROUP, 'other', 'third'] }),
    );
    await page.route('**/api/groups/details/**', (route) =>
      fulfillJson(route, {
        details: [
          groupDetails(MOCK_GROUP, MOCK_USER.username),
          groupDetails('other', MOCK_USER.username, 'monitors'),
          groupDetails('third', MOCK_USER.username),
        ],
      }),
    );
    await page.route('**/api/groups/*/details', (route) => {
      const name = new URL(route.request().url()).pathname.split('/').slice(-2)[0];
      return fulfillJson(route, groupDetails(name, MOCK_USER.username, name === 'other' ? 'monitors' : 'users'));
    });
    await page.goto(`/tools/${TOOL_ID}`);
    const options = page.getByTestId('edit-add-group').locator('option');
    await expect(options).toHaveText(['Choose a group', 'third']);
  });
});

test.describe('tool create', () => {
  test('a fresh form shows the missing name and warns about a duplicate name', async ({ page }) => {
    await setupMockAuth(page);
    const existing = mockEntity('Tool', TOOL_ID, 'taken-name', { kind: 'Image', latest: null, instances: [] });
    await mockEntityList(page, [existing]);
    await page.goto('/tools/create');
    await expect(page.getByTestId('tool-problem')).toHaveText('Enter a name.');
    await page.getByTestId('tool-name').fill('taken-name');
    await page.getByTestId('groups-' + MOCK_GROUP).check();
    await expect(page.getByTestId('tool-duplicate')).toContainText(`already exists in ${MOCK_GROUP}`);
    await page.getByTestId('tool-name').fill('fresh-name');
    await expect(page.getByTestId('tool-duplicate')).toHaveCount(0);
  });

  test('the create button stays disabled after a create that returned portability notes', async ({ page }) => {
    await setupMockAuth(page);
    await mockEntityList(page, []);
    let creates = 0;
    await page.route('**/api/entities/', (route) => {
      if (route.request().method() !== 'POST') return route.fallback();
      creates += 1;
      return fulfillJson(route, {
        id: TOOL_ID,
        name: 'x',
        portability: { strict_compatible: false, gemini_compatible: true, strict_issues: ['note'], gemini_rewrites: [] },
      });
    });
    await page.goto('/tools/create');
    await page.getByTestId('tool-name').fill('portable');
    await page.getByTestId('groups-' + MOCK_GROUP).check();
    await page.getByTestId('tool-submit').click();
    await expect(page.getByTestId('tool-portability')).toBeVisible();
    await expect(page.getByTestId('tool-submit')).toBeDisabled();
    expect(creates).toBe(1);
  });
});
