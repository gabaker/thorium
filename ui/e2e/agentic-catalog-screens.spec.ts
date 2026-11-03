import { test } from '@playwright/test';

import { loginViaUI } from './helpers';
import { loadFixtures } from './agentic-fixtures';

const DIR = process.env.THORIUM_SCREENSHOT_DIR;
const FX = loadFixtures();

// Captures small viewport screenshots of the catalog pages when a target directory is given.
test.describe('catalog screenshots', () => {
  test.skip(!DIR, 'set THORIUM_SCREENSHOT_DIR to capture screenshots');
  test.use({ viewport: { width: 1100, height: 650 } });

  test('captures toolsets, a workflow instance, and models', async ({ page }) => {
    await loginViaUI(page);
    await page.goto('/toolsets');
    await page.getByTestId(`toolset-row-${FX.toolset}`).waitFor();
    await page.screenshot({ path: `${DIR}/catalog-toolsets.png` });
    await page.goto('/workflows');
    await page.getByTestId(`workflow-row-${FX.workflow}`).getByRole('link', { name: FX.workflow }).click();
    await page.getByTestId('workflow-instances').getByRole('link').first().waitFor();
    await page.screenshot({ path: `${DIR}/catalog-workflow.png` });
    await page.goto('/models');
    await page.getByTestId(`endpoint-notes-${FX.banned_endpoint}`).getByTestId('model-notification').first().waitFor();
    await page.screenshot({ path: `${DIR}/catalog-models.png` });
  });
});
