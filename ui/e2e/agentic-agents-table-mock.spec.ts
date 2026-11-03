import { test } from '@playwright/test';

import { expect, mockEntity, mockEntityList } from './agentic-mocks';
import { setupMockAuth } from './helpers';

/**
 * Build an agent with the given name, harness, and model ids.
 *
 * @param id - The agent's id.
 * @param name - The agent's name.
 * @param harness - The name of the agent's harness.
 * @param models - The ids of the agent's models in priority order.
 * @returns The agent entity.
 */
function agent(id: string, name: string, harness: string, models: string[]) {
  return mockEntity('Agent', id, name, {
    config: { models: models.map((model_id) => ({ model_id })) },
    harness: { name: harness },
    latest_tools: [{ name: 'a' }, { name: 'b' }],
    pinned_tools: [],
    tool_sets: [{ name: 'ts' }],
    sub_agents: [{ name: 'sub' }],
  });
}

/** The agents with the longest names and harnesses and two models each, so every column is as wide as it gets. */
const ENTITIES = [
  agent('00000000-0000-0000-0000-00000000fa01', 'jup-stub-agent-orphan-warn', 'jup-stub-harness-warn', ['stub-model-a', 'stub-model-b']),
  agent('00000000-0000-0000-0000-00000000fa02', 'demo-agent', 'jup-stub-harness@1.0.0', ['stub-model-a', 'stub-model-b']),
];

test.describe('the agent table at 1024', () => {
  test.beforeEach(async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 800 });
    await setupMockAuth(page);
    await mockEntityList(page, ENTITIES);
    await page.goto('/agents');
    await expect(page.getByTestId('agent-row-demo-agent')).toBeVisible();
  });

  test('fits its card so the used by line is never cut off', async ({ page }) => {
    // the scroll box around the table has no hidden sideways content once the rows have settled
    const table = page.getByTestId('agent-table');
    await expect
      .poll(() => table.evaluate((node) => (node.parentElement?.scrollWidth ?? 0) - (node.parentElement?.clientWidth ?? 0)))
      .toBeLessThanOrEqual(0);
    // the whole used by line sits inside the card, right edge included
    const cell = await page.getByTestId('agent-used-by-demo-agent').boundingBox();
    const card = await table.evaluate((node) => node.parentElement?.getBoundingClientRect().right ?? 0);
    expect((cell?.x ?? 0) + (cell?.width ?? 0)).toBeLessThanOrEqual(card);
  });

  test('never leaves the model separator alone on a line', async ({ page }) => {
    // each model id sits on a line of its own with the separator trailing the first, so no line holds only the separator
    // the outer span of each model holds its id and the separator
    const spans = page.getByTestId('agent-row-demo-agent').locator('td').nth(3).locator(':scope > span');
    await expect(spans).toHaveText(['stub-model-a\u00a0>', 'stub-model-b']);
    const first = await spans.nth(0).boundingBox();
    const second = await spans.nth(1).boundingBox();
    // the first model's whole span keeps one line height, the separator never wrapping beneath it
    expect(first?.height ?? 999).toBeLessThan(30);
    expect(second?.height ?? 999).toBeLessThan(30);
  });
});
