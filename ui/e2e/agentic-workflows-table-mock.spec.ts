import { test } from '@playwright/test';

import { expect, mockEntity, mockEntityList } from './agentic-mocks';
import { setupMockAuth } from './helpers';

const AGENT_ID = '00000000-0000-0000-0000-00000000fb01';
const WORKFLOW_ID = '00000000-0000-0000-0000-00000000fb02';
const INSTANCE_ID = '00000000-0000-0000-0000-00000000fb03';
const BARE_WORKFLOW_ID = '00000000-0000-0000-0000-00000000fb04';

/** A workflow, entry agent and latest instance whose names are as long as the busiest names on a live cluster. */
const WORKFLOW_NAME = 'uiloop-orphan-warn-workflow';
const AGENT_NAME = 'jup-stub-agent-orphan-warn';
const INSTANCE_NAME = `${WORKFLOW_NAME}#r12`;

/** The agent, a workflow with a Ready instance and long names, and a workflow with no Ready instance. */
const ENTITIES = [
  mockEntity('Agent', AGENT_ID, AGENT_NAME, {
    config: {},
    harness: null,
    latest_tools: [],
    pinned_tools: [],
    tool_sets: [],
    sub_agents: [],
  }),
  mockEntity('WorkflowCatalog', WORKFLOW_ID, WORKFLOW_NAME, {
    agents: [AGENT_ID],
    entry_agent: AGENT_ID,
    highest_revision: 12,
    latest_instance: INSTANCE_ID,
  }),
  mockEntity('WorkflowInstance', INSTANCE_ID, INSTANCE_NAME, {
    workflow: WORKFLOW_ID,
    workflow_groups: [],
    revision: 12,
    previous: null,
    status: 'Ready',
    warnings: [],
    trigger: 'Manual',
    triggered_by: 'tester',
    entry_agent: AGENT_ID,
    limits: {},
    default_models: [],
    agents: [],
    tools: [],
  }),
  mockEntity('WorkflowCatalog', BARE_WORKFLOW_ID, 'uiloop-unscheduled-workflow', {
    agents: [AGENT_ID],
    entry_agent: AGENT_ID,
    highest_revision: 1,
    latest_instance: null,
  }),
];

test.describe('the workflow table below 1240', () => {
  // the widths the table used to scroll sideways at, down to the narrowest desktop window
  for (const width of [1024, 1100, 1180, 1200]) {
    test(`fits its card with every column visible at ${width}`, async ({ page }) => {
      await page.setViewportSize({ width, height: 800 });
      await setupMockAuth(page);
      await mockEntityList(page, ENTITIES);
      await page.goto('/workflows');
      const row = page.getByTestId(`workflow-row-${WORKFLOW_NAME}`);
      await expect(row).toContainText(INSTANCE_NAME);
      // the scroll box around the table has no hidden sideways content once the rows have settled
      const table = page.getByTestId('workflow-table');
      await expect
        .poll(() => table.evaluate((node) => (node.parentElement?.scrollWidth ?? 0) - (node.parentElement?.clientWidth ?? 0)))
        .toBeLessThanOrEqual(0);
      // the last column's header and cell, and the status badge, end inside the card
      const card = await table.evaluate((node) => node.parentElement?.getBoundingClientRect().right ?? 0);
      for (const target of [table.getByRole('columnheader', { name: 'Revision' }), row.locator('td').last(), row.getByText('Ready')]) {
        await expect(target).toBeVisible();
        const box = await target.boundingBox();
        expect((box?.x ?? 0) + (box?.width ?? 0)).toBeLessThanOrEqual(card);
      }
    });
  }
});
