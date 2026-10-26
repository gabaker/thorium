import { test, expect } from '@playwright/test';

import { loginViaUI } from './helpers';
import { loadFixtures } from './agentic-fixtures';

const FX = loadFixtures();

test.describe('models (live)', () => {
  test.beforeEach(async ({ page }) => {
    await loginViaUI(page);
  });

  test('lists endpoints with their models and shows a banned endpoint with its notification', async ({ page }) => {
    await page.goto('/models');
    for (const endpoint of FX.model_endpoints) {
      await expect(page.getByTestId(`endpoint-row-${endpoint}`)).toBeVisible({ timeout: 20000 });
    }
    // the banners sit in a full width row under the endpoint, not in its name cell
    const banned = page.getByTestId(`endpoint-notes-${FX.banned_endpoint}`);
    await expect(banned.getByTestId('model-notification').first()).toBeVisible({ timeout: 20000 });
    await expect(banned).not.toContainText('`');
  });

  test('switches to the credential tab without showing a secret', async ({ page }) => {
    await page.goto('/models');
    await page.getByRole('tab', { name: /Credentials/ }).click();
    await expect(page.getByTestId('credential-table')).toBeVisible();
    await expect(page.getByTestId('credential-table')).not.toContainText(/secret|password|sk-/i);
  });

  test('lists profiles', async ({ page }) => {
    await page.goto('/models');
    await page.getByRole('tab', { name: /Profiles/ }).click();
    await expect(page.getByTestId('profile-table')).toBeVisible();
  });
});
