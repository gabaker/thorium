import { test, expect } from '@playwright/test';

import { loginViaUI } from './helpers';
import { loadFixtures, uniqueName } from './agentic-fixtures';

const FX = loadFixtures();

test.describe('agents (live)', () => {
  test.beforeEach(async ({ page }) => {
    await loginViaUI(page);
  });

  test('lists the seeded agent and shows its configuration', async ({ page }) => {
    await page.goto('/agents');
    const row = page.getByTestId(`agent-row-${FX.agent}`);
    await expect(row).toBeVisible({ timeout: 20000 });
    await row.getByRole('link', { name: FX.agent }).click();
    await expect(page.getByTestId('agent-title')).toHaveText(FX.agent);
    await expect(page.getByTestId('agent-info')).toContainText(FX.group);
    await expect(page.getByTestId('agent-info')).toContainText('Harness');
  });

  test('creates an agent with a harness and tools, then deletes it', async ({ page }) => {
    const name = uniqueName('agents');
    await page.goto('/agents/create');
    await page.getByTestId('agent-name').fill(name);
    await page.getByTestId(`groups-${FX.group}`).check();
    await page.getByTestId('agent-system-prompt').fill('You are a test agent');
    await page.getByTestId('agent-prompt').fill('Summarize the input');
    await page.getByTestId('agent-max-turns').fill('7');
    await page.getByTestId('agent-harness').selectOption({ label: 'uip-harness' });
    await page.getByTestId('agent-tools-uip-scan-call').check();
    await page.getByTestId('submit-agent').click();
    await expect(page.getByTestId('agent-title')).toHaveText(name, { timeout: 20000 });
    await expect(page.getByTestId('agent-info')).toContainText('uip-harness');
    await expect(page.getByTestId('agent-info')).toContainText('uip-scan-call');
    await expect(page.getByTestId('agent-info')).toContainText('7');
    await expect(page.getByTestId('agent-task-prompt')).toHaveText('Summarize the input');
    await page.getByTestId('delete-agent').click();
    await page.getByTestId('delete-agent-confirm').click();
    await expect(page).toHaveURL(/\/agents$/);
    await expect(page.getByTestId(`agent-row-${name}`)).toHaveCount(0);
  });
});
