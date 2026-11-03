import { test, expect } from '@playwright/test';

import { loginViaUI } from './helpers';
import { loadFixtures, uniqueName } from './agentic-fixtures';

const FX = loadFixtures();

test.describe('toolsets (live)', () => {
  test.beforeEach(async ({ page }) => {
    await loginViaUI(page);
  });

  test('lists the seeded toolset with its settings and latest instance', async ({ page }) => {
    await page.goto('/toolsets');
    const row = page.getByTestId(`toolset-row-${FX.toolset}`);
    await expect(row).toBeVisible({ timeout: 20000 });
    await expect(row).toContainText(FX.group);
    await expect(row).toContainText(`${FX.toolset}#r`);
  });

  test('shows a toolset instance with members and status', async ({ page }) => {
    await page.goto('/toolsets');
    await page.getByTestId(`toolset-row-${FX.toolset}`).getByRole('link', { name: FX.toolset }).click();
    await expect(page.getByTestId('toolset-title')).toHaveText(FX.toolset);
    await expect(page.getByTestId('toolset-instance-view')).toBeVisible({ timeout: 20000 });
    await expect(page.getByTestId('toolset-members')).toContainText(/uip-/);
    await expect(page.getByTestId('toolset-instance-view')).toContainText('Ready');
    // the shown badge keeps a gap from the instance name before it
    const shown = page.getByTestId('toolset-instances').getByText('shown', { exact: true });
    await expect(shown).toBeVisible();
    expect(await shown.evaluate((el) => parseFloat(getComputedStyle(el).marginLeft))).toBeGreaterThan(0);
  });

  test('creates, edits, requests an instance for, and deletes a toolset', async ({ page }) => {
    // the instance poll waits up to 60s after the form and edit steps, past the 60s default
    test.setTimeout(120_000);
    const name = uniqueName('toolsets');
    await page.goto('/toolsets/create');
    await page.getByTestId('toolset-name').fill(name);
    await page.getByTestId('toolset-description').fill('created by e2e');
    await page.getByTestId(`groups-${FX.group}`).check();
    await page.getByTestId('tools-uip-scan-call').check();
    await page.getByTestId('submit-toolset').click();
    await expect(page.getByTestId('toolset-title')).toHaveText(name, { timeout: 20000 });
    await expect(page.getByTestId('toolset-tools')).toContainText('uip-scan-call');
    // edit the description and flip the read only flag
    await page.getByTestId('edit-description').fill('edited');
    await page.getByTestId('save-description').click();
    await expect(page.getByText('Description saved')).toBeVisible();
    await page.getByTestId('toggle-read-only').click();
    await expect(page.getByText('Read only changed')).toBeVisible();
    await page.getByTestId('request-instance').click();
    await expect(page.getByText(/Instance requested/)).toBeVisible();
    // the event handler builds the instance shortly after
    await expect(async () => {
      await page.getByTestId('reload').click();
      await expect(page.getByTestId('toolset-instances')).toContainText(`${name}#r1`, { timeout: 2000 });
    }).toPass({ timeout: 60000 });
    // delete it
    await page.getByTestId('delete-toolset').click();
    await page.getByTestId('delete-toolset-confirm').click();
    await expect(page).toHaveURL(/\/toolsets$/);
    await expect(page.getByTestId(`toolset-row-${name}`)).toHaveCount(0);
  });
});
