import { expect, test } from '@playwright/test';

import { loadFixtures, waitForRunStatus } from './agentic-helpers';
import { chooseRun, openLaunchedRun } from './agentic-mocks';
import { loginViaUI } from './helpers';

// the pipeline run tab runs against the live fixture stack
const fx = loadFixtures();

test.describe('run a pipeline (live)', () => {
  test.beforeEach(async ({ page }) => {
    await loginViaUI(page);
  });

  test('a pipeline run on an existing sample completes and shows its tool plan', async ({ page }) => {
    // waitForRunStatus waits up to 180s for the run to finish
    test.setTimeout(240_000);
    await page.goto('/launch?tab=pipeline');
    await chooseRun(page, 'run-pipeline', 'uip-pipe');
    await page.getByTestId('run-samples').fill(fx.sample_sha256);
    await page.getByTestId('run-submit').click();
    await openLaunchedRun(page);
    await waitForRunStatus(page, 'Completed');
    await expect(page.getByText('scan').first()).toBeVisible();
  });

  test('a banned pipeline names its ban and cannot be launched', async ({ page }) => {
    await page.goto('/launch?tab=pipeline');
    await chooseRun(page, 'run-pipeline', fx.banned_pipeline);
    await expect(page.getByTestId('pipeline-blocker')).toContainText('bans');
    await expect(page.getByTestId('run-submit')).toBeDisabled();
  });
});
