import { test } from '@playwright/test';
import type { Page } from '@playwright/test';

import { expect, fulfillJson, mockEntity, mockEntityList } from './agentic-mocks';
import { MOCK_USER } from './helpers';

// Mocked tests: a detail page opened with the id of another kind of entity names what the id is
// instead of failing to render, for every neighbor of the tool page.

/** The id of an entity of another kind than the page under test. */
const OTHER_ID = '00000000-0000-4000-8000-0000000000aa';

/** The detail routes and the kind of entity each one expects. */
const PAGES = [
  { route: '/agents', what: 'Agent', noun: 'an agent', list: 'agents' },
  { route: '/toolsets', what: 'Toolset', noun: 'a toolset', list: 'toolsets' },
  { route: '/workflows', what: 'Workflow', noun: 'a workflow', list: 'workflows' },
  { route: '/workflows/instances', what: 'Workflow instance', noun: 'a workflow instance', list: 'workflows' },
];

/**
 * Sign in as a mocked user and serve one entity of a kind the pages under test never expect.
 *
 * @param page - The page whose requests are mocked.
 */
async function mockOtherKind(page: Page) {
  // catch-all first so the specific routes registered later take priority
  await page.route('**/api/**', (route) => fulfillJson(route, {}));
  await page.route('**/api/users/whoami', (route) => fulfillJson(route, MOCK_USER));
  await page.context().addCookies([{ name: 'THORIUM_TOKEN', value: MOCK_USER.token, domain: 'localhost', path: '/' }]);
  // the entity lists the pages read beside the entity itself are empty
  await mockEntityList(page, []);
  await page.route(`**/api/entities/${OTHER_ID}`, (route) =>
    fulfillJson(route, mockEntity('Tool', OTHER_ID, 'a-tool', { instances: [], latest: '' })),
  );
}

test.describe('detail pages opened with an id of another kind', () => {
  for (const { route, what, noun, list } of PAGES) {
    test(`${route}/<tool id> reports that the id is a tool instead of crashing`, async ({ page }) => {
      await mockOtherKind(page);
      await page.goto(`${route}/${OTHER_ID}`);
      await expect(page.getByTestId('entity-not-found-title')).toHaveText(`${what} not found`);
      // the entity exists, so the page says what it is rather than claiming it cannot be seen
      await expect(page.getByTestId('entity-not-found')).toContainText(`${OTHER_ID} is a tool, not ${noun}`);
      await expect(page.getByTestId('entity-not-found')).not.toContainText('does not exist');
      // the error boundary's page is not what rendered
      await expect(page.getByText('An error occurred while rendering')).toHaveCount(0);
      await page.getByRole('link', { name: `Back to ${list}` }).click();
      await expect(page).toHaveURL(new RegExp(`${list}$`));
    });
  }
});
