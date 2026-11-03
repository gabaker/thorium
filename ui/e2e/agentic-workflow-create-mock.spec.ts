import { test } from '@playwright/test';

import { expect, fulfillJson, MOCK_GROUP, mockEntity, mockEntityList } from './agentic-mocks';
import { setupMockAuth } from './helpers';

const AGENT_A = mockEntity('Agent', '00000000-0000-0000-0000-00000000ee0a', 'agent-a', {});
const AGENT_B = mockEntity('Agent', '00000000-0000-0000-0000-00000000ee0b', 'agent-b', {});

test.describe('workflow create form', () => {
  test('unchecking the entry agent clears the entry agent', async ({ page }) => {
    await setupMockAuth(page);
    await mockEntityList(page, [AGENT_A, AGENT_B]);
    // record the create form's fields
    let sent: string | null = null;
    await page.route('**/api/entities/', (route) => {
      sent = route.request().postData() ?? '';
      return fulfillJson(route, { id: '00000000-0000-0000-0000-00000000ee99' });
    });
    await page.goto('/workflows/create');
    await page.getByTestId('workflow-name').fill('stale-entry-demo');
    await page.getByTestId(`groups-${MOCK_GROUP}`).check();
    await page.getByTestId('workflow-agents-agent-a').check();
    await page.getByTestId('workflow-agents-agent-b').check();
    await page.getByTestId('workflow-entry').selectOption(AGENT_A.id);
    await page.getByTestId('workflow-agents-agent-a').uncheck();
    await expect(page.getByTestId('workflow-entry')).toHaveValue('');
    // checking the agent again leaves the entry unset
    await page.getByTestId('workflow-agents-agent-a').check();
    await page.getByTestId('workflow-agents-agent-a').uncheck();
    await page.getByTestId('submit-workflow').click();
    await expect.poll(() => sent).not.toBeNull();
    expect(sent).toContain(AGENT_B.id);
    expect(sent).not.toContain('workflow_entry_agent');
  });

  for (const [path, check, submit] of [
    ['/toolsets/create', 'auto-instance', 'submit-toolset'],
    ['/workflows/create', 'workflow-auto', 'submit-workflow'],
  ]) {
    test(`${path} keeps the create button clear of the checkbox above it`, async ({ page }) => {
      await setupMockAuth(page);
      await mockEntityList(page, [AGENT_A, AGENT_B]);
      await page.goto(path);
      const row = page.getByTestId(check).locator('xpath=ancestor::label');
      await expect(row).toBeVisible();
      const rowBox = await row.boundingBox();
      const buttonBox = await page.getByTestId(submit).boundingBox();
      expect(rowBox && buttonBox).toBeTruthy();
      // the button does not touch the last checkbox row
      expect(buttonBox!.y - (rowBox!.y + rowBox!.height)).toBeGreaterThanOrEqual(8);
    });
  }
});
