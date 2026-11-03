import { test, expect } from '@playwright/test';

import { loginViaUI } from './helpers';
import { loadFixtures, uniqueName } from './agentic-fixtures';

const FX = loadFixtures();

test.describe('workflows (live)', () => {
  test.beforeEach(async ({ page }) => {
    await loginViaUI(page);
  });

  test('lists the seeded workflow with a Ready instance', async ({ page }) => {
    await page.goto('/workflows');
    const row = page.getByTestId(`workflow-row-${FX.workflow}`);
    await expect(row).toBeVisible({ timeout: 20000 });
    await expect(row).toContainText(`${FX.workflow}#r`);
    await expect(row).toContainText('Ready');
  });

  test('shows workflow details, an instance, and the per-agent tool specs', async ({ page }) => {
    await page.goto('/workflows');
    await page.getByTestId(`workflow-row-${FX.workflow}`).getByRole('link', { name: FX.workflow }).click();
    await expect(page.getByTestId('workflow-title')).toHaveText(FX.workflow);
    await expect(page.getByTestId('workflow-agents')).toContainText(FX.agent);
    await expect(page.getByTestId('workflow-info')).toContainText(FX.group);
    const instances = page.getByTestId('workflow-instances');
    await expect(instances).toContainText('Ready');
    await instances.getByRole('link').first().click();
    await expect(page.getByTestId('instance-info')).toContainText('Ready');
    await expect(page.getByTestId('instance-agents')).toContainText(FX.agent);
    await expect(page.getByTestId('instance-tools')).toContainText('uip-');
    await page.getByTestId(`toolset-${FX.agent}`).click();
    await expect(page.getByTestId('agent-toolset')).toBeVisible({ timeout: 20000 });
  });

  test('links to the launch page', async ({ page }) => {
    await page.goto('/workflows');
    await page.getByTestId(`workflow-row-${FX.workflow}`).getByRole('link', { name: FX.workflow }).click();
    await page.getByTestId('run-workflow').click();
    await expect(page).toHaveURL(/\/launch\?workflow=/);
  });

  test('creates a workflow, sets its entry agent, requests an instance, and deletes it', async ({ page }) => {
    // the instance poll waits up to 90s after the form steps, past the 60s default
    test.setTimeout(180_000);
    const name = uniqueName('workflows');
    await page.goto('/workflows/create');
    await page.getByTestId('workflow-name').fill(name);
    await page.getByTestId(`groups-${FX.group}`).check();
    await page.getByTestId(`workflow-agents-${FX.agent}`).check();
    await page.getByTestId('workflow-entry').selectOption({ label: FX.agent });
    await page.getByTestId('workflow-alive').fill('3');
    await page.getByTestId('submit-workflow').click();
    await expect(page.getByTestId('workflow-title')).toHaveText(name, { timeout: 20000 });
    await expect(page.getByTestId('workflow-info')).toContainText(FX.agent);
    await expect(page.getByTestId('workflow-info')).toContainText('3');
    // clear and set the entry agent again through the edit controls
    await page.getByTestId('clear-entry').click();
    await expect(page.getByText('Entry agent cleared')).toBeVisible();
    await page.getByTestId('entry-select').selectOption({ label: FX.agent });
    await page.getByTestId('set-entry').click();
    await expect(page.getByText('Entry agent set')).toBeVisible();
    await page.getByTestId('request-instance').click();
    await expect(async () => {
      await page.getByTestId('reload').click();
      await expect(page.getByTestId('workflow-instances')).toContainText(`${name}#r1`, { timeout: 2000 });
    }).toPass({ timeout: 90000 });
    await page.getByTestId('delete-workflow').click();
    await page.getByTestId('delete-workflow-confirm').click();
    await expect(page).toHaveURL(/\/workflows$/);
    await expect(page.getByTestId(`workflow-row-${name}`)).toHaveCount(0);
  });
});
