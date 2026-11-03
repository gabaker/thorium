import { test } from '@playwright/test';

import { expect, fulfillJson, mockEntity } from './agentic-mocks';
import { setupMockAuth } from './helpers';

const AGENT_ID = '00000000-0000-0000-0000-00000000fe01';

/** An agent with both a system prompt and a task prompt. */
const AGENT = mockEntity('Agent', AGENT_ID, 'prompted', {
  config: { models: [], system_prompt: 'You are a test agent', prompt: 'Summarize the input' },
});

test.describe('agent details page', () => {
  test('the task prompt heading is spaced from the system prompt box above it', async ({ page }) => {
    await setupMockAuth(page);
    await page.route('**/api/entities/details/**', (route) => fulfillJson(route, { data: [], cursor: null }));
    await page.route('**/api/entities/*', (route) => fulfillJson(route, AGENT));
    await page.goto(`/agents/${AGENT_ID}`);
    const system = page.getByTestId('agent-system-prompt');
    await expect(system).toBeVisible();
    const heading = page.getByRole('heading', { name: 'Task prompt' });
    const above = await system.boundingBox();
    const below = await heading.boundingBox();
    // the heading does not touch the bottom border of the box above it
    expect((below?.y ?? 0) - ((above?.y ?? 0) + (above?.height ?? 0))).toBeGreaterThanOrEqual(8);
  });
});
