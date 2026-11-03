import type { Page } from '@playwright/test';

import { authenticate, buildClient } from './helpers';

// the live fixtures have one loader, shared with the specs that import it from here
export { loadFixtures, uniqueName } from './agentic-fixtures';

/**
 * Get an API client signed in as the test user.
 *
 * @returns An axios client with the user's token.
 */
export async function adminApi() {
  const token = await authenticate(process.env.THORIUM_USER || 'thorium', process.env.THORIUM_PASS || 'password');
  return buildClient(token);
}

/**
 * Wait until the run page shows a finished status.
 *
 * @param page - The page on a run's details route.
 * @param status - The status to wait for.
 */
export async function waitForRunStatus(page: Page, status: 'Completed' | 'Failed') {
  await page.locator('[data-testid="run-status"]', { hasText: status }).waitFor({ timeout: 180_000 });
}
