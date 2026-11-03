import { test } from '@playwright/test';

import { expect, fulfillJson, mockEntity } from './agentic-mocks';
import { setupMockAuth } from './helpers';

const INSTANCE_ID = '00000000-0000-0000-0000-00000000fd01';
const REASON =
  'entry agent `an-extremely-long-agent-name-with-no-harness-at-all` has no harness to run in and so the build could not finish, and this reason keeps going';
const HASH = 'abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789';

/** An invalid workflow instance with one agent and one tool. */
const INSTANCE = mockEntity('WorkflowInstance', INSTANCE_ID, 'wf-long#r5', {
  workflow: '00000000-0000-0000-0000-00000000fd02',
  workflow_groups: [],
  revision: 5,
  previous: null,
  status: { Invalid: { errors: [REASON] } },
  warnings: [],
  trigger: 'Manual',
  triggered_by: 'tester',
  entry_agent: null,
  limits: {},
  default_models: [],
  agents: [
    {
      agent: '00000000-0000-0000-0000-00000000fd10',
      name: 'lead',
      groups: [],
      config: {},
      config_hash: HASH,
      harness: null,
      tools: ['00000000-0000-0000-0000-00000000fd20'],
      tool_sets: [],
      bindings: [],
      sub_agents: [],
      toolset_digest: 'v1:abc',
    },
  ],
  tools: [
    {
      tool: '00000000-0000-0000-0000-00000000fd30',
      tool_instance: '00000000-0000-0000-0000-00000000fd20',
      name: 'scanner',
      groups: [],
      kind: 'Function',
      version: '1.0.0',
      spec_hash: HASH,
      function_name: 'scan',
      pinned: false,
      dependency: false,
    },
  ],
  digest: 'v1:abc',
});

test.describe('workflow instance page', () => {
  test('a long invalid reason wraps and the page does not scroll sideways at 1024', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 800 });
    await setupMockAuth(page);
    await page.route('**/api/entities/*', (route) => fulfillJson(route, INSTANCE));
    await page.goto(`/workflows/instances/${INSTANCE_ID}`);
    // the status is a short badge with the reason beneath it
    await expect(page.getByTestId('instance-info').getByText('Invalid', { exact: true })).toBeVisible();
    await expect(page.getByTestId('instance-status-reason')).toContainText('has no harness');
    // the hashes ride under the names rather than taking columns of their own
    await expect(page.getByTestId('instance-agents').getByRole('columnheader', { name: 'Config hash' })).toHaveCount(0);
    await expect(page.getByTestId('instance-tools').getByRole('columnheader', { name: 'Spec hash' })).toHaveCount(0);
    await expect(page.getByTestId('instance-agents')).toContainText('config abcdef01');
    await expect(page.getByTestId('instance-tools')).toContainText('spec abcdef01');
    await expect(page.getByTestId('toolset-lead')).toBeVisible();
    // neither the document nor the tables overflow the viewport
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
    expect(overflow).toBeLessThanOrEqual(0);
    const agents = await page.getByTestId('instance-agents').evaluate((table) => ({
      table: table.getBoundingClientRect().width,
      box: (table.parentElement as HTMLElement).getBoundingClientRect().width,
    }));
    expect(agents.table).toBeLessThanOrEqual(agents.box + 1);
  });

  test('hyphenated names in the agents table stay whole at 1024', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 800 });
    await setupMockAuth(page);
    const meta = INSTANCE.metadata.WorkflowInstance as { agents: { name: string }[]; tools: { name: string }[] };
    const hyphenated = mockEntity('WorkflowInstance', INSTANCE_ID, 'wf-long#r5', {
      ...meta,
      agents: [{ ...meta.agents[0], name: 'a-long-hyphenated-agent-name' }],
      tools: [{ ...meta.tools[0], name: 'a-long-hyphenated-tool-name' }],
    });
    await page.route('**/api/entities/*', (route) => fulfillJson(route, hyphenated));
    await page.goto(`/workflows/instances/${INSTANCE_ID}`);
    // each identifier is a single line box, so none of them is split at a hyphen
    await expect(page.getByTestId('instance-agents').getByText('a-long-hyphenated-agent-name', { exact: true })).toBeVisible();
    await expect(page.getByTestId('instance-agents').getByText('a-long-hyphenated-tool-name', { exact: true })).toBeVisible();
    for (const text of ['a-long-hyphenated-agent-name', 'a-long-hyphenated-tool-name']) {
      const box = await page.getByTestId('instance-agents').getByText(text, { exact: true }).boundingBox();
      // one line of 0.9rem text is far shorter than two
      expect(box?.height ?? 999).toBeLessThan(30);
    }
  });

  test('an agent with a harness, a sub-agent, and bindings fits its table at 1024 with whole names', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 800 });
    await setupMockAuth(page);
    const meta = INSTANCE.metadata.WorkflowInstance as { agents: Record<string, unknown>[]; tools: Record<string, unknown>[] };
    const toolInstance = (suffix: string, name: string) => ({
      ...meta.tools[0],
      tool: `00000000-0000-0000-0000-00000000fd3${suffix}`,
      tool_instance: `00000000-0000-0000-0000-00000000fd2${suffix}`,
      name: `${name}@1.0.0`,
    });
    const full = mockEntity('WorkflowInstance', INSTANCE_ID, 'wf-long#r5', {
      ...meta,
      agents: [
        {
          ...meta.agents[0],
          name: 'uiloop-long-hyphenated-agent',
          harness: '00000000-0000-0000-0000-00000000fd21',
          tools: ['00000000-0000-0000-0000-00000000fd22'],
          sub_agents: ['00000000-0000-0000-0000-00000000fd11'],
          bindings: [{ tool_instance: '00000000-0000-0000-0000-00000000fd22', mechanism: 'Cli' }],
        },
        { ...meta.agents[0], agent: '00000000-0000-0000-0000-00000000fd11', name: 'demo-subagent', harness: null, tools: [] },
      ],
      tools: [toolInstance('1', 'jup-stub-harness'), toolInstance('2', 'jup-stub-call')],
    });
    await page.route('**/api/entities/*', (route) =>
      route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(full) }),
    );
    await page.goto(`/workflows/instances/${INSTANCE_ID}`);
    const table = page.getByTestId('instance-agents');
    // the harness shows under the agent name rather than in a column of its own
    await expect(table.getByRole('columnheader', { name: 'Harness' })).toHaveCount(0);
    await expect(page.getByTestId('agent-harness-uiloop-long-hyphenated-agent')).toContainText('jup-stub-harness@1.0.0');
    await expect(page.getByTestId('agent-harness-demo-subagent')).toContainText('none');
    // the table fits its box, and no identifier is split at a hyphen
    const fit = await table.evaluate((el) => ({ table: el.scrollWidth, box: (el.parentElement as HTMLElement).clientWidth }));
    expect(fit.table).toBeLessThanOrEqual(fit.box + 1);
    for (const text of ['uiloop-long-hyphenated-agent', 'jup-stub-call@1.0.0']) {
      const box = await table.getByText(text, { exact: true }).first().boundingBox();
      expect(box?.height ?? 999).toBeLessThan(30);
    }
    // a binding reads "tool: mechanism"
    await expect(table.getByRole('cell').filter({ hasText: 'jup-stub-call@1.0.0: Cli' })).toHaveCount(1);
  });

  test('short hyphenated names stay whole when busy sibling lists crowd the agents table', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 800 });
    await setupMockAuth(page);
    const meta = INSTANCE.metadata.WorkflowInstance as { agents: Record<string, unknown>[]; tools: Record<string, unknown>[] };
    const ids = Array.from({ length: 11 }, (_, index) => `00000000-0000-0000-0000-00000000fe${String(index).padStart(2, '0')}`);
    const busy = mockEntity('WorkflowInstance', INSTANCE_ID, 'wf-long#r5', {
      ...meta,
      agents: [
        {
          ...meta.agents[0],
          name: 'uiloop-lead-agent',
          tools: ids,
          bindings: ids.map((tool_instance) => ({ tool_instance, mechanism: 'Cli' })),
          sub_agents: ['00000000-0000-0000-0000-00000000fd11'],
        },
        { ...meta.agents[0], agent: '00000000-0000-0000-0000-00000000fd11', name: 'demo-subagent', harness: null, tools: [] },
      ],
      tools: ids.map((tool_instance, index) => ({ ...meta.tools[0], tool_instance, name: `uiloop-busy-tool-${index}@1.0.0` })),
    });
    await page.route('**/api/entities/*', (route) => fulfillJson(route, busy));
    await page.goto(`/workflows/instances/${INSTANCE_ID}`);
    const table = page.getByTestId('instance-agents');
    await expect(table).toContainText('uiloop-busy-tool-10@1.0.0: Cli');
    // every short name is one line box, so none is split at a hyphen
    for (const text of ['demo-subagent', 'uiloop-lead-agent']) {
      const box = await table.getByText(text, { exact: true }).first().boundingBox();
      expect(box?.height ?? 999).toBeLessThan(30);
    }
    // a binding's ": mechanism" never starts a line by itself
    const binding = table.getByText('uiloop-busy-tool-0@1.0.0:', { exact: true }).first();
    const mechanism = binding.locator('xpath=following-sibling::span[1]');
    const [named, detail] = await Promise.all([binding.boundingBox(), mechanism.boundingBox()]);
    expect(Math.abs((named?.y ?? 0) - (detail?.y ?? 999)) < 30 || (detail?.y ?? 0) > (named?.y ?? 0)).toBe(true);
    // the separating comma rides with the mechanism
    await expect(mechanism).toHaveText('Cli,');
  });

  test('names of 28 characters fit the agents table box at 1024, with each comma on its name line', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 800 });
    await setupMockAuth(page);
    const meta = INSTANCE.metadata.WorkflowInstance as { agents: Record<string, unknown>[]; tools: Record<string, unknown>[] };
    const names = ['yara-signature-scanner@1.0.0', 'static-analysis-harness@2.11', 'strings-extractor-tool@1.0.0'];
    const ids = Array.from({ length: 6 }, (_, index) => `00000000-0000-0000-0000-00000000ff${String(index).padStart(2, '0')}`);
    const wide = mockEntity('WorkflowInstance', INSTANCE_ID, 'wf-long#r5', {
      ...meta,
      agents: [
        {
          ...meta.agents[0],
          name: 'uiloop-triage-lead',
          harness: ids[1],
          tools: ids,
          bindings: ids.map((tool_instance) => ({ tool_instance, mechanism: 'Cli' })),
          sub_agents: ['00000000-0000-0000-0000-00000000fd11'],
        },
        { ...meta.agents[0], agent: '00000000-0000-0000-0000-00000000fd11', name: 'demo-subagent', harness: null, tools: [] },
      ],
      tools: ids.map((tool_instance, index) => ({ ...meta.tools[0], tool_instance, name: names[index % names.length] })),
    });
    await page.route('**/api/entities/*', (route) => fulfillJson(route, wide));
    await page.goto(`/workflows/instances/${INSTANCE_ID}`);
    const table = page.getByTestId('instance-agents');
    await expect(table).toContainText('yara-signature-scanner@1.0.0: Cli');
    // no column is cut off at the box edge
    const fit = await table.evaluate((el) => {
      const box = el.parentElement as HTMLElement;
      return { scroll: box.scrollWidth, client: box.clientWidth };
    });
    expect(fit.scroll).toBeLessThanOrEqual(fit.client + 1);
    // a separating comma is part of the name before it, so it cannot sit on a line of its own
    const commas = await table.evaluate(
      (el) => [...el.querySelectorAll('tbody td span')].filter((span) => span.children.length === 0 && span.textContent === ',').length,
    );
    expect(commas).toBe(0);
    // in a wide window every name is whole again, and narrowing the window again still fits the box
    const nameHeight = async () =>
      (await table.getByText('yara-signature-scanner@1.0.0,', { exact: true }).first().boundingBox())?.height ?? 999;
    await page.setViewportSize({ width: 1600, height: 800 });
    await expect.poll(nameHeight).toBeLessThan(30);
    await page.setViewportSize({ width: 1024, height: 800 });
    await expect
      .poll(() => table.evaluate((el) => (el.parentElement as HTMLElement).scrollWidth - (el.parentElement as HTMLElement).clientWidth))
      .toBeLessThanOrEqual(1);
  });

  test('long agent, tool and binding names wrap inside the agents table at 1024 and the Bindings column stays in the box', async ({
    page,
  }) => {
    await page.setViewportSize({ width: 1024, height: 800 });
    await setupMockAuth(page);
    const meta = INSTANCE.metadata.WorkflowInstance as {
      agents: Record<string, unknown>[];
      tools: Record<string, unknown>[];
    };
    const toolIds = ['00000000-0000-0000-0000-00000000fd21', '00000000-0000-0000-0000-00000000fd22'];
    const toolNames = ['jup-stub-call-extremely-long-tool-name', 'another-very-long-hyphenated-tool-name-for-binding'];
    const long = mockEntity('WorkflowInstance', INSTANCE_ID, 'wf-long#r5', {
      ...meta,
      agents: [
        {
          ...meta.agents[0],
          name: 'a-remarkably-long-hyphenated-lead-agent-name',
          harness: toolIds[0],
          tools: toolIds,
          bindings: toolIds.map((tool_instance) => ({ tool_instance, mechanism: 'CliTool' })),
        },
      ],
      tools: toolIds.map((tool_instance, index) => ({
        ...meta.tools[0],
        tool_instance,
        name: toolNames[index],
      })),
    });
    await page.route('**/api/entities/*', (route) => fulfillJson(route, long));
    await page.goto(`/workflows/instances/${INSTANCE_ID}`);
    const table = page.getByTestId('instance-agents');
    await expect(table).toContainText('CliTool');
    // the table fits its scroll box, so no column is cut off
    const sizes = await table.evaluate((el) => ({
      table: el.getBoundingClientRect().width,
      box: (el.parentElement as HTMLElement).getBoundingClientRect().width,
      scroll: (el.parentElement as HTMLElement).scrollWidth,
      client: (el.parentElement as HTMLElement).clientWidth,
    }));
    expect(sizes.table).toBeLessThanOrEqual(sizes.box + 1);
    expect(sizes.scroll).toBeLessThanOrEqual(sizes.client + 1);
    // the last column's right edge is inside the box
    const lastHeader = await table.getByRole('columnheader', { name: 'Bindings' }).boundingBox();
    const box = await table.locator('xpath=..').boundingBox();
    expect((lastHeader?.x ?? 0) + (lastHeader?.width ?? 0)).toBeLessThanOrEqual((box?.x ?? 0) + (box?.width ?? 0) + 1);
  });

  test('an instance whose workflow is gone says so instead of linking to a not-found page', async ({ page }) => {
    await setupMockAuth(page);
    await page.route('**/api/entities/*', (route) => {
      if (route.request().url().endsWith(INSTANCE_ID)) return fulfillJson(route, INSTANCE);
      return fulfillJson(route, { error: 'entity not found' }, 404);
    });
    await page.goto(`/workflows/instances/${INSTANCE_ID}`);
    await expect(page.getByTestId('instance-workflow-gone')).toContainText('workflow was deleted');
    await expect(page.getByRole('link', { name: 'Back to workflow' })).toHaveCount(0);
  });

  test('an instance whose workflow exists keeps the back link', async ({ page }) => {
    await setupMockAuth(page);
    const workflow = mockEntity('WorkflowCatalog', '00000000-0000-0000-0000-00000000fd02', 'wf-long', {});
    await page.route('**/api/entities/*', (route) => fulfillJson(route, route.request().url().endsWith(INSTANCE_ID) ? INSTANCE : workflow));
    await page.goto(`/workflows/instances/${INSTANCE_ID}`);
    await expect(page.getByRole('link', { name: 'Back to workflow' })).toBeVisible();
    await expect(page.getByTestId('instance-workflow-gone')).toHaveCount(0);
  });
});
