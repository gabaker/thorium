import { test, Locator, Page } from '@playwright/test';

import { expect, fulfillJson, mockEntity, mockEntityList } from './agentic-mocks';
import { setupMockAuth } from './helpers';

const CRED_ID = '00000000-0000-0000-0000-00000000a601';
const ENDPOINT_ID = '00000000-0000-0000-0000-00000000a602';
const BASE_URL = 'http://Jup-Mock.svc:8080/m/stub-model-a/v1';
const BAN = { id: 'ban-1', time_banned: '2026-10-02T00:00:00Z', ban_kind: { Generic: 'banned by hand for testing' } };
const CREDENTIAL = mockEntity('ModelCredential', CRED_ID, 'mock-key', {
  kind: 'ApiKey',
  secrets: { fingerprint: '98f4255836b1' },
  expires: null,
  rotated: '2026-10-28T10:29:24Z',
  bans: {},
  modified: '2026-10-28T10:29:24Z',
});
const ROLE = mockEntity('ModelCredential', '00000000-0000-0000-0000-00000000a603', 'mock-role', {
  kind: { AwsAssumeRole: { role_arn: 'arn:aws:iam::1:role/r', external_id: null, source: { AccessKey: CRED_ID } } },
  secrets: { fingerprint: null },
  expires: null,
  rotated: null,
  bans: {},
  modified: '2026-10-28T10:29:24Z',
});
const ENDPOINT = mockEntity('ModelEndpoint', ENDPOINT_ID, 'mock-ep', {
  provider: { OpenAiCompatible: { base_url: BASE_URL, extra_headers: {}, key_header: null } },
  credential: CRED_ID,
  catalog: [
    {
      model_id: 'stub-model-a',
      limits: { context_window: 128000, max_output_tokens: 8192 },
      capabilities: { tool_calling: true },
      bans: {},
    },
    { model_id: 'extra-model', limits: null, capabilities: null, bans: { 'ban-1': BAN } },
  ],
  bans: {},
  refresh: { state: 'Succeeded' },
  disabled_models: [],
  modified: '2026-10-28T10:29:24Z',
});
const PROFILE = mockEntity('ModelProfile', '00000000-0000-0000-0000-00000000a604', 'mock-prof', {
  endpoint: ENDPOINT_ID,
  model_id: 'stub-model-a',
  api: 'OpenAiChat',
  parameters: { temperature: 0.2, max_output_tokens: 512, stop_sequences: [] },
  limits: null,
  capabilities: null,
  bans: {},
  modified: '2026-10-28T10:29:24Z',
});

/**
 * Serve the given entities to the models page.
 *
 * @param page - The page whose requests are mocked.
 * @param entities - Every entity the list may return.
 */
async function mockModels(page: Page, entities: { kind: string }[]) {
  await setupMockAuth(page);
  await mockEntityList(page, entities);
}

/**
 * Read the lines an element's text wraps onto, in order.
 *
 * @param locator - The element whose text is read.
 * @returns The text of each rendered line.
 */
function textLines(locator: Locator): Promise<string[]> {
  return locator.evaluate((root) => {
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    const found: string[] = [];
    let current = '';
    let top: number | null = null;
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      const text = node.textContent ?? '';
      for (let index = 0; index < text.length; index += 1) {
        const range = document.createRange();
        range.setStart(node, index);
        range.setEnd(node, index + 1);
        const rect = range.getClientRects()[0];
        if (!rect) continue;
        // a character far enough below the last one starts a new line
        if (top !== null && Math.abs(rect.top - top) > 3) {
          found.push(current);
          current = '';
        }
        top = rect.top;
        current += text[index];
      }
    }
    found.push(current);
    return found;
  });
}

test.describe('models page (mocked)', () => {
  test('an endpoint row shows its provider, base URL, credential fingerprint, limits, and catalog model bans', async ({ page }) => {
    await mockModels(page, [ENDPOINT, CREDENTIAL, ROLE, PROFILE]);
    await page.goto('/models');
    const row = page.getByTestId('endpoint-row-mock-ep');
    await expect(row.getByTestId('endpoint-provider')).toContainText('OpenAiCompatible');
    // the base URL keeps its case
    await expect(row.getByTestId('endpoint-provider')).toContainText(BASE_URL);
    await expect(row.getByTestId('endpoint-credential')).toContainText('mock-key');
    await expect(row.getByTestId('endpoint-credential')).toContainText('fp 98f4255836b1');
    // an endpoint's catalog lists each limit on a line of its own
    await expect(row.getByTestId('endpoint-models')).toContainText('context 128000');
    await expect(row.getByTestId('endpoint-models')).toContainText('output 8192');
    await expect(row.getByTestId('model-banned')).toHaveAttribute('title', /banned by hand for testing/);
    await expect(row.getByTestId('model-ban-badge')).toContainText('extra-model: Generic');
    await expect(row.getByTestId('model-ban-badge')).toHaveAttribute('title', /banned by hand for testing/);
  });

  test('a long banned catalog model id wraps inside the page instead of widening it', async ({ page }) => {
    const longId = 'uiloop-extra-model-with-a-very-long-identifier-v2-2026-10-28';
    const longEndpoint = mockEntity('ModelEndpoint', '00000000-0000-0000-0000-00000000a605', 'long-ep', {
      ...ENDPOINT.metadata.ModelEndpoint,
      catalog: [{ model_id: longId, limits: null, capabilities: null, bans: { 'ban-1': BAN } }],
    });
    await mockModels(page, [longEndpoint, CREDENTIAL]);
    await page.setViewportSize({ width: 1024, height: 768 });
    await page.goto('/models');
    const badge = page.getByTestId('endpoint-row-long-ep').getByTestId('model-ban-badge');
    await expect(badge).toContainText(longId);
    // neither the document nor the table runs past the viewport
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
    expect(overflow).toBeLessThanOrEqual(0);
  });

  test('a profile row shows its endpoint, parameters, and inherited limits', async ({ page }) => {
    await mockModels(page, [ENDPOINT, CREDENTIAL, PROFILE]);
    await page.goto('/models');
    await page.getByRole('tab', { name: /Profiles/ }).click();
    const row = page.getByTestId('profile-row-mock-prof');
    await expect(row.getByTestId('profile-endpoint')).toHaveText('mock-ep');
    await expect(row.getByTestId('profile-parameters')).toHaveText('temperature 0.2, max_output_tokens 512');
    // the limits are stacked on lines of their own, matching the Endpoints Models column
    await expect(row.getByTestId('profile-limits')).toContainText('context 128000');
    await expect(row.getByTestId('profile-limits')).toContainText('output 8192');
    await expect(row.getByTestId('profile-limits')).toContainText('tool_calling');
  });

  test('profile names without separators keep the table inside its panel at 1024 pixels wide', async ({ page }) => {
    const named = (name: string) =>
      mockEntity('ModelProfile', `00000000-0000-0000-0000-00000000b${name.length}`, name, {
        ...PROFILE.metadata.ModelProfile,
        parameters: { temperature: 0.2, max_output_tokens: 1024 },
      });
    await mockModels(page, [ENDPOINT, CREDENTIAL, PROFILE, named('productionclaude'), named('uiloopnoseparato')]);
    await page.setViewportSize({ width: 1024, height: 768 });
    await page.goto('/models');
    await page.getByRole('tab', { name: /Profiles/ }).click();
    await expect(page.getByTestId('profile-row-productionclaude')).toBeVisible();
    // every column, the last included, stays inside the scroller
    const spill = await page
      .getByTestId('profile-table')
      .evaluate((el) => (el.parentElement?.scrollWidth ?? 0) - (el.parentElement?.clientWidth ?? 0));
    expect(spill).toBeLessThanOrEqual(0);
  });

  test('a name made of ordinary words wraps only after a separator at 1024 pixels wide', async ({ page }) => {
    const names = ['observability-endpoint', 'uiloop-production-claude-sonnet-high-effort'];
    const profiles = names.map((name, index) =>
      mockEntity('ModelProfile', `00000000-0000-0000-0000-00000000c${index}0`, name, {
        ...PROFILE.metadata.ModelProfile,
        parameters: { temperature: 0.2, max_output_tokens: 1024 },
      }),
    );
    const endpoints = names.map((name, index) =>
      mockEntity('ModelEndpoint', `00000000-0000-0000-0000-00000000c${index}1`, name, ENDPOINT.metadata.ModelEndpoint),
    );
    await mockModels(page, [CREDENTIAL, ...endpoints, ...profiles]);
    await page.setViewportSize({ width: 1024, height: 768 });
    await page.goto('/models');
    for (const [tab, rowPrefix] of [
      ['Endpoints', 'endpoint-row-'],
      ['Profiles', 'profile-row-'],
    ]) {
      await page.getByRole('tab', { name: new RegExp(tab) }).click();
      for (const name of names) {
        const lines = (await textLines(page.getByTestId(`${rowPrefix}${name}`).locator('td:first-child'))).map((line) => line.trim());
        // the name wraps only after a separator, so no word is split and no line starts with one
        expect(lines.join('')).toBe(name);
        expect(lines.every((line) => /^[a-z0-9]/.test(line))).toBe(true);
        expect(lines.slice(0, -1).every((line) => /-$/.test(line))).toBe(true);
      }
    }
  });

  test('a one-word name of an ordinary length stays on one line at 1024 pixels wide', async ({ page }) => {
    // a word of fourteen characters or fewer stays whole at every width; a longer one may break on a narrow page
    const names = ['summarization', 'observability', 'authentication'];
    const profiles = names.map((name, index) =>
      mockEntity('ModelProfile', `00000000-0000-0000-0000-00000000d${index}0`, name, {
        ...PROFILE.metadata.ModelProfile,
        parameters: { temperature: 0.2, max_output_tokens: 1024 },
      }),
    );
    const endpoints = names.map((name, index) =>
      mockEntity('ModelEndpoint', `00000000-0000-0000-0000-00000000d${index}1`, name, ENDPOINT.metadata.ModelEndpoint),
    );
    await mockModels(page, [CREDENTIAL, ...endpoints, ...profiles]);
    await page.setViewportSize({ width: 1024, height: 768 });
    await page.goto('/models');
    for (const [tab, rowPrefix] of [
      ['Endpoints', 'endpoint-row-'],
      ['Profiles', 'profile-row-'],
    ]) {
      await page.getByRole('tab', { name: new RegExp(tab) }).click();
      for (const name of names) {
        const lines = (await textLines(page.getByTestId(`${rowPrefix}${name}`).locator('td:first-child'))).map((line) => line.trim());
        // the word fits its column, so it is not split mid-word
        expect(lines).toEqual([name]);
      }
    }
  });

  test('a ban banner spans the table in its own row and shows model ids as code', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 800 });
    const banned = mockEntity('ModelEndpoint', ENDPOINT_ID, 'mock-ep', {
      ...ENDPOINT.metadata.ModelEndpoint,
      bans: { 'ban-1': BAN },
    });
    await mockModels(page, [banned, CREDENTIAL, PROFILE]);
    await page.route('**/api/entities/*/notifications', (route) =>
      fulfillJson(route, [
        {
          key: ENDPOINT_ID,
          created: '2026-10-02T00:00:00Z',
          id: 'note-1',
          msg: 'model `extra-model` is banned: Generic: banned by hand for testing',
          level: 'Error',
          ban_id: 'ban-1',
        },
      ]),
    );
    await page.goto('/models');
    const notes = page.getByTestId('endpoint-notes-mock-ep');
    await expect(notes.getByTestId('model-notification')).toContainText('model extra-model is banned');
    await expect(notes.locator('code')).toHaveText('extra-model');
    // the banner is wide and short instead of a one word column
    const box = await notes.boundingBox();
    expect(box?.height).toBeLessThan(100);
    expect(box?.width).toBeGreaterThan(600);
    // the name cell holds only the name
    await expect(page.getByTestId('endpoint-row-mock-ep').locator('td').first()).toHaveText('mock-ep');
  });

  test('a profile keeps its capability names whole and wraps its model id only at separators at a narrow width', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 800 });
    await mockModels(page, [ENDPOINT, CREDENTIAL, PROFILE]);
    await page.goto('/models');
    await page.getByRole('tab', { name: /Profiles/ }).click();
    const row = page.getByTestId('profile-row-mock-prof');
    // a capability name is one line of small text; a mid-word break doubles its height
    const capability = await row.getByTestId('profile-limits').getByText('tool_calling').boundingBox();
    expect(capability?.height ?? 0).toBeLessThan(25);
    // the model id may wrap after a separator but never inside a word
    await expect(row.getByTestId('profile-model').getByTestId('id-text')).toHaveCSS('overflow-wrap', 'normal');
  });

  test('the endpoint and profile tables keep their Bans column visible at 1024 with sixteen character names that never strand a character', async ({
    page,
  }) => {
    await page.setViewportSize({ width: 1024, height: 900 });
    const longCredential = mockEntity('ModelCredential', CRED_ID, 'jup-mock-model-k', { ...CREDENTIAL.metadata.ModelCredential });
    const longEndpoint = mockEntity('ModelEndpoint', '00000000-0000-0000-0000-00000000a606', 'jup-mock-model-b', {
      ...ENDPOINT.metadata.ModelEndpoint,
      // one catalog model, so the table is as wide as its names make it rather than as its banned model's id does
      catalog: ENDPOINT.metadata.ModelEndpoint.catalog.slice(0, 1),
    });
    const banned = mockEntity('ModelProfile', '00000000-0000-0000-0000-00000000a607', 'uiloop-profile-banned', {
      ...PROFILE.metadata.ModelProfile,
      endpoint: '00000000-0000-0000-0000-00000000a606',
      bans: { 'ban-1': BAN },
    });
    const plain = mockEntity('ModelProfile', '00000000-0000-0000-0000-00000000a608', 'uiloop-profile-b', {
      ...PROFILE.metadata.ModelProfile,
      endpoint: '00000000-0000-0000-0000-00000000a606',
    });
    await mockModels(page, [longEndpoint, longCredential, banned, plain]);
    await page.goto('/models');
    // the endpoints table fits its panel too, so its last column needs no horizontal scroll
    await expect(page.getByTestId('endpoint-row-jup-mock-model-b')).toBeVisible();
    const endpoints = await page.getByTestId('endpoint-table').evaluate((table) => {
      const holder = table.parentElement as HTMLElement;
      return { content: holder.scrollWidth, shown: holder.clientWidth };
    });
    expect(endpoints.content).toBeLessThanOrEqual(endpoints.shown);
    await page.getByRole('tab', { name: /Profiles/ }).click();
    await expect(page.getByTestId('profile-row-uiloop-profile-b')).toBeVisible();
    // the table fits its panel, so the Bans column needs no horizontal scroll
    const scroll = await page.getByTestId('profile-table').evaluate((table) => {
      const holder = table.parentElement as HTMLElement;
      return { content: holder.scrollWidth, shown: holder.clientWidth };
    });
    expect(scroll.content).toBeLessThanOrEqual(scroll.shown);
    // no name wraps so that a line of it is a single character
    for (const [testId, cell] of [
      ['profile-row-uiloop-profile-b', 'td:first-child'],
      ['profile-row-uiloop-profile-banned', 'td:first-child'],
      ['profile-row-uiloop-profile-b', '[data-testid=profile-endpoint]'],
    ]) {
      const lines = await textLines(page.getByTestId(testId).locator(cell));
      expect(lines.every((line) => line.trim().length > 1)).toBe(true);
    }
  });

  test('the endpoint and profile tables keep their Bans column visible at 1024 with names of 19 to 24 characters between separators', async ({
    page,
  }) => {
    await page.setViewportSize({ width: 1024, height: 900 });
    const longCredential = mockEntity('ModelCredential', CRED_ID, 'observabilityapikey-x', { ...CREDENTIAL.metadata.ModelCredential });
    const longEndpoint = mockEntity('ModelEndpoint', '00000000-0000-0000-0000-00000000a60b', 'instrumentationkey-a', {
      ...ENDPOINT.metadata.ModelEndpoint,
      credential: CRED_ID,
    });
    const banned = mockEntity('ModelProfile', '00000000-0000-0000-0000-00000000a60c', 'observabilitytriage-x', {
      ...PROFILE.metadata.ModelProfile,
      endpoint: '00000000-0000-0000-0000-00000000a60b',
      bans: { 'ban-1': BAN },
    });
    const plain = mockEntity('ModelProfile', '00000000-0000-0000-0000-00000000a60d', 'observabilityendpoint-b', {
      ...PROFILE.metadata.ModelProfile,
      endpoint: '00000000-0000-0000-0000-00000000a60b',
    });
    await mockModels(page, [longEndpoint, longCredential, banned, plain]);
    await page.goto('/models');
    await expect(page.getByTestId('endpoint-row-instrumentationkey-a')).toBeVisible();
    // a name chunk too long for its column breaks mid-word instead of widening the table past its panel
    const endpoints = await page.getByTestId('endpoint-table').evaluate((table) => {
      const holder = table.parentElement as HTMLElement;
      return { content: holder.scrollWidth, shown: holder.clientWidth };
    });
    expect(endpoints.content).toBeLessThanOrEqual(endpoints.shown);
    await page.getByRole('tab', { name: /Profiles/ }).click();
    await expect(page.getByTestId('profile-row-observabilityendpoint-b')).toBeVisible();
    const profiles = await page.getByTestId('profile-table').evaluate((table) => {
      const holder = table.parentElement as HTMLElement;
      return { content: holder.scrollWidth, shown: holder.clientWidth };
    });
    expect(profiles.content).toBeLessThanOrEqual(profiles.shown);
  });

  test('endpoint and credential names with a long first piece never start a line with a hyphen at 1024', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 900 });
    const longCredential = mockEntity('ModelCredential', CRED_ID, 'observability-key', { ...CREDENTIAL.metadata.ModelCredential });
    const longEndpoint = mockEntity('ModelEndpoint', '00000000-0000-0000-0000-00000000a60a', 'observability-endpoint-b', {
      ...ENDPOINT.metadata.ModelEndpoint,
      credential: CRED_ID,
    });
    await mockModels(page, [longEndpoint, longCredential]);
    await page.goto('/models');
    const row = page.getByTestId('endpoint-row-observability-endpoint-b');
    await expect(row).toBeVisible();
    // a mid-word break may fall in the long first piece, but the separator never begins a line
    for (const cell of [row.locator('td:first-child'), row.getByTestId('endpoint-credential')]) {
      const lines = await textLines(cell);
      expect(lines.every((line) => !line.trim().startsWith('-') && line.trim().length > 1)).toBe(true);
    }
  });

  test('the profile table fits its panel at 1024 with names of up to thirty-five characters', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 900 });
    const endpointIds = ['00000000-0000-0000-0000-00000000a610', '00000000-0000-0000-0000-00000000a611'];
    const endpoints = ['bedrock-claude-a', 'openai-compatible-endpoint-prod-a'].map((name, index) =>
      mockEntity('ModelEndpoint', endpointIds[index], name, {
        ...ENDPOINT.metadata.ModelEndpoint,
        catalog: ENDPOINT.metadata.ModelEndpoint.catalog.slice(0, 1),
      }),
    );
    // names whose hyphen pieces are short, so they group into chunks that are wide for their column
    const names = [
      'triage-agent-fast-tier',
      'triage-agent-default',
      'uiloop-profile-banned-by-ratelimit',
      'profile-for-triage-agent-a',
      // a short tail such as `v2` or `eu` follows a piece that is already at the chunk cap
      'claude-reasoning-v2',
      'haiku-triage-fast-eu',
      'triage-agent-fast-tier-v2',
      'sonnet-reasoning-eu',
    ];
    const profiles = names.map((name, index) =>
      mockEntity('ModelProfile', `00000000-0000-0000-0000-00000000a62${index}`, name, {
        ...PROFILE.metadata.ModelProfile,
        endpoint: endpointIds[index % 2],
        limits: { context_window: 128000, max_output_tokens: 8192 },
        bans: index === 2 ? { 'ban-1': BAN } : {},
      }),
    );
    await mockModels(page, [...endpoints, CREDENTIAL, ...profiles]);
    await page.goto('/models');
    await page.getByRole('tab', { name: /Profiles/ }).click();
    await expect(page.getByTestId('profile-row-triage-agent-default')).toBeVisible();
    // the table fits its panel, so no column is cut off behind a horizontal scroll
    const scroll = await page.getByTestId('profile-table').evaluate((table) => {
      const holder = table.parentElement as HTMLElement;
      return { content: holder.scrollWidth, shown: holder.clientWidth };
    });
    expect(scroll.content).toBeLessThanOrEqual(scroll.shown);
    // the banned profile's badge is inside the panel
    const badge = await page.getByTestId('profile-row-uiloop-profile-banned-by-ratelimit').getByTestId('ban-badge').boundingBox();
    const panel = await page
      .getByTestId('profile-table')
      .evaluate((table) => (table.parentElement as HTMLElement).getBoundingClientRect().right);
    expect((badge?.x ?? 0) + (badge?.width ?? 0)).toBeLessThanOrEqual(panel);
  });

  test('the endpoint and profile tables fit their panel at 1024 with words of up to twenty-six characters and every column filled', async ({
    page,
  }) => {
    await page.setViewportSize({ width: 1024, height: 900 });
    const credentials = ['authenticationprovider-key', 'classification-key'].map((name, index) =>
      mockEntity('ModelCredential', `00000000-0000-0000-0000-00000000a66${index}`, name, { ...CREDENTIAL.metadata.ModelCredential }),
    );
    const endpointNames = ['observabilityendpoint-b', 'classification-eu'];
    const endpointIds = endpointNames.map((_, index) => `00000000-0000-0000-0000-00000000a67${index}`);
    const endpoints = endpointNames.map((name, index) =>
      mockEntity('ModelEndpoint', endpointIds[index], name, {
        ...ENDPOINT.metadata.ModelEndpoint,
        credential: `00000000-0000-0000-0000-00000000a66${index}`,
        bans: { 'ban-1': BAN },
      }),
    );
    // a word of fifteen characters or fewer stays whole, and a longer one gives up width instead of pushing a column out
    const profileNames = ['internationalization-eu', 'instrumentationkey-a', 'classification-eu', 'observability-b'];
    const profiles = profileNames.map((name, index) =>
      mockEntity('ModelProfile', `00000000-0000-0000-0000-00000000a68${index}`, name, {
        ...PROFILE.metadata.ModelProfile,
        endpoint: endpointIds[index % 2],
        limits: { context_window: 1000000, max_output_tokens: 128000 },
        capabilities: { tool_calling: true },
        bans: { 'ban-1': BAN },
      }),
    );
    await mockModels(page, [...endpoints, ...credentials, ...profiles]);
    await page.goto('/models');
    await expect(page.getByTestId('endpoint-row-classification-eu')).toBeVisible();
    // each table fits its panel, so no column is cut off behind a horizontal scroll
    for (const [tab, table, ready] of [
      ['Endpoints', 'endpoint-table', 'endpoint-row-classification-eu'],
      ['Profiles', 'profile-table', 'profile-row-observability-b'],
    ]) {
      await page.getByRole('tab', { name: new RegExp(tab) }).click();
      await expect(page.getByTestId(ready)).toBeVisible();
      const scroll = await page.getByTestId(table).evaluate((element) => {
        const holder = element.parentElement as HTMLElement;
        return { content: holder.scrollWidth, shown: holder.clientWidth };
      });
      expect(scroll.content).toBeLessThanOrEqual(scroll.shown);
    }
    // the last column's ban badge is inside the panel
    const badge = await page.getByTestId('profile-row-observability-b').getByTestId('ban-badge').boundingBox();
    const panel = await page
      .getByTestId('profile-table')
      .evaluate((element) => (element.parentElement as HTMLElement).getBoundingClientRect().right);
    expect((badge?.x ?? 0) + (badge?.width ?? 0)).toBeLessThanOrEqual(panel);
  });

  for (const width of [1024, 1440]) {
    test(`names of eleven to fifteen character words stay whole without stranding a separator or character at ${width}`, async ({
      page,
    }) => {
      await page.setViewportSize({ width, height: 900 });
      const endpointNames = ['observability-b', 'multiregion-endpoint-b', 'observability-endpoint-b'];
      const endpointIds = endpointNames.map((_, index) => `00000000-0000-0000-0000-00000000a64${index}`);
      const endpoints = endpointNames.map((name, index) =>
        mockEntity('ModelEndpoint', endpointIds[index], name, {
          ...ENDPOINT.metadata.ModelEndpoint,
          catalog: ENDPOINT.metadata.ModelEndpoint.catalog.slice(0, 1),
        }),
      );
      const profileNames = ['classification-eu', 'summarization-a', 'multiregion-a'];
      const profiles = profileNames.map((name, index) =>
        mockEntity('ModelProfile', `00000000-0000-0000-0000-00000000a65${index}`, name, {
          ...PROFILE.metadata.ModelProfile,
          endpoint: endpointIds[index],
          // one short limit: with every column at its narrowest, the table has no room for a `context 128000` pair kept whole
          limits: { context_window: null, max_output_tokens: 8192 },
        }),
      );
      await mockModels(page, [...endpoints, CREDENTIAL, ...profiles]);
      await page.goto('/models');
      await expect(page.getByTestId('endpoint-row-observability-b')).toBeVisible();
      // no name or endpoint reference wraps so that a line of it is a lone character or a lone separator
      for (const name of endpointNames) {
        const lines = await textLines(page.getByTestId(`endpoint-row-${name}`).locator('td:first-child'));
        expect(lines.every((line) => line.trim().length > 1)).toBe(true);
      }
      await page.getByRole('tab', { name: /Profiles/ }).click();
      await expect(page.getByTestId('profile-row-summarization-a')).toBeVisible();
      for (const name of profileNames) {
        const row = page.getByTestId(`profile-row-${name}`);
        const lines = await textLines(row.locator('td:first-child'));
        expect(lines.every((line) => line.trim().length > 1)).toBe(true);
        const endpointLines = await textLines(row.getByTestId('profile-endpoint'));
        expect(endpointLines.every((line) => line.trim().length > 1)).toBe(true);
      }
      // the table still fits its panel
      const scroll = await page.getByTestId('profile-table').evaluate((table) => {
        const holder = table.parentElement as HTMLElement;
        return { content: holder.scrollWidth, shown: holder.clientWidth };
      });
      expect(scroll.content).toBeLessThanOrEqual(scroll.shown);
    });
  }

  test('an endpoint whose catalog bans a model with a realistic id still fits its panel at 1024 with its Bans column visible', async ({
    page,
  }) => {
    await page.setViewportSize({ width: 1024, height: 900 });
    const longCredential = mockEntity('ModelCredential', CRED_ID, 'jup-mock-model-k', { ...CREDENTIAL.metadata.ModelCredential });
    const bannedId = 'us.anthropic.claude-sonnet-4-5-20250929-v1:0';
    const bannedEndpoint = mockEntity('ModelEndpoint', '00000000-0000-0000-0000-00000000a609', 'jup-mock-model-b', {
      ...ENDPOINT.metadata.ModelEndpoint,
      // the provider URL of a cluster service, as wide as a real endpoint's
      provider: {
        OpenAiCompatible: {
          base_url: 'http://jup-mock-model.jup-gate-fixtures.svc.cluster.local:8080/m/stub-model-b/v1',
          extra_headers: {},
          key_header: null,
        },
      },
      catalog: [
        ENDPOINT.metadata.ModelEndpoint.catalog[0],
        { model_id: bannedId, limits: { context_window: 200000, max_output_tokens: 64000 }, capabilities: null, bans: { 'ban-1': BAN } },
      ],
    });
    await mockModels(page, [bannedEndpoint, longCredential]);
    await page.goto('/models');
    const row = page.getByTestId('endpoint-row-jup-mock-model-b');
    await expect(row.getByTestId('model-ban-badge')).toContainText(bannedId);
    // the table fits its panel, so the Bans column needs no horizontal scroll
    const fit = await page.getByTestId('endpoint-table').evaluate((table) => {
      const holder = table.parentElement as HTMLElement;
      return { content: holder.scrollWidth, shown: holder.clientWidth };
    });
    expect(fit.content).toBeLessThanOrEqual(fit.shown);
    // the ban badge sits inside the table, not clipped at its edge
    const badge = await row.getByTestId('model-ban-badge').boundingBox();
    const table = await page.getByTestId('endpoint-table').boundingBox();
    expect((badge?.x ?? 0) + (badge?.width ?? 0)).toBeLessThanOrEqual((table?.x ?? 0) + (table?.width ?? 0));
    // the badge wraps its id only after a separator, so no word of it breaks and no line starts with a separator
    const lines = (await textLines(row.getByTestId('model-ban-badge').getByTestId('id-text'))).map((line) => line.trim());
    expect(lines.join('')).toBe(bannedId);
    expect(lines.every((line) => /^[a-z0-9]/.test(line))).toBe(true);
    expect(lines.slice(0, -1).every((line) => /[.:/_@-]$/.test(line))).toBe(true);
  });

  // OpenRouter, Together and vendor ids whose first piece is longer than any word of a Bedrock id
  for (const bannedId of [
    'cognitivecomputations/dolphin-mixtral-8x22b',
    'togethercomputer/RedPajama-INCITE-7B-Chat',
    'vendor.abcdefghijklmnopqrstuvwx-v1',
  ]) {
    test(`an endpoint whose catalog bans ${bannedId} fits its panel at 1024 with its ban badge unclipped`, async ({ page }) => {
      await page.setViewportSize({ width: 1024, height: 900 });
      const bannedEndpoint = mockEntity('ModelEndpoint', '00000000-0000-0000-0000-00000000a60a', 'jup-mock-model-b', {
        ...ENDPOINT.metadata.ModelEndpoint,
        provider: {
          OpenAiCompatible: {
            base_url: 'http://jup-mock-model.jup-gate-fixtures.svc.cluster.local:8080/m/stub-model-b/v1',
            extra_headers: {},
            key_header: null,
          },
        },
        catalog: [
          ENDPOINT.metadata.ModelEndpoint.catalog[0],
          { model_id: bannedId, limits: { context_window: 128000, max_output_tokens: 8192 }, capabilities: null, bans: { 'ban-1': BAN } },
        ],
      });
      await mockModels(page, [bannedEndpoint, CREDENTIAL]);
      await page.goto('/models');
      const row = page.getByTestId('endpoint-row-jup-mock-model-b');
      await expect(row.getByTestId('model-ban-badge')).toContainText(bannedId);
      // the table fits its panel, so the Bans column needs no horizontal scroll
      const fit = await page.getByTestId('endpoint-table').evaluate((table) => {
        const holder = table.parentElement as HTMLElement;
        return { content: holder.scrollWidth, shown: holder.clientWidth };
      });
      expect(fit.content).toBeLessThanOrEqual(fit.shown);
      // the ban badge sits inside the table, not clipped at its edge
      const badge = await row.getByTestId('model-ban-badge').boundingBox();
      const table = await page.getByTestId('endpoint-table').boundingBox();
      expect((badge?.x ?? 0) + (badge?.width ?? 0)).toBeLessThanOrEqual((table?.x ?? 0) + (table?.width ?? 0));
    });
  }

  /**
   * Serve one endpoint whose catalog bans the given model, and open the models page at 1024 pixels wide.
   *
   * @param page - The page to drive.
   * @param bannedId - The id of the banned catalog model.
   * @returns The endpoint's table row.
   */
  async function openBannedEndpoint(page: Page, bannedId: string): Promise<Locator> {
    await page.setViewportSize({ width: 1024, height: 900 });
    const endpoint = mockEntity('ModelEndpoint', '00000000-0000-0000-0000-00000000a60a', 'jup-mock-model-c', {
      ...ENDPOINT.metadata.ModelEndpoint,
      catalog: [
        ENDPOINT.metadata.ModelEndpoint.catalog[0],
        { model_id: bannedId, limits: null, capabilities: null, bans: { 'ban-1': BAN } },
      ],
    });
    await mockModels(page, [endpoint, CREDENTIAL]);
    await page.goto('/models');
    const row = page.getByTestId('endpoint-row-jup-mock-model-c');
    await expect(row.getByTestId('model-ban-badge')).toContainText(bannedId);
    return row;
  }

  test('a banned id with a long organisation prefix breaks the prefix, so its badge stays narrow and inside the table', async ({
    page,
  }) => {
    const bannedId = 'cognitivecomputations/dolphin-mixtral-8x22b';
    const row = await openBannedEndpoint(page, bannedId);
    // the Bans column is no wider than a badge that wraps its id, rather than holding the 22 character prefix whole
    const bans = await page.getByTestId('endpoint-table').locator('thead th').last().boundingBox();
    expect(bans?.width ?? 0).toBeLessThan(100);
    // the badge sits inside the table, not clipped at its edge
    const badge = await row.getByTestId('model-ban-badge').boundingBox();
    const table = await page.getByTestId('endpoint-table').boundingBox();
    expect((badge?.x ?? 0) + (badge?.width ?? 0)).toBeLessThanOrEqual((table?.x ?? 0) + (table?.width ?? 0));
    // the id still reads in order, and the words after the prefix stay whole
    const lines = (await textLines(row.getByTestId('model-ban-badge').getByTestId('id-text'))).map((line) => line.trim());
    expect(lines.join('')).toBe(bannedId);
    expect(lines.some((line) => line.includes('dolphin-'))).toBe(true);
  });

  test('an endpoint catalog lists each limit on its own line with its label kept with its value', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 900 });
    await mockModels(page, [ENDPOINT, CREDENTIAL, PROFILE]);
    await page.goto('/models');
    // each `label value` pair is a block of its own, kept on one line while the table has room for it
    const models = page.getByTestId('endpoint-row-mock-ep').getByTestId('endpoint-models');
    const context = models.getByText('context 128000', { exact: true });
    const output = models.getByText('output 8192', { exact: true });
    await expect(context).toBeVisible();
    for (const limit of [context, output]) {
      expect(await limit.evaluate((el) => el.getClientRects().length)).toBe(1);
      expect(await limit.evaluate((el) => getComputedStyle(el).display)).toBe('block');
    }
    // the second limit sits below the first rather than beside it
    const first = await context.boundingBox();
    const second = await output.boundingBox();
    expect(second?.y ?? 0).toBeGreaterThan((first?.y ?? 0) + (first?.height ?? 0) - 1);
  });

  test('a profile lists each limit on one line at 1024, and a 1M context window is the only one that breaks', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 900 });
    const big = mockEntity('ModelProfile', '00000000-0000-0000-0000-00000000a670', 'uiloop-big-window', {
      ...PROFILE.metadata.ModelProfile,
      limits: { context_window: 1000000, max_output_tokens: 8192 },
    });
    await mockModels(page, [ENDPOINT, CREDENTIAL, PROFILE, big]);
    await page.goto('/models');
    await page.getByRole('tab', { name: /Profiles/ }).click();
    await expect(page.getByTestId('profile-row-uiloop-big-window')).toBeVisible();
    const plain = page.getByTestId(`profile-row-${PROFILE.name}`).getByTestId('profile-limits');
    const large = page.getByTestId('profile-row-uiloop-big-window').getByTestId('profile-limits');
    // a limit that fits keeps its label with its value, so a profile's limits read as two lines
    for (const text of ['context 128000', 'output 8192']) {
      const limit = plain.getByText(text, { exact: true });
      await expect(limit).toBeVisible();
      expect(await limit.evaluate((el) => el.getClientRects().length)).toBe(1);
    }
    // a 1M window may wrap between its label and its value, while the short limit beside it stays whole
    expect(await large.getByText('context 1000000', { exact: true }).evaluate((el) => getComputedStyle(el).whiteSpace)).toBe('normal');
    expect(await large.getByText('output 8192', { exact: true }).evaluate((el) => el.getClientRects().length)).toBe(1);
  });

  test('a list that fails to load shows the error and neither an empty state nor zero counts', async ({ page }) => {
    await setupMockAuth(page);
    await page.route('**/api/entities/details/**', (route) => fulfillJson(route, { error: 'uiloop: injected failure' }, 500));
    await page.goto('/models');
    await expect(page.getByText('uiloop: injected failure').first()).toBeVisible();
    await expect(page.getByTestId('model-empty')).toHaveCount(0);
    // a count that was never loaded is not shown as 0
    await expect(page.getByRole('tab', { name: /Endpoints/ })).toHaveText('Endpoints');
  });

  test('credential rows show the fingerprint and the non-secret kind fields', async ({ page }) => {
    await mockModels(page, [ENDPOINT, CREDENTIAL, ROLE]);
    await page.goto('/models');
    await page.getByRole('tab', { name: /Credentials/ }).click();
    await expect(page.getByTestId('credential-row-mock-key').getByTestId('credential-fingerprint')).toHaveText('98f4255836b1');
    // the fingerprint and rotated timestamp stay whole rather than wrapping inside a narrow column
    const row = page.getByTestId('credential-row-mock-key');
    await expect(row.getByTestId('credential-fingerprint').locator('code')).toHaveCSS('overflow-wrap', 'normal');
    await expect(row.getByTestId('credential-fingerprint').locator('code')).toHaveCSS('word-break', 'normal');
    await expect(row.getByText('2026-10-28 10:29:24')).toHaveCSS('white-space', 'nowrap');
    // each of them occupies a single line box
    for (const cell of [row.getByTestId('credential-fingerprint').locator('code'), row.getByText('2026-10-28 10:29:24')]) {
      expect(await cell.evaluate((el) => el.getClientRects().length)).toBe(1);
    }
    const role = page.getByTestId('credential-row-mock-role').getByTestId('credential-kind');
    await expect(role).toContainText('AwsAssumeRole');
    await expect(role).toContainText('role_arn: arn:aws:iam::1:role/r');
  });

  test('a user with no model entities sees an empty state on each tab', async ({ page }) => {
    await mockModels(page, []);
    await page.goto('/models');
    await expect(page.getByTestId('model-empty')).toContainText('No model endpoints');
    await page.getByRole('tab', { name: /Profiles/ }).click();
    await expect(page.getByTestId('model-empty')).toContainText('No model profiles');
    await page.getByRole('tab', { name: /Credentials/ }).click();
    await expect(page.getByTestId('model-empty')).toContainText('No model credentials');
  });
});

test.describe('system settings (mocked)', () => {
  test('the models allow list reads as left aligned provider and base URL lines in their own case', async ({ page }) => {
    await setupMockAuth(page);
    await page.route('**/api/system/settings', (route) =>
      fulfillJson(route, { reserved_cpu: 0, models: { allowed_endpoints: [{ provider: 'OpenAiCompatible', base_url: BASE_URL }] } }),
    );
    await page.goto('/settings');
    const cell = page.getByTestId('setting-models');
    await expect(cell).toContainText(`OpenAiCompatible ${BASE_URL}`);
    await expect(cell.locator('div')).toHaveCSS('text-transform', 'none');
    await expect(cell.locator('div')).toHaveCSS('text-align', 'left');
  });

  test('each allow list entry is its own block that hangs its wrapped text, and an empty list reads (none) like a scalar', async ({
    page,
  }) => {
    await setupMockAuth(page);
    await page.setViewportSize({ width: 1024, height: 800 });
    await page.route('**/api/system/settings', (route) =>
      fulfillJson(route, {
        host_path_whitelist: [],
        models: {
          allowed_endpoints: [{ provider: 'OpenAiCompatible', base_url: `${BASE_URL}/with/a/very/long/path/that/wraps/${'x'.repeat(80)}` }],
        },
      }),
    );
    await page.goto('/settings');
    const lines = page.getByTestId('setting-models').getByTestId('setting-line');
    await expect(lines).toHaveCount(2);
    const [key, entry] = await Promise.all(
      [lines.nth(0), lines.nth(1)].map((line) =>
        line.evaluate((el) => {
          const style = getComputedStyle(el);
          return { left: parseFloat(style.paddingLeft) + parseFloat(style.textIndent), height: el.getBoundingClientRect().height };
        }),
      ),
    );
    // the entry sits one level in from its key, and it wraps onto more lines than the key
    expect(entry.left).toBeGreaterThan(key.left);
    expect(entry.height).toBeGreaterThan(key.height);
    // the continuation of the entry is indented past the first line's start
    expect(await lines.nth(1).evaluate((el) => parseFloat(getComputedStyle(el).paddingLeft))).toBeGreaterThan(
      await lines.nth(1).evaluate((el) => parseFloat(getComputedStyle(el).paddingLeft) + parseFloat(getComputedStyle(el).textIndent)),
    );
    // an empty list is centered like the other scalars, not left aligned monospace
    await expect(page.getByTestId('setting-host_path_whitelist').locator('center')).toContainText(/none/i);
  });
});
