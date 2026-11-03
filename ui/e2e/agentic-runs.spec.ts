import { expect, test } from '@playwright/test';

import { adminApi, loadFixtures } from './agentic-helpers';
import { loginViaUI } from './helpers';

const fx = loadFixtures();

/** Start a workflow run on the fixture sample through the API and return its reaction id. */
async function startWorkflowRun(): Promise<string> {
  const api = await adminApi();
  const body = { inputs: { samples: [fx.sample_sha256], repos: [] }, group: fx.group, tags: [] };
  const res = await api.post(`/workflows/instances/${fx.workflow_instance}/runs`, body);
  return res.data.id as string;
}

test.describe('runs list and details (live)', () => {
  test.beforeEach(async ({ page }) => {
    await loginViaUI(page);
  });

  test('the runs list shows a started workflow run and filters by kind', async ({ page }) => {
    const id = await startWorkflowRun();
    await page.goto('/runs');
    await expect(page.getByTestId('runs-table')).toBeVisible({ timeout: 30_000 });
    await expect(page.locator(`a[href="/runs/${fx.group}/${id}"]`)).toBeVisible();
    await page.getByRole('tab', { name: /Tool/ }).click();
    await expect(page.locator(`a[href="/runs/${fx.group}/${id}"]`)).toHaveCount(0);
    await page.getByRole('tab', { name: /Workflow/ }).click();
    await expect(page.locator(`a[href="/runs/${fx.group}/${id}"]`)).toBeVisible();
  });

  test('a run row opens the run details', async ({ page }) => {
    const id = await startWorkflowRun();
    await page.goto('/runs');
    await page.locator(`a[href="/runs/${fx.group}/${id}"]`).click();
    await expect(page.getByTestId('run-summary')).toContainText(id);
    await expect(page.getByTestId('run-provenance')).toContainText('uip-scan');
  });

  test('cancel fails an unfinished run and retry starts a new one', async ({ page }) => {
    const id = await startWorkflowRun();
    await page.goto(`/runs/${fx.group}/${id}`);
    await page.getByTestId('run-cancel').click();
    await page.getByTestId('run-cancel-confirm').click();
    await expect(page.getByTestId('run-status')).toHaveText('Failed', { timeout: 30_000 });
    await expect(page.getByTestId('run-cancel')).toHaveCount(0);
    await page.getByTestId('run-retry').click();
    await page.waitForURL((url) => url.pathname.startsWith(`/runs/${fx.group}/`) && !url.pathname.endsWith(id), { timeout: 30_000 });
    await expect(page.getByTestId('run-kind')).toHaveText('Workflow');
  });

  test('a missing run reports an error', async ({ page }) => {
    await page.goto(`/runs/${fx.group}/00000000-0000-0000-0000-000000000000`);
    await expect(page.getByText(/Get Reaction|could not be read|404/)).toBeVisible({ timeout: 30_000 });
  });
});
