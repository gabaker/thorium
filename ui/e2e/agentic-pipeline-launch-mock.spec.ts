import { test } from '@playwright/test';

import { expect, fulfillJson, MOCK_GROUP } from './agentic-mocks';
import { setupMockAuth } from './helpers';

// a pipeline with an image stage: the stage has no stage tool, so the API refuses to run it
const LEGACY_PIPELINE = { group: MOCK_GROUP, name: 'legacy-pipe', order: [['harvest']], sla: 3600, creator: 'test', bans: {} };

test.describe('pipeline launch form', () => {
  test('a blocked pipeline cannot be run and uploads nothing', async ({ page }) => {
    await setupMockAuth(page);
    await page.route(`**/api/pipelines/list/${MOCK_GROUP}/details/**`, (route) => fulfillJson(route, { details: [LEGACY_PIPELINE] }));
    // record every upload and reaction create the form sends
    const writes: string[] = [];
    await page.route('**/api/files/', (route) => {
      writes.push(`${route.request().method()} files`);
      return fulfillJson(route, { sha256: 'b'.repeat(64) });
    });
    await page.route('**/api/reactions/', (route) => {
      writes.push(`${route.request().method()} reactions`);
      return fulfillJson(route, { error: 'pipeline has stages with no tool' }, 400);
    });
    await page.goto('/launch?tab=pipeline');
    await page.getByTestId(`run-pipeline-${MOCK_GROUP}/legacy-pipe`).check();
    await expect(page.getByTestId('pipeline-blocker')).toContainText('harvest');
    await page.getByTestId('run-prompt').fill('summarize this');
    await expect(page.getByTestId('run-submit')).toBeDisabled();
    // a click on the disabled button must not start the run either
    await page.getByTestId('run-submit').click({ force: true });
    await page.waitForTimeout(500);
    expect(writes.filter((write) => write.startsWith('POST'))).toEqual([]);
  });
});
