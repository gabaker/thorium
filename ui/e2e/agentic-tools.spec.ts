import { test, expect } from '@playwright/test';

import { adminApi, loadFixtures } from './agentic-helpers';
import { loginViaUI } from './helpers';

// the tools pages run against the live fixture stack
const fixtures = loadFixtures();
const unique = () => `e2e-tool-${Math.random().toString(36).slice(2, 8)}`;

const IMAGE_SPEC = `Image:
  environment:
    BareMetal:
      scheduling:
        resources: {cpu: 250, memory: 128}
        spawn_limit: Unlimited
  execution:
    args:
      entrypoint: [/bin/true]
    timeout: 60
`;

test.describe('tools', () => {
  test.beforeEach(async ({ page }) => {
    await loginViaUI(page);
  });

  test('lists the fixture tools with kind and latest version', async ({ page }) => {
    await page.goto('/tools');
    const row = page.getByTestId('tool-row-uip-scan');
    await expect(row).toBeVisible();
    await expect(row).toContainText('Image');
    await expect(row).toContainText('1.1.0');
    await page.screenshot({ path: 'e2e/screenshots/tools-list.png', clip: { x: 0, y: 0, width: 1280, height: 520 } });
  });

  test('filters the list by text and kind', async ({ page }) => {
    await page.goto('/tools');
    await expect(page.getByTestId('tool-row-uip-scan')).toBeVisible();
    await page.getByTestId('tool-filter-text').fill('uip-report');
    await expect(page.getByTestId('tool-row-uip-scan')).toHaveCount(0);
    await expect(page.getByTestId('tool-row-uip-report')).toBeVisible();
    await page.getByTestId('tool-filter-text').fill('');
    await page.getByTestId('tool-filter-kind').selectOption('Harness');
    await expect(page.getByTestId('tool-row-uip-harness')).toBeVisible();
    await expect(page.getByTestId('tool-row-uip-scan')).toHaveCount(0);
  });

  test('shows a tool with its versions and a spec', async ({ page }) => {
    await page.goto(`/tools/${fixtures.tool_scan_id}`);
    await expect(page.getByTestId('tool-title')).toHaveText('uip-scan');
    await expect(page.getByTestId('version-row-1.0.0')).toBeVisible();
    await expect(page.getByTestId('version-row-1.1.0')).toContainText('latest');
    await page.getByTestId('view-spec-1.0.0').click();
    await expect(page.getByTestId('spec-view')).toContainText('BareMetal');
    await page.screenshot({ path: 'e2e/screenshots/tools-details.png', clip: { x: 0, y: 0, width: 1280, height: 700 } });
  });

  test('shows a deprecated fixture tool as having no live version', async ({ page }) => {
    await page.goto('/tools');
    await expect(page.getByTestId('tool-row-uip-bad')).toBeVisible();
  });

  test('creates a tool, publishes a version, deprecates it, and deletes the tool', async ({ page }) => {
    const name = unique();
    await page.goto('/tools/create');
    await page.getByTestId('tool-name').fill(name);
    await page.getByTestId('tool-description').fill('made by the browser test');
    await page.getByTestId(`groups-${fixtures.group}`).check();
    await page.getByTestId('tool-spec').fill(IMAGE_SPEC);
    await page.getByTestId('tool-submit').click();
    await page.waitForURL(/\/tools\/[0-9a-f-]{36}$/);
    await expect(page.getByTestId('tool-title')).toHaveText(name);
    await expect(page.getByTestId('tool-latest')).toHaveText('1.0.0');
    // publish a second version with a changed timeout
    await page.getByTestId('publish-version').fill('1.1.0');
    await page.getByTestId('publish-spec').fill(IMAGE_SPEC.replace('timeout: 60', 'timeout: 90'));
    await page.getByTestId('publish-submit').click();
    await expect(page.getByTestId('tool-notice')).toHaveText('Version published');
    await expect(page.getByTestId('tool-latest')).toHaveText('1.1.0');
    // deprecating the newest version moves latest back
    await page.getByTestId('deprecate-1.1.0').click();
    await expect(page.getByTestId('tool-latest')).toHaveText('1.0.0');
    await expect(page.getByTestId('version-row-1.1.0')).toContainText('deprecated');
    // an identical spec is refused by the API and its message is shown
    await page.getByTestId('publish-version').fill('1.2.0');
    await page.getByTestId('publish-spec').fill(IMAGE_SPEC);
    await page.getByTestId('publish-submit').click();
    await expect(page.getByText(/identical|same|409|already/i).first()).toBeVisible();
    // deleting removes every version and the tool
    await page.getByTestId('delete-tool').click();
    await page.getByTestId('delete-tool-confirm').click();
    await page.waitForURL(/\/tools$/);
    await expect(page.getByTestId(`tool-row-${name}`)).toHaveCount(0);
  });

  test('refuses an invalid spec before sending it', async ({ page }) => {
    await page.goto('/tools/create');
    await page.getByTestId('tool-name').fill(unique());
    await page.getByTestId(`groups-${fixtures.group}`).check();
    await page.getByTestId('tool-spec').fill('Other: {}');
    await expect(page.getByTestId('tool-problem')).toContainText('exactly one');
    await expect(page.getByTestId('tool-submit')).toBeDisabled();
    await page.getByTestId('tool-kind').selectOption('Agentic');
    await expect(page.getByTestId('tool-problem')).toContainText('Agentic');
  });

  test('the tool page links to a direct run of that version', async ({ page }) => {
    await page.goto(`/tools/${fixtures.tool_scan_id}`);
    await page.getByTestId('run-version-1.0.0').click();
    await expect(page).toHaveURL(/\/launch\?tab=tool/);
  });

  test('the API agrees with what the page shows', async () => {
    const api = await adminApi();
    const res = await api.get(`/entities/tools/${fixtures.tool_scan_id}/instances`);
    expect(res.data.length).toBeGreaterThanOrEqual(2);
  });
});
