import { expect, test } from '@playwright/test';
import path from 'path';

import { loadFixtures, waitForRunStatus } from './agentic-helpers';
import { chooseRun, openLaunchedRun } from './agentic-mocks';
import { loginViaUI } from './helpers';

const fx = loadFixtures();
const SHOTS = process.env.THORIUM_SCREENSHOTS;

/** Save a small viewport screenshot when a screenshot directory is configured. */
async function shot(page: import('@playwright/test').Page, name: string) {
  if (SHOTS) await page.screenshot({ path: path.join(SHOTS, name) });
}

test.describe('launch a run (live)', () => {
  test.beforeEach(async ({ page }) => {
    await loginViaUI(page);
  });

  test('workflow tab shows the Ready instance and holds back a run with no input', async ({ page }) => {
    await page.goto('/launch');
    await chooseRun(page, 'run-workflow', fx.workflow);
    await expect(page.getByTestId('run-instance-name')).toContainText(`${fx.workflow}#r`);
    await shot(page, 'runs-launch-form.png');
    await page.getByTestId('run-group').selectOption(fx.group);
    // with no input the Run button is disabled and says what the run needs
    await expect(page.getByTestId('run-submit')).toBeDisabled();
    await expect(page.getByTestId('run-submit-hint')).toContainText('Add a file, an existing sample, or a prompt');
  });

  test('workflow tab preselects from the query string', async ({ page }) => {
    await page.goto(`/launch?workflow=${fx.workflow_id}`);
    await expect(page.getByTestId('run-instance-name')).toBeVisible();
  });

  test('launching a workflow with a prompt opens the run with plan, provenance, and source', async ({ page }) => {
    await page.goto('/launch');
    await chooseRun(page, 'run-workflow', fx.workflow);
    await page.getByTestId('run-group').selectOption(fx.group);
    await page.getByTestId('run-prompt').fill('Summarize this sample for the e2e test');
    await page.getByTestId('run-tags').fill('e2e=launch');
    await page.getByTestId('run-submit').click();
    await openLaunchedRun(page);
    await expect(page).toHaveURL(new RegExp(`/runs/${fx.group}/[0-9a-f-]{36}`));
    await expect(page.getByTestId('run-kind')).toHaveText('Workflow');
    await expect(page.getByTestId('plan-stage').first()).toBeVisible();
    await expect(page.getByTestId('run-result-key')).toContainText(`${fx.workflow}#r`);
    await expect(page.getByTestId('provenance-tools')).toContainText('uip-harness');
    await expect(page.getByTestId('provenance-tools')).toContainText('harness');
    await expect(page.getByTestId('run-models')).toContainText('stub-model');
    await expect(page.getByTestId('run-input-samples')).not.toContainText('None');
    await shot(page, 'runs-in-progress.png');
    await page.getByTestId('run-provenance').scrollIntoViewIfNeeded();
    await shot(page, 'runs-provenance.png');
  });

  test('launching a workflow with an uploaded file runs on that file', async ({ page }) => {
    await page.goto('/launch');
    await chooseRun(page, 'run-workflow', fx.workflow);
    await page.getByTestId('run-group').selectOption(fx.group);
    await page
      .getByTestId('run-file')
      .setInputFiles({ name: 'upload.txt', mimeType: 'text/plain', buffer: Buffer.from(`e2e upload ${Date.now()}`) });
    await page.getByTestId('run-submit').click();
    await openLaunchedRun(page);
    await expect(page).toHaveURL(new RegExp(`/runs/${fx.group}/[0-9a-f-]{36}`));
    await expect(page.getByTestId('run-input-samples').locator('a')).toHaveCount(1);
  });

  test('a workflow run completes and shows its results', async ({ page }) => {
    test.setTimeout(240_000);
    await page.goto('/launch');
    await chooseRun(page, 'run-workflow', fx.workflow);
    await page.getByTestId('run-group').selectOption(fx.group);
    await page.getByTestId('run-samples').fill(fx.sample_sha256);
    await page.getByTestId('run-submit').click();
    await openLaunchedRun(page);
    await expect(page).toHaveURL(new RegExp(`/runs/${fx.group}/[0-9a-f-]{36}`));
    await waitForRunStatus(page, 'Completed');
    // the progress row is the completed indicator and text, not a bar
    await expect(page.getByTestId('run-progress').getByRole('img', { name: 'Completed' })).toBeVisible();
    await expect(page.getByTestId('run-input-results')).toBeVisible({ timeout: 30_000 });
    await shot(page, 'runs-completed.png');
  });

  test('tool tab lists runnable tools, versions, and starts a direct run', async ({ page }) => {
    await page.goto('/launch?tab=tool');
    const tools = page.getByTestId('run-tool');
    await expect(tools.locator('label', { hasText: new RegExp(`^${fx.tools[0]}\\(`) })).toHaveCount(1);
    // harness tools cannot be run directly
    await expect(tools.locator('label', { hasText: 'uip-harness' })).toHaveCount(0);
    await chooseRun(page, 'run-tool', fx.tools[0]);
    const versions = page.getByTestId('run-tool-version');
    // every version plus the "Choose a version" placeholder
    await expect(versions.locator('option')).toHaveCount(fx.scan_versions.length + 1);
    await expect(versions.locator('option', { hasText: '(latest)' })).toHaveCount(1);
    await page.getByTestId('run-group').selectOption(fx.group);
    await page.getByTestId('run-samples').fill(fx.sample_sha256);
    await page.getByTestId('run-args-toggle').click();
    await page.getByTestId('run-args-kwargs').fill('--mode=fast');
    await page.getByTestId('run-submit').click();
    await openLaunchedRun(page);
    await expect(page).toHaveURL(new RegExp(`/runs/${fx.group}/[0-9a-f-]{36}`));
    await expect(page.getByTestId('run-kind')).toHaveText('Tool');
    await expect(page.getByTestId('provenance-tools')).toContainText(fx.tools[0]);
  });

  test('a direct tool run completes and shows its logs and results', async ({ page }) => {
    test.setTimeout(240_000);
    await page.goto('/launch?tab=tool');
    await chooseRun(page, 'run-tool', fx.tools[0]);
    await page.getByTestId('run-group').selectOption(fx.group);
    await page
      .getByTestId('run-file')
      .setInputFiles({ name: 'tool-input.txt', mimeType: 'text/plain', buffer: Buffer.from(`tool run input ${Date.now()}`) });
    await page.getByTestId('run-submit').click();
    await openLaunchedRun(page);
    await expect(page).toHaveURL(new RegExp(`/runs/${fx.group}/[0-9a-f-]{36}`));
    await waitForRunStatus(page, 'Completed');
    // a run of one tool shows its stage's logs open, with no plan to expand
    await expect(page.locator('[data-testid^="stage-logs-"]').first()).toBeVisible({ timeout: 30_000 });
    await expect(page.getByTestId('run-result-json').first()).toBeVisible({ timeout: 30_000 });
    await page.getByTestId('run-results').scrollIntoViewIfNeeded();
    await shot(page, 'runs-completed.png');
  });

  test('tool tab holds back malformed keyword args without calling the API', async ({ page }) => {
    await page.goto('/launch?tab=tool');
    await chooseRun(page, 'run-tool', fx.tools[0]);
    await page.getByTestId('run-group').selectOption(fx.group);
    await page.getByTestId('run-samples').fill(fx.sample_sha256);
    await page.getByTestId('run-args-toggle').click();
    await page.getByTestId('run-args-kwargs').fill('broken');
    await expect(page.getByTestId('run-item-blocker')).toContainText('KEY=VALUE');
    await expect(page.getByTestId('run-submit')).toBeDisabled();
  });
});
