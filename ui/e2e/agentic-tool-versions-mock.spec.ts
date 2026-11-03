import { test, Page } from '@playwright/test';

import { expect, fulfillJson, mockEntity, mockInstanceSummary } from './agentic-mocks';
import { setupMockAuth } from './helpers';

const TOOL_ID = '00000000-0000-0000-0000-00000000dd01';

/**
 * Serve a tool whose versions can be deleted, moving latest to the newest surviving version like the API does.
 *
 * @param page - The page whose requests are mocked.
 * @param versions - The tool's version labels, newest first.
 * @param failing - The version labels whose delete the API refuses with a server error.
 */
async function mockToolVersions(page: Page, versions: string[], failing: string[] = []) {
  const live = versions.map((version) => mockInstanceSummary(`inst-${version}`, version));
  const tool = () => {
    const latest = live[0] ?? null;
    return mockEntity('Tool', TOOL_ID, 'demo-tool', {
      kind: 'Image',
      latest: latest?.id ?? null,
      latest_version: latest?.version ?? null,
      latest_spec_hash: latest?.spec_hash ?? null,
      modified: '2026-10-01T00:00:00Z',
      // the API leaves the list out when it is empty
      ...(live.length > 0 ? { instances: live } : {}),
    });
  };
  await setupMockAuth(page);
  await page.route('**/api/pipelines/list/**', (route) => fulfillJson(route, { details: [] }));
  await page.route(`**/api/entities/tools/${TOOL_ID}/instances`, (route) => fulfillJson(route, live));
  await page.route('**/api/entities/*', (route) => {
    const id = new URL(route.request().url()).pathname.split('/').pop() ?? '';
    if (id === TOOL_ID) return fulfillJson(route, tool());
    const index = live.findIndex((entry) => entry.id === id);
    if (index < 0) return fulfillJson(route, { error: `entity ${id} not found` }, 404);
    if (route.request().method() === 'DELETE') {
      if (failing.includes(live[index].version.SemVer)) return fulfillJson(route, { error: 'delete failed' }, 500);
      live.splice(index, 1);
      return route.fulfill({ status: 204 });
    }
    const summary = live[index];
    return fulfillJson(
      route,
      mockEntity('ToolInstance', id, `demo-tool@${summary.version.SemVer}`, {
        tool: TOOL_ID,
        version: summary.version,
        previous: null,
        spec_hash: summary.spec_hash,
        deprecated: false,
        spec: { Image: { marker: `spec-of-${summary.version.SemVer}` } },
      }),
    );
  });
}

/**
 * Delete one version through its two-step confirm button.
 *
 * @param page - The page on the tool's details route.
 * @param version - The version label to delete.
 */
async function deleteVersion(page: Page, version: string) {
  await page.getByTestId(`delete-version-${version}`).click();
  await page.getByTestId(`delete-version-${version}-confirm`).click();
  await expect(page.getByTestId('tool-notice')).toHaveText('Version deleted');
}

test.describe('tool versions', () => {
  test('an API error marks names as code instead of showing backticks, with a gap below the control', async ({ page }) => {
    await mockToolVersions(page, ['1.1.0', '1.0.0']);
    // a later route takes precedence over the mocks above
    await page.route('**/api/entities/inst-1.0.0', (route) =>
      route.request().method() === 'DELETE'
        ? fulfillJson(route, { error: 'version `1.0.0` of tool `demo-tool` is pinned' }, 409)
        : route.fallback(),
    );
    await page.goto(`/tools/${TOOL_ID}`);
    await page.getByTestId('delete-version-1.0.0').click();
    await page.getByTestId('delete-version-1.0.0-confirm').click();
    const banner = page.getByText('is pinned');
    await expect(banner).toContainText('version 1.0.0 of tool demo-tool is pinned');
    await expect(banner.locator('code, span').filter({ hasText: '1.0.0' }).first()).toBeVisible();
    await expect(banner).not.toContainText('`');
  });

  test('deleting the selected latest version moves the spec to the new latest', async ({ page }) => {
    await mockToolVersions(page, ['1.2.0', '1.1.0', '1.0.0']);
    await page.goto(`/tools/${TOOL_ID}`);
    await expect(page.getByTestId('spec-view')).toContainText('spec-of-1.2.0');
    await deleteVersion(page, '1.2.0');
    await expect(page.getByTestId('spec-view')).toContainText('spec-of-1.1.0');
    await expect(page.getByText('Get Entity')).toHaveCount(0);
  });

  test('choosing another version while an earlier spec is loading keeps the last choice on screen', async ({ page }) => {
    await mockToolVersions(page, ['1.2.0', '1.1.0', '1.0.0']);
    // the spec of 1.1.0 is read slowly, so the read of 1.0.0 finishes first
    await page.route('**/api/entities/inst-1.1.0', async (route) => {
      await new Promise((resolve) => setTimeout(resolve, 1500));
      await route.fallback();
    });
    await page.goto(`/tools/${TOOL_ID}`);
    await expect(page.getByTestId('spec-view')).toContainText('spec-of-1.2.0');
    await page.getByTestId('view-spec-1.1.0').click();
    await page.getByTestId('view-spec-1.0.0').click();
    await expect(page.getByTestId('spec-view')).toContainText('spec-of-1.0.0');
    // wait past the slow read: it must not replace the spec of the version chosen last
    await page.waitForTimeout(2000);
    await expect(page.getByTestId('spec-view')).toContainText('spec-of-1.0.0');
    await expect(page.getByTestId('spec-view')).not.toContainText('spec-of-1.1.0');
  });

  test('an error from a spec read the user has moved past is not shown beside the newer spec', async ({ page }) => {
    await mockToolVersions(page, ['1.2.0', '1.1.0', '1.0.0']);
    // the read of 1.1.0 fails slowly, after the user has chosen 1.0.0
    await page.route('**/api/entities/inst-1.1.0', async (route) => {
      await new Promise((resolve) => setTimeout(resolve, 1500));
      await fulfillJson(route, { error: 'injected failure for stale read' }, 500);
    });
    await page.goto(`/tools/${TOOL_ID}`);
    await expect(page.getByTestId('spec-view')).toContainText('spec-of-1.2.0');
    await page.getByTestId('view-spec-1.1.0').click();
    await page.getByTestId('view-spec-1.0.0').click();
    await expect(page.getByTestId('spec-view')).toContainText('spec-of-1.0.0');
    // wait past the failing read: its error must not appear for the version now shown
    await page.waitForTimeout(2000);
    await expect(page.getByText('injected failure for stale read')).toHaveCount(0);
    await expect(page.getByTestId('spec-view')).toContainText('spec-of-1.0.0');
  });

  test('deleting a selected older version stops showing its spec', async ({ page }) => {
    await mockToolVersions(page, ['1.2.0', '1.1.0', '1.0.0']);
    await page.goto(`/tools/${TOOL_ID}`);
    await page.getByTestId('view-spec-1.0.0').click();
    await expect(page.getByTestId('spec-view')).toContainText('spec-of-1.0.0');
    await deleteVersion(page, '1.0.0');
    await expect(page.getByTestId('spec-view')).toContainText('spec-of-1.2.0');
    await expect(page.getByText('Get Entity')).toHaveCount(0);
  });

  test('deleting the only version leaves no spec behind', async ({ page }) => {
    await mockToolVersions(page, ['1.0.0']);
    await page.goto(`/tools/${TOOL_ID}`);
    await expect(page.getByTestId('spec-view')).toContainText('spec-of-1.0.0');
    await deleteVersion(page, '1.0.0');
    await expect(page.getByTestId('tool-latest')).toContainText('no live version');
    await expect(page.getByTestId('spec-view')).toHaveCount(0);
    await expect(page.getByText('Get Entity')).toHaveCount(0);
  });

  test('a delete tool that fails partway reloads the versions it already deleted', async ({ page }) => {
    await mockToolVersions(page, ['1.2.0', '1.1.0'], ['1.1.0']);
    await page.goto(`/tools/${TOOL_ID}`);
    await expect(page.getByTestId('version-row-1.2.0')).toBeVisible();
    await page.getByTestId('delete-tool').click();
    await page.getByTestId('delete-tool-confirm').click();
    // the newest version was deleted before the second delete failed, so the list must not still show it
    await expect(page.getByTestId('version-row-1.2.0')).toHaveCount(0);
    await expect(page.getByTestId('version-row-1.1.0')).toBeVisible();
    await expect(page.getByText('delete failed')).toBeVisible();
    // the banner says the newest version is already gone
    await expect(page.getByText('version 1.2.0 was already deleted')).toBeVisible();
    // what was already lost leads the banner on its own line, ahead of the API refusal
    const lines = page.getByTestId('tool-error-line');
    await expect(lines.first()).toContainText('version 1.2.0 was already deleted');
    await expect(lines.nth(1)).toContainText('delete failed');
  });

  test('keeps every version action inside the table at 1024 px', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 800 });
    await mockToolVersions(page, ['1.2.0', '1.1.0', '1.0.0']);
    await page.goto(`/tools/${TOOL_ID}`);
    const table = page.getByTestId('version-table');
    await expect(table).toBeVisible();
    // the scroll box around the table has nothing to scroll
    const [scroll, client] = await table.evaluate((el) => [el.parentElement?.scrollWidth ?? 0, el.parentElement?.clientWidth ?? 0]);
    expect(scroll).toBeLessThanOrEqual(client);
    // each Delete button ends inside the scroll box
    const box = await table.evaluate((el) => el.parentElement?.getBoundingClientRect().right ?? 0);
    for (const version of ['1.2.0', '1.1.0', '1.0.0']) {
      const button = await page.getByTestId(`delete-version-${version}`).boundingBox();
      expect((button?.x ?? 0) + (button?.width ?? 0)).toBeLessThanOrEqual(box);
    }
  });
});
