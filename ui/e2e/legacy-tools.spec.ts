import { test, expect } from '@playwright/test';

import { adminApi, loadFixtures, uniqueName } from './agentic-helpers';
import { loginViaUI } from './helpers';

// the legacy image page and the pipeline page run against the live fixture stack
const fixtures = loadFixtures();

test.describe('legacy images and pipeline pages', () => {
  test.beforeEach(async ({ page }) => {
    await loginViaUI(page);
  });

  test('the images page says images are legacy', async ({ page }) => {
    await page.goto('/images');
    await expect(page.getByTestId('images-note')).toContainText('legacy');
    await expect(page.getByTestId('create-image-btn')).toHaveCount(0);
  });

  test('creating an image explains the 410 and links to tool creation', async ({ page }) => {
    await page.goto('/create/image');
    await expect(page.getByTestId('image-gone')).toContainText('410');
    await page.getByTestId('goto-create-tool').click();
    await expect(page).toHaveURL(/\/tools\/create$/);
  });

  test('the pipelines page lists the fixture pipeline with its stage tools', async ({ page }) => {
    await page.goto('/pipelines');
    await expect(page.getByTestId('pipelines-note')).toContainText('tools');
    await page.getByText('uip-pipe', { exact: true }).first().click();
    await expect(page.getByTestId('stage-tools').first()).toBeVisible();
    await page.screenshot({ path: 'e2e/screenshots/pipelines-stage-tools.png', clip: { x: 0, y: 0, width: 1280, height: 760 } });
  });

  test('creates a pipeline whose stage is backed by a tool and deletes it', async ({ page }) => {
    const name = uniqueName('pipe');
    await page.goto('/create/pipeline');
    await page.getByTestId('pipeline-name').fill(name);
    await page.getByTestId('pipeline-group').selectOption(fixtures.group);
    await page.getByTestId('stage-name-0-0').fill('scan');
    await page.getByTestId('stage-tool-0-0').selectOption({ label: 'uip-scan' });
    await page.getByTestId('create-submit').click();
    await page.waitForURL(/\/pipelines$/);
    const api = await adminApi();
    const created = await api.get(`/pipelines/data/${fixtures.group}/${name}`);
    expect(created.data.order).toEqual([['scan']]);
    expect(Object.keys(created.data.stage_tools)).toEqual(['scan']);
    await api.delete(`/pipelines/${fixtures.group}/${name}`);
  });

  test('a pipeline form with a stage and no tool cannot be submitted', async ({ page }) => {
    await page.goto('/create/pipeline');
    await page.getByTestId('pipeline-name').fill(uniqueName('pipe'));
    await page.getByTestId('pipeline-group').selectOption(fixtures.group);
    await page.getByTestId('stage-name-0-0').fill('scan');
    await expect(page.getByTestId('pipeline-problem')).toContainText('Choose a tool');
    await expect(page.getByTestId('create-submit')).toBeDisabled();
  });
});
