import { test } from '@playwright/test';

import { expect, mockEntity, mockEntityList, mockInstanceSummary } from './agentic-mocks';
import { setupMockAuth } from './helpers';

const AGENT_ID = '00000000-0000-0000-0000-00000000ee01';
const TOOLSET_ID_A = '00000000-0000-0000-0000-00000000ee02';
const TOOLSET_ID_B = '00000000-0000-0000-0000-00000000ee03';
const TOOL_ID_A = '00000000-0000-0000-0000-00000000ee04';
const TOOL_ID_B = '00000000-0000-0000-0000-00000000ee05';

/**
 * Build a tool with a long hyphenated name, a description that fills its two lines, and a deprecated version, so its row crowds the
 * tool list the way the busiest rows do.
 *
 * @param id - The tool's id.
 * @param name - The tool's name.
 * @returns The tool entity.
 */
function crowdedTool(id: string, name: string) {
  const live = mockInstanceSummary(`${id}-2`, '2.0.0');
  const tool = mockEntity('Tool', id, name, {
    kind: 'Image',
    latest: live.id,
    latest_version: live.version,
    latest_spec_hash: live.spec_hash,
    modified: '2026-10-01T08:45:12Z',
    instances: [live, mockInstanceSummary(`${id}-1`, '1.0.0', true)],
  });
  return {
    ...tool,
    description: 'A tool whose description is long enough to need both of the lines a list row gives it, and then some more.',
  };
}

/** The entities the lists show: a hyphenated agent, two toolsets that share a name (so a short id follows it), and two crowded tools. */
const ENTITIES = [
  mockEntity('Agent', AGENT_ID, 'demo-subagent', {
    config: {},
    harness: null,
    latest_tools: [],
    pinned_tools: [],
    tool_sets: [],
    sub_agents: [],
  }),
  ...[TOOLSET_ID_A, TOOLSET_ID_B].map((id) =>
    mockEntity('ToolSet', id, 'demo-toolset', {
      read_only: false,
      selector: null,
      auto_instance: true,
      highest_revision: 0,
      latest_instance: null,
    }),
  ),
  crowdedTool(TOOL_ID_A, 'uiloop-v2a2-gan1024-v1'),
  crowdedTool(TOOL_ID_B, 'auto-volatility3-worker'),
];

test.describe('hyphenated names in the list tables', () => {
  for (const [path, testId, name] of [
    ['/agents', 'agent-row-demo-subagent', 'demo-subagent'],
    ['/toolsets', 'toolset-row-demo-toolset', 'demo-toolset'],
    ['/tools', 'tool-row-uiloop-v2a2-gan1024-v1', 'uiloop-v2a2-gan1024-v1'],
    ['/tools', 'tool-row-auto-volatility3-worker', 'auto-volatility3-worker'],
  ]) {
    test(`${path} keeps ${name} on one line at 1024`, async ({ page }) => {
      await page.setViewportSize({ width: 1024, height: 800 });
      await setupMockAuth(page);
      await mockEntityList(page, ENTITIES);
      await page.goto(path);
      const link = page.getByTestId(testId).first().getByRole('link', { name, exact: true });
      await expect(link).toBeVisible();
      // the name can never break at a hyphen, however many other columns compete for the row's width
      const whiteSpace = await link.evaluate((node) => getComputedStyle(node.firstElementChild ?? node).whiteSpace);
      expect(whiteSpace).toBe('nowrap');
      // one line of 0.9rem text is far shorter than two, so the name was not split at its hyphen
      const box = await link.boundingBox();
      expect(box?.height ?? 999).toBeLessThan(30);
      // the page does not scroll sideways to make room for the whole names
      const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
      expect(overflow).toBeLessThanOrEqual(0);
      // the table fits its card at 1024 rather than scrolling sideways inside it
      const table = page.locator('table').first();
      const tableOverflow = await table.evaluate((node) => node.scrollWidth - (node.parentElement?.clientWidth ?? 0));
      expect(tableOverflow).toBeLessThanOrEqual(0);
    });
  }
});

/** A toolset name past the short-name limit, as long as the one that made the whole list scroll sideways. */
const LONG_TOOLSET_NAME = 'uiloop-a-really-long-toolset-name-to-force-the-scroll';
const LONG_TOOLSET_ID = '00000000-0000-0000-0000-00000000ee06';
const LONG_INSTANCE_ID = '00000000-0000-0000-0000-00000000ee07';

/** The toolsets list rows: a short-named toolset with its latest instance, and one with a very long name and latest instance. */
const TOOLSET_ENTITIES = [
  mockEntity('ToolSet', TOOLSET_ID_A, 'demo-toolset', {
    read_only: false,
    selector: null,
    auto_instance: true,
    highest_revision: 1,
    latest_instance: TOOLSET_ID_B,
  }),
  mockEntity('ToolSetInstance', TOOLSET_ID_B, 'demo-toolset#r1', { members: [], status: 'Ready', tool_set: TOOLSET_ID_A, revision: 1 }),
  mockEntity('ToolSet', LONG_TOOLSET_ID, LONG_TOOLSET_NAME, {
    read_only: true,
    selector: null,
    auto_instance: false,
    highest_revision: 1,
    latest_instance: LONG_INSTANCE_ID,
  }),
  mockEntity('ToolSetInstance', LONG_INSTANCE_ID, `${LONG_TOOLSET_NAME}#r1`, {
    members: [],
    status: 'Ready',
    tool_set: LONG_TOOLSET_ID,
    revision: 1,
  }),
];

test.describe('a very long toolset name in the toolsets list', () => {
  for (const width of [1024, 1280, 1440]) {
    test(`wraps instead of widening the table at ${width}`, async ({ page }) => {
      await page.setViewportSize({ width, height: 800 });
      await setupMockAuth(page);
      await mockEntityList(page, TOOLSET_ENTITIES);
      await page.goto('/toolsets');
      const row = page.getByTestId(`toolset-row-${LONG_TOOLSET_NAME}`);
      await expect(row).toBeVisible();
      // the table fits its card, so no column is clipped and no scrollbar is needed to reach the Latest instance column
      const table = page.getByTestId('toolset-table');
      const tableOverflow = await table.evaluate((node) => node.scrollWidth - (node.parentElement?.clientWidth ?? 0));
      expect(tableOverflow).toBeLessThanOrEqual(0);
      // the long name is allowed to wrap, so it takes more than one line wherever the table is too narrow to hold it on one
      const link = row.getByRole('link', { name: LONG_TOOLSET_NAME, exact: true });
      const box = await link.boundingBox();
      if (width < 1440) expect(box?.height ?? 0).toBeGreaterThan(30);
      // the long instance name is cut at the revision suffix or a hyphen, and the revision suffix stays whole
      const latest = page.getByTestId(`toolset-latest-${LONG_TOOLSET_NAME}`);
      await expect(latest).toContainText('#r1');
      const latestRight = await latest.evaluate((node) => node.getBoundingClientRect().right);
      const tableRight = await table.evaluate((node) => node.getBoundingClientRect().right);
      expect(latestRight).toBeLessThanOrEqual(tableRight);
    });
  }

  test('keeps the short toolset and its instance on one line at 1024', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 800 });
    await setupMockAuth(page);
    await mockEntityList(page, TOOLSET_ENTITIES);
    await page.goto('/toolsets');
    const latest = page.getByTestId('toolset-latest-demo-toolset');
    // one line of text is far shorter than two
    const box = await latest.getByText('demo-toolset').first().boundingBox();
    expect(box?.height ?? 999).toBeLessThan(30);
  });
});
