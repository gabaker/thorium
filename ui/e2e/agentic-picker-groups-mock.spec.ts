import { test } from '@playwright/test';

import { expect, fulfillJson, mockEntity, mockEntityList } from './agentic-mocks';
import { MOCK_USER, setupMockAuth } from './helpers';

const OTHER_GROUP = 'other';
const NARROW = mockEntity('Agent', '00000000-0000-0000-0000-00000000aa01', 'narrow-agent', {});
const WIDE = mockEntity('Agent', '00000000-0000-0000-0000-00000000aa02', 'wide-agent', {}, [...MOCK_USER.groups, OTHER_GROUP]);
const TWIN_A = mockEntity('Agent', '00000000-0000-0000-0000-00000000aa03', 'twin', {});
const TWIN_B = mockEntity('Agent', '00000000-0000-0000-0000-00000000aa04', 'twin', {});

/**
 * Sign in a mocked user that is in two groups and serve the agents.
 *
 * @param page - The page whose requests are mocked.
 */
async function setup(page: Parameters<typeof setupMockAuth>[0]) {
  await setupMockAuth(page);
  await page.route('**/api/users/whoami', (route) => fulfillJson(route, { ...MOCK_USER, groups: [...MOCK_USER.groups, OTHER_GROUP] }));
  await mockEntityList(page, [NARROW, WIDE, TWIN_A, TWIN_B]);
}

test.describe('entity pickers and create form checks', () => {
  test('greys out entities that lack a chosen group and tells same-named entities apart', async ({ page }) => {
    await setup(page);
    await page.goto('/workflows/create');
    await page.getByTestId(`groups-${OTHER_GROUP}`).check();
    // the agent that is not in the chosen group can't be picked and says which group it lacks
    await expect(page.getByTestId('workflow-agents-narrow-agent')).toBeDisabled();
    await expect(page.getByTestId('workflow-agents-wide-agent')).toBeEnabled();
    await expect(page.getByTestId('workflow-agents')).toContainText('not in other');
    // same-named agents are told apart by a short id
    await expect(page.getByTestId('workflow-agents')).toContainText('#00000000');
    await expect(page.locator(`[data-entity-id="${TWIN_A.id}"]`)).toBeVisible();
  });

  test('drops a selection that loses eligibility when a group is added', async ({ page }) => {
    await setup(page);
    await page.goto('/workflows/create');
    await page.getByTestId('workflow-agents-narrow-agent').check();
    await expect(page.getByTestId('workflow-agents-narrow-agent')).toBeChecked();
    await page.getByTestId(`groups-${OTHER_GROUP}`).check();
    await expect(page.getByTestId('workflow-agents-narrow-agent')).not.toBeChecked();
  });

  test('reports a number that is not a positive whole number and blocks create', async ({ page }) => {
    await setup(page);
    await page.goto('/agents/create');
    await page.getByTestId('agent-name').fill('numbers');
    await page.getByTestId(`groups-${MOCK_USER.groups[0]}`).check();
    await expect(page.getByTestId('submit-agent')).toBeEnabled();
    await page.getByTestId('agent-max-turns').fill('abc');
    await expect(page.getByTestId('agent-max-turns-error')).toBeVisible();
    await expect(page.getByTestId('submit-agent')).toBeDisabled();
    await page.getByTestId('agent-max-turns').fill('7');
    await expect(page.getByTestId('submit-agent')).toBeEnabled();
  });

  test('shows a not found page with a link back to the list', async ({ page }) => {
    await setupMockAuth(page);
    await mockEntityList(page, []);
    await page.route('**/api/entities/00000000-0000-0000-0000-00000000bb01', (route) =>
      fulfillJson(route, { error: 'Entity not found' }, 404),
    );
    await page.goto('/toolsets/00000000-0000-0000-0000-00000000bb01');
    await expect(page.getByTestId('entity-not-found-title')).toHaveText(/Toolset not found/i);
    await page.getByRole('link', { name: 'Back to toolsets' }).click();
    await expect(page).toHaveURL(/\/toolsets$/);
  });
});
