import { test, Page } from '@playwright/test';

import { expect, fulfillJson, mockEntity, mockInstanceSummary } from './agentic-mocks';
import { setupMockAuth } from './helpers';

const IMAGE_TOOL_ID = '00000000-0000-0000-0000-00000000f101';
const IMAGE_INSTANCE_ID = '00000000-0000-0000-0000-00000000f102';
const AGENTIC_TOOL_ID = '00000000-0000-0000-0000-00000000f103';
const AGENTIC_INSTANCE_ID = '00000000-0000-0000-0000-00000000f104';

/**
 * Serve an agentic tool pinned to a version of an Image tool.
 *
 * @param page - The page whose requests are mocked.
 * @param readable - Whether the pinned Image tool instance can be read.
 */
async function mockPinnedTool(page: Page, readable: boolean) {
  const version = mockInstanceSummary(AGENTIC_INSTANCE_ID, '1.0.0');
  await setupMockAuth(page);
  await page.route('**/api/pipelines/list/**', (route) => fulfillJson(route, { details: [] }));
  await page.route(`**/api/entities/tools/${AGENTIC_TOOL_ID}/instances`, (route) => fulfillJson(route, [version]));
  await page.route('**/api/entities/*', (route) => {
    const id = new URL(route.request().url()).pathname.split('/').pop() ?? '';
    if (id === AGENTIC_TOOL_ID) {
      return fulfillJson(
        route,
        mockEntity('Tool', AGENTIC_TOOL_ID, 'pinner', {
          kind: 'Agentic',
          latest: version.id,
          latest_version: version.version,
          instances: [version],
          modified: '',
        }),
      );
    }
    if (id === AGENTIC_INSTANCE_ID) {
      return fulfillJson(
        route,
        mockEntity('ToolInstance', id, 'pinner@1.0.0', {
          tool: AGENTIC_TOOL_ID,
          version: version.version,
          previous: null,
          spec_hash: version.spec_hash,
          deprecated: false,
          spec: { Agentic: { function_name: 'pin', backing: { Container: { tool_instance: IMAGE_INSTANCE_ID, inputs: {} } } } },
        }),
      );
    }
    if (id === IMAGE_INSTANCE_ID && readable) {
      return fulfillJson(
        route,
        mockEntity('ToolInstance', id, 'pinbase@1.2.0', {
          tool: IMAGE_TOOL_ID,
          version: { SemVer: '1.2.0' },
          previous: null,
          spec_hash: 'h',
          deprecated: false,
          spec: { Image: {} },
        }),
      );
    }
    return fulfillJson(route, { error: `entity ${id} not found` }, 404);
  });
}

test.describe('tool pins and publish feedback', () => {
  test('an agentic tool names and links the image tool version it is pinned to', async ({ page }) => {
    await mockPinnedTool(page, true);
    await page.goto(`/tools/${AGENTIC_TOOL_ID}`);
    const pinned = page.getByTestId('pinned-backing');
    await expect(pinned).toContainText('pinbase@1.2.0');
    await expect(pinned.locator('a')).toHaveAttribute('href', `/tools/${IMAGE_TOOL_ID}`);
  });

  test('an unreadable pinned version leaves its id shown', async ({ page }) => {
    await mockPinnedTool(page, false);
    await page.goto(`/tools/${AGENTIC_TOOL_ID}`);
    await expect(page.getByTestId('pinned-backing')).toHaveText(IMAGE_INSTANCE_ID);
  });

  test('the published notice stays in view when the new version loads late and adds a tag row above', async ({ page }) => {
    const versions = [mockInstanceSummary('inst-1.0.0', '1.0.0')];
    let published = false;
    const tool = () =>
      mockEntity('Tool', IMAGE_TOOL_ID, 'pub-tool', {
        kind: 'Image',
        latest: versions[0].id,
        latest_version: versions[0].version,
        instances: versions,
        modified: '',
      });
    await setupMockAuth(page);
    await page.setViewportSize({ width: 1280, height: 600 });
    await page.route('**/api/pipelines/list/**', (route) => fulfillJson(route, { details: [] }));
    await page.route(`**/api/entities/tools/${IMAGE_TOOL_ID}/instances`, (route) => fulfillJson(route, versions));
    await page.route('**/api/entities/*', async (route) => {
      const id = new URL(route.request().url()).pathname.split('/').pop() ?? '';
      if (id === IMAGE_TOOL_ID && route.request().method() === 'PATCH') {
        versions.unshift(mockInstanceSummary('inst-1.1.0', '1.1.0'));
        published = true;
        return route.fulfill({ status: 204 });
      }
      if (id === IMAGE_TOOL_ID) return fulfillJson(route, tool());
      const entity = mockEntity('ToolInstance', id, `pub-tool@${id.replace('inst-', '')}`, {
        tool: IMAGE_TOOL_ID,
        version: versions.find((entry) => entry.id === id)?.version,
        previous: null,
        spec_hash: 'h',
        deprecated: false,
        spec: { Image: { marker: id } },
      });
      // the new version reads late and carries enough tags to wrap onto several lines
      if (id === 'inst-1.1.0') {
        await new Promise((resolve) => setTimeout(resolve, 800));
        const tags = Object.fromEntries(Array.from({ length: 40 }, (_, index) => [`ToolTag${index}`, { [`value-number-${index}`]: [] }]));
        return fulfillJson(route, { ...entity, tags });
      }
      return fulfillJson(route, entity);
    });
    await page.goto(`/tools/${IMAGE_TOOL_ID}`);
    await expect(page.getByTestId('spec-view')).toContainText('inst-1.0.0');
    await page.getByTestId('publish-version').fill('1.1.0');
    await page.getByTestId('publish-spec').fill('Image:\n  marker: changed\n');
    await page.getByTestId('publish-submit').click();
    const notice = page.getByTestId('tool-notice');
    await expect(notice).toHaveText('Version published');
    // wait for the late version, whose tag row moves the publish form down the page
    await expect(page.getByTestId('version-tags')).toBeVisible();
    expect(published).toBe(true);
    await expect(notice).toBeInViewport();
  });

  test('the publish spec holds the latest spec when another version is chosen before the latest has loaded', async ({ page }) => {
    const versions = [mockInstanceSummary('inst-1.1.0', '1.1.0'), mockInstanceSummary('inst-1.0.0', '1.0.0')];
    await setupMockAuth(page);
    await page.route('**/api/pipelines/list/**', (route) => fulfillJson(route, { details: [] }));
    await page.route(`**/api/entities/tools/${IMAGE_TOOL_ID}/instances`, (route) => fulfillJson(route, versions));
    // the latest's first read is slow so the older version can be chosen while it is pending
    let latestReads = 0;
    await page.route('**/api/entities/*', async (route) => {
      const id = new URL(route.request().url()).pathname.split('/').pop() ?? '';
      if (id === IMAGE_TOOL_ID) {
        return fulfillJson(
          route,
          mockEntity('Tool', IMAGE_TOOL_ID, 'race-tool', {
            kind: 'Image',
            latest: versions[0].id,
            latest_version: versions[0].version,
            instances: versions,
            modified: '',
          }),
        );
      }
      const entity = mockEntity('ToolInstance', id, `race-tool@${id.replace('inst-', '')}`, {
        tool: IMAGE_TOOL_ID,
        version: versions.find((entry) => entry.id === id)?.version,
        previous: null,
        spec_hash: 'h',
        deprecated: false,
        spec: { Image: { marker: id } },
      });
      if (id === 'inst-1.1.0' && latestReads++ === 0) await new Promise((resolve) => setTimeout(resolve, 2500));
      return fulfillJson(route, entity);
    });
    await page.goto(`/tools/${IMAGE_TOOL_ID}`);
    await page.getByTestId('view-spec-1.0.0').click();
    await expect(page.getByTestId('spec-view')).toContainText('inst-1.0.0');
    // the form starts from the latest spec, not the chosen older one, and not empty
    await expect(page.getByTestId('publish-spec')).toHaveValue(/inst-1\.1\.0/);
  });

  test("opening another tool of the same kind starts its publish form from its own spec, not the first tool's text", async ({ page }) => {
    const tools = ['00000000-0000-0000-0000-00000000f201', '00000000-0000-0000-0000-00000000f202'];
    const versionOf = (toolId: string) => mockInstanceSummary(`inst-${toolId}`, '1.0.0');
    await setupMockAuth(page);
    await page.route('**/api/pipelines/list/**', (route) => fulfillJson(route, { details: [] }));
    await page.route('**/api/entities/tools/*/instances', (route) => {
      const toolId = new URL(route.request().url()).pathname.split('/').slice(-2)[0];
      return fulfillJson(route, [versionOf(toolId)]);
    });
    await page.route('**/api/entities/*', (route) => {
      const id = new URL(route.request().url()).pathname.split('/').pop() ?? '';
      if (tools.includes(id)) {
        const version = versionOf(id);
        return fulfillJson(
          route,
          mockEntity('Tool', id, `carry-${id.slice(-1)}`, {
            kind: 'Image',
            latest: version.id,
            latest_version: version.version,
            instances: [version],
            modified: '',
          }),
        );
      }
      const toolId = id.replace('inst-', '');
      return fulfillJson(
        route,
        mockEntity('ToolInstance', id, `carry@1.0.0`, {
          tool: toolId,
          version: { SemVer: '1.0.0' },
          previous: null,
          spec_hash: 'h',
          deprecated: false,
          spec: { Image: { marker: id } },
        }),
      );
    });
    await page.goto(`/tools/${tools[0]}`);
    await expect(page.getByTestId('publish-spec')).toHaveValue(new RegExp(`inst-${tools[0]}`));
    await page.getByTestId('publish-version').fill('uiloop-label-from-first');
    // move to the other tool without a reload, as following a link inside the app does
    await page.evaluate((path) => {
      window.history.pushState({}, '', path);
      window.dispatchEvent(new PopStateEvent('popstate'));
    }, `/tools/${tools[1]}`);
    await expect(page.getByTestId('publish-spec')).toHaveValue(new RegExp(`inst-${tools[1]}`));
    await expect(page.getByTestId('publish-spec')).not.toHaveValue(new RegExp(`inst-${tools[0]}`));
    await expect(page.getByTestId('publish-version')).toHaveValue('');
  });
});
