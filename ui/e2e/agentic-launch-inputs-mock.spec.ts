import { test } from '@playwright/test';
import type { Page, Route } from '@playwright/test';

import { MOCK_USER } from './helpers';
import { expect } from './agentic-mocks';

// Mocked tests of the launch page's inputs: the Files / Repos / Both selector, the sample hash tokens and their
// lookups, the per-tab state, the shareable URL, and the Run Analysis links from the file and repo pages.

/** The ids of the mocked tool, its version, the workflow, and its instance. */
const IDS = {
  tool: '11111111-1111-4111-8111-111111111111',
  toolV2: '22222222-2222-4222-8222-222222222222',
  tool2: '33333333-3333-4333-8333-333333333333',
  tool2V1: '44444444-4444-4444-8444-444444444444',
  workflow: '55555555-5555-4555-8555-555555555555',
  workflowInstance: '66666666-6666-4666-8666-666666666666',
  run: '77777777-7777-4777-8777-777777777777',
};

/** A sample the user can see, one the API doesn't return, and one whose lookup never answers. */
const GOOD = 'a'.repeat(64);
const MISSING = 'b'.repeat(64);
const SLOW = 'c'.repeat(64);

/** Answer a route with a JSON body. */
function json(route: Route, body: unknown, status = 200) {
  return route.fulfill({ status, contentType: 'application/json', body: JSON.stringify(body) });
}

/**
 * Sign in, serve one tool, one workflow, and one pipeline, and answer the sample lookups.
 *
 * @param page - The page whose requests are mocked.
 * @returns The sample lookups made, by hash.
 */
async function mockLaunch(page: Page) {
  const lookups: string[] = [];
  await page.route('**/api/**', (route) => json(route, {}));
  await page.route('**/api/users/whoami', (route) => json(route, MOCK_USER));
  const base = { description: null, submitter: 'test', groups: ['system'], tags: {}, image: null, created: '2026-01-01T00:00:00Z' };
  const tool = {
    ...base,
    id: IDS.tool,
    name: 'scan',
    kind: 'Tool',
    metadata: {
      Tool: {
        kind: 'Image',
        latest: IDS.toolV2,
        latest_version: { SemVer: '2.0.0' },
        latest_spec_hash: 'b',
        modified: '2026-01-01T00:00:00Z',
        instances: [{ id: IDS.toolV2, version: { SemVer: '2.0.0' }, spec_hash: 'b', deprecated: false, created: '2026-01-02T00:00:00Z' }],
      },
    },
  };
  const tool2 = {
    ...base,
    id: IDS.tool2,
    name: 'strings',
    kind: 'Tool',
    metadata: {
      Tool: {
        kind: 'Image',
        latest: IDS.tool2V1,
        latest_version: { SemVer: '1.0.0' },
        latest_spec_hash: 'c',
        modified: '2026-01-01T00:00:00Z',
        instances: [{ id: IDS.tool2V1, version: { SemVer: '1.0.0' }, spec_hash: 'c', deprecated: false, created: '2026-01-02T00:00:00Z' }],
      },
    },
  };
  const workflow = {
    ...base,
    id: IDS.workflow,
    name: 'triage',
    kind: 'WorkflowCatalog',
    metadata: { WorkflowCatalog: { latest_instance: IDS.workflowInstance } },
  };
  await page.route('**/api/entities/details/**', (route) => {
    const url = route.request().url();
    return json(route, { data: url.includes('WorkflowCatalog') ? [workflow] : url.includes('Tool') ? [tool, tool2] : [], cursor: null });
  });
  await page.route(`**/api/entities/${IDS.workflowInstance}`, (route) =>
    json(route, {
      ...workflow,
      id: IDS.workflowInstance,
      kind: 'WorkflowInstance',
      metadata: { WorkflowInstance: { revision: 1, status: 'Ready' } },
    }),
  );
  await page.route('**/api/pipelines/list/system/details/**', (route) =>
    json(route, {
      details: [{ group: 'system', name: 'triage', order: [['scan']], sla: 3600, stage_tools: { scan: { tool: IDS.tool } } }],
    }),
  );
  await page.route('**/api/files/sample/*', (route) => {
    const hash = new URL(route.request().url()).pathname.split('/').pop() ?? '';
    lookups.push(hash);
    if (hash === GOOD) {
      return json(route, {
        sha256: GOOD,
        sha1: '',
        md5: '',
        tags: {},
        comments: [],
        submissions: [
          {
            id: 's',
            name: 'evil.exe',
            description: null,
            groups: ['system'],
            submitter: 'test',
            uploaded: '2026-01-01T00:00:00Z',
            origin: 'None',
          },
        ],
      });
    }
    // a lookup that never answers keeps its token pending
    if (hash === SLOW) return new Promise<void>(() => undefined);
    return json(route, { error: 'not found' }, 404);
  });
  await page.context().addCookies([{ name: 'THORIUM_TOKEN', value: MOCK_USER.token, domain: 'localhost', path: '/' }]);
  return lookups;
}

/**
 * Enter hashes into the existing samples field, one token each.
 *
 * @param page - The launch page.
 * @param hashes - The entries to add.
 */
async function enterSamples(page: Page, hashes: string[]) {
  const entry = page.getByTestId('run-samples');
  for (const hash of hashes) {
    await entry.fill(hash);
    await entry.press('Enter');
  }
}

test.describe('launch inputs (mocked)', () => {
  test('a workflow ref that is not in the catalog offers no instance request and sends none', async ({ page }) => {
    await mockLaunch(page);
    const requests: string[] = [];
    page.on('request', (request) => {
      if (request.method() === 'POST') requests.push(new URL(request.url()).pathname);
    });
    await page.goto('/launch?workflow=..%2F..%2Fx');
    // the ref is shown with the reason it can't run, and no way to request an instance for it
    await expect(page.getByTestId('run-item')).toHaveCount(1);
    await expect(page.getByTestId('run-item-blocker')).toContainText('is not available to you');
    await expect(page.getByTestId('request-instance')).toHaveCount(0);
    await expect(page.getByTestId('no-ready-instance')).toHaveCount(0);
    expect(requests).toEqual([]);
  });

  test('the selector shows one input section at a time and keeps hidden values without sending them', async ({ page }) => {
    await mockLaunch(page);
    const posted: unknown[] = [];
    await page.route(`**/api/workflows/instances/${IDS.workflowInstance}/runs`, (route) => {
      posted.push(route.request().postDataJSON());
      return json(route, { id: IDS.run });
    });
    await page.goto(`/launch?workflow=${IDS.workflow}`);
    // Files is the default: the dropzone and the sample tokens show, the repo doesn't
    await expect(page.getByTestId('dropzone-compact')).toBeVisible();
    await expect(page.getByTestId('run-repo')).toHaveCount(0);
    await enterSamples(page, [GOOD]);
    await expect(page.getByTestId(`hash-token-${GOOD}`)).toHaveAttribute('data-status', 'valid');
    // Repos hides the samples, and with no repo the button says what is missing
    await page.getByTestId('inputs-mode-repos').click();
    await expect(page.getByTestId('run-samples')).toHaveCount(0);
    await expect(page.getByTestId('run-submit')).toBeDisabled();
    await expect(page.getByTestId('run-submit-hint')).toContainText('Enter a repo or a prompt');
    await page.getByTestId('run-repo').fill('github.com/org/project:main');
    // switching back keeps the sample that was entered
    await page.getByTestId('inputs-mode-files').click();
    await expect(page.getByTestId(`hash-token-${GOOD}`)).toBeVisible();
    await page.getByTestId('inputs-mode-repos').click();
    await expect(page.getByTestId('run-repo')).toHaveValue('github.com/org/project:main');
    await page.getByTestId('run-submit').click();
    await expect.poll(() => posted.length).toBe(1);
    // only the shown repo is sent, not the hidden sample
    expect(posted[0]).toMatchObject({ inputs: { samples: [], repos: [{ url: 'github.com/org/project', commitish: 'main' }] } });
  });

  test('Both sends the samples and the repo', async ({ page }) => {
    await mockLaunch(page);
    const posted: unknown[] = [];
    await page.route(`**/api/workflows/instances/${IDS.workflowInstance}/runs`, (route) => {
      posted.push(route.request().postDataJSON());
      return json(route, { id: IDS.run });
    });
    await page.goto(`/launch?workflow=${IDS.workflow}&inputs=both&sample=${GOOD}&repo=github.com/org/project`);
    await expect(page.getByTestId(`hash-token-${GOOD}`)).toHaveAttribute('data-status', 'valid');
    await page.getByTestId('run-submit').click();
    await expect.poll(() => posted.length).toBe(1);
    expect(posted[0]).toMatchObject({ inputs: { samples: [GOOD], repos: [{ url: 'github.com/org/project' }] } });
  });

  test('each entered hash is checked: found, not accessible, malformed, and pending, and the button waits', async ({ page }) => {
    const lookups = await mockLaunch(page);
    await page.goto(`/launch?workflow=${IDS.workflow}`);
    await expect(page.getByTestId('run-samples')).toBeVisible();
    await enterSamples(page, [GOOD, MISSING, 'nothex']);
    const good = page.getByTestId(`hash-token-${GOOD}`);
    await expect(good).toHaveAttribute('data-status', 'valid');
    await expect(good.getByRole('link')).toHaveAttribute('href', `/file/${GOOD}`);
    await expect(good).toContainText('evil.exe');
    await expect(page.getByTestId(`hash-token-${MISSING}`)).toHaveAttribute('data-status', 'missing');
    await expect(page.getByTestId('hash-token-nothex')).toHaveAttribute('data-status', 'malformed');
    // a malformed entry is never looked up
    expect(lookups).not.toContain('nothex');
    await expect(page.getByTestId('run-submit')).toBeDisabled();
    await expect(page.getByTestId('run-submit-hint')).toContainText('"nothex" is not a SHA256');
    await page.getByTestId('hash-token-remove-nothex').click();
    await expect(page.getByTestId('run-submit-hint')).toContainText('not found or is not accessible');
    await page.getByTestId(`hash-token-remove-${MISSING}`).click();
    await expect(page.getByTestId('run-submit')).toBeEnabled();
    // a hash whose lookup hasn't answered keeps the button disabled
    await enterSamples(page, [SLOW]);
    await expect(page.getByTestId(`hash-token-${SLOW}`)).toHaveAttribute('data-status', 'pending');
    await expect(page.getByTestId('run-submit-hint')).toContainText('Checking');
    // each hash is looked up once
    expect(lookups.filter((hash) => hash === GOOD)).toHaveLength(1);
  });

  test('chosen files are listed with a remove control', async ({ page }) => {
    await mockLaunch(page);
    await page.goto(`/launch?workflow=${IDS.workflow}`);
    await page.getByTestId('run-file').setInputFiles([
      { name: 'one.bin', mimeType: 'application/octet-stream', buffer: Buffer.from('1') },
      { name: 'two.bin', mimeType: 'application/octet-stream', buffer: Buffer.from('22') },
    ]);
    await expect(page.getByTestId('dropzone-file')).toHaveCount(2);
    await expect(page.getByTestId('run-submit')).toBeEnabled();
    await page.getByRole('button', { name: 'Remove one.bin' }).click();
    await expect(page.getByTestId('dropzone-file')).toHaveCount(1);
    await expect(page.getByTestId('dropzone-file')).toContainText('two.bin');
  });

  test('each tab keeps its choices and options, and one double click starts each listed run once', async ({ page }) => {
    await mockLaunch(page);
    const toolRuns: unknown[] = [];
    const reactions: unknown[] = [];
    await page.route(`**/api/tools/instances/*/runs`, (route) => {
      toolRuns.push({ url: route.request().url(), body: route.request().postDataJSON() });
      return json(route, { id: IDS.run });
    });
    await page.route('**/api/reactions/', (route) => {
      reactions.push(route.request().postDataJSON());
      return json(route, { id: IDS.run });
    });
    await page.goto('/launch?tab=pipeline');
    await page.getByTestId('run-pipeline-system/triage').check();
    await page.getByTestId('run-item-tags').fill('pipeline-tag');
    await page.getByTestId('launch-tab-tool').click();
    await page.getByTestId(`run-tool-${IDS.tool}`).check();
    // a tool ticked and then unticked is not listed, so it is never started
    await page.getByTestId(`run-tool-${IDS.tool2}`).check();
    await page.getByTestId(`run-tool-${IDS.tool2}`).uncheck();
    await expect(page.getByTestId('run-tool-version')).toHaveValue(IDS.toolV2);
    await page.getByTestId('run-tags').fill('shared-tag');
    await enterSamples(page, [GOOD]);
    await page.getByTestId('launch-tab-pipeline').click();
    await expect(page.getByTestId('run-pipeline-system/triage')).toBeChecked();
    await expect(page.getByTestId('run-item')).toHaveCount(2);
    await expect(page.getByTestId('run-submit')).toHaveText('Launch 2 runs');
    await page.getByTestId('run-submit').dblclick();
    await expect(page.getByTestId('launch-result-link')).toHaveCount(2);
    expect(toolRuns).toHaveLength(1);
    expect(reactions).toHaveLength(1);
    expect(toolRuns[0]).toMatchObject({ body: { inputs: { samples: [GOOD] }, tags: ['shared-tag'] } });
    expect((toolRuns[0] as { url: string }).url).toContain(IDS.toolV2);
    expect(reactions[0]).toMatchObject({ pipeline: 'triage', samples: [GOOD], tags: ['shared-tag', 'pipeline-tag'] });
    // the started runs leave the list so they can't be started twice, and the inputs stay for the next launch
    await expect(page.getByTestId('run-item')).toHaveCount(0);
    await expect(page.getByTestId(`hash-token-${GOOD}`)).toBeVisible();
  });

  test('switching tabs and back keeps the pipeline choice and its options', async ({ page }) => {
    await mockLaunch(page);
    await page.goto('/launch?tab=pipeline');
    await page.getByTestId('run-pipeline-system/triage').check();
    await page.getByTestId('run-item-tags').fill('pipeline-tag');
    await page.getByTestId('launch-tab-tool').click();
    await page.getByTestId('launch-tab-pipeline').click();
    await expect(page.getByTestId('run-pipeline-system/triage')).toBeChecked();
    await expect(page.getByTestId('run-item-tags')).toHaveValue('pipeline-tag');
    // a pipeline may run with no inputs
    await expect(page.getByTestId('inputs-requirement')).toContainText('may run with no inputs');
    await expect(page.getByTestId('run-submit')).toBeEnabled();
  });

  test('one failing run is reported beside the runs that started, and stays listed to retry', async ({ page }) => {
    await mockLaunch(page);
    await page.route(`**/api/tools/instances/${IDS.toolV2}/runs`, (route) => json(route, { id: IDS.run }));
    await page.route(`**/api/tools/instances/${IDS.tool2V1}/runs`, (route) => json(route, { error: 'strings is broken' }, 400));
    await page.goto(`/launch?tab=tool&tool=${IDS.tool}&tool=${IDS.tool2}&sample=${GOOD}`);
    await expect(page.getByTestId('run-submit')).toHaveText('Launch 2 runs');
    await expect(page.getByTestId(`hash-token-${GOOD}`)).toHaveAttribute('data-status', 'valid');
    await page.getByTestId('run-submit').click();
    await expect(page.getByTestId('launch-results-summary')).toContainText('Started 1 of 2 runs');
    await expect(page.getByTestId('launch-result-link').getByRole('link')).toHaveAttribute('href', `/runs/system/${IDS.run}`);
    await expect(page.getByTestId('launch-error')).toContainText('strings is broken');
    await expect(page.getByTestId('run-item')).toHaveCount(1);
    await expect(page.getByTestId('run-item-name')).toHaveText('strings');
  });

  test('the URL follows the inputs and a reload restores them', async ({ page }) => {
    await mockLaunch(page);
    await page.goto(`/launch?workflow=${IDS.workflow}`);
    await enterSamples(page, [GOOD, MISSING]);
    await expect.poll(() => new URL(page.url()).searchParams.getAll('sample')).toEqual([GOOD, MISSING]);
    await page.reload();
    await expect(page.getByTestId(`hash-token-${GOOD}`)).toHaveAttribute('data-status', 'valid');
    await expect(page.getByTestId(`hash-token-${MISSING}`)).toHaveAttribute('data-status', 'missing');
    await expect(page.locator(`[data-testid="run-item"][data-key="workflow:${IDS.workflow}"]`)).toBeVisible();
  });

  test('Run Analysis on a file page opens the run page with the file filled in', async ({ page }) => {
    await mockLaunch(page);
    await page.route('**/api/files/count/**', (route) => json(route, { tags: {} }));
    await page.route('**/api/trees/**', (route) => json(route, { error: 'no graph' }, 404));
    await page.route(`**/api/files/results/${GOOD}**`, (route) => json(route, { results: {} }));
    await page.goto(`/file/${GOOD}`);
    const link = page.getByTestId('file-run-analysis');
    await expect(link).toHaveAttribute('href', `/launch?sample=${GOOD}`);
    // the old Create Reactions tab is now Run Analysis, pointing at the same page
    await page.getByRole('tab', { name: 'Run Analysis' }).click();
    await expect(page.getByTestId('file-run-analysis-tab-link')).toHaveAttribute('href', `/launch?sample=${GOOD}`);
    await link.click();
    await expect(page).toHaveURL(new RegExp(`/launch\\?sample=${GOOD}`));
    // the inputs already hold the file
    await expect(page.getByTestId(`hash-token-${GOOD}`)).toHaveAttribute('data-status', 'valid');
    await expect(page.getByTestId('inputs-mode-files')).toHaveAttribute('aria-selected', 'true');
  });

  test('a run link naming only a repo opens the Repos mode', async ({ page }) => {
    await mockLaunch(page);
    await page.goto(`/launch?workflow=${IDS.workflow}&repo=github.com/org/project`);
    await expect(page.getByTestId('inputs-mode-repos')).toHaveAttribute('aria-selected', 'true');
    await expect(page.getByTestId('run-repo')).toHaveValue('github.com/org/project');
  });

  test('Run Analysis on a file page starts two tools and a pipeline on the file in one launch', async ({ page }) => {
    await mockLaunch(page);
    await page.route('**/api/files/count/**', (route) => json(route, { tags: {} }));
    await page.route('**/api/trees/**', (route) => json(route, { error: 'no graph' }, 404));
    await page.route(`**/api/files/results/${GOOD}**`, (route) => json(route, { results: {} }));
    const started: string[] = [];
    let next = 0;
    const created = () => {
      next += 1;
      return `00000000-0000-4000-8000-00000000000${next}`;
    };
    await page.route('**/api/tools/instances/*/runs', (route) => {
      started.push(new URL(route.request().url()).pathname);
      return json(route, { id: created() });
    });
    await page.route('**/api/reactions/', (route) => {
      started.push('reaction');
      return json(route, { id: created() });
    });
    await page.goto(`/file/${GOOD}`);
    await page.getByTestId('file-run-analysis').click();
    await expect(page).toHaveURL(new RegExp(`/launch\\?sample=${GOOD}`));
    await page.getByTestId('launch-tab-tool').click();
    await page.getByTestId(`run-tool-${IDS.tool}`).check();
    await page.getByTestId(`run-tool-${IDS.tool2}`).check();
    await page.getByTestId('launch-tab-pipeline').click();
    await page.getByTestId('run-pipeline-search').fill('tri');
    await page.getByTestId('run-pipeline-system/triage').check();
    await expect(page.getByTestId(`hash-token-${GOOD}`)).toHaveAttribute('data-status', 'valid');
    await expect(page.getByTestId('runs-inputs-summary')).toContainText('1 existing sample');
    await page.getByRole('button', { name: 'Launch 3 runs' }).click();
    await expect(page.getByTestId('launch-result-link')).toHaveCount(3);
    await expect(page.getByTestId('launch-results-summary')).toContainText('Started 3 of 3 runs');
    expect(started).toHaveLength(3);
    expect(started.filter((entry) => entry === 'reaction')).toHaveLength(1);
    for (const link of await page.getByTestId('launch-result-link').getByRole('link').all()) {
      await expect(link).toHaveAttribute('href', /^\/runs\/system\/00000000-/);
    }
  });

  test('a shared launch URL with several runs restores them, their versions, and the sample after a reload', async ({ page }) => {
    await mockLaunch(page);
    await page.goto(`/launch?sample=${GOOD}`);
    await page.getByTestId('launch-tab-tool').click();
    await page.getByTestId(`run-tool-${IDS.tool}`).check();
    await page.getByTestId(`run-tool-${IDS.tool2}`).check();
    await page.getByTestId('launch-tab-pipeline').click();
    await page.getByTestId('run-pipeline-system/triage').check();
    await expect
      .poll(() => {
        const params = new URL(page.url()).searchParams;
        return [params.getAll('tool'), params.getAll('instance'), params.getAll('pipeline'), params.getAll('sample')];
      })
      .toEqual([[IDS.tool, IDS.tool2], [IDS.toolV2, IDS.tool2V1], ['system/triage'], [GOOD]]);
    const shared = page.url();
    await page.reload();
    await expect(page.getByTestId('run-item')).toHaveCount(3);
    await expect(page.getByTestId('run-submit')).toHaveText('Launch 3 runs');
    await expect(page.getByTestId('run-tool-version').first()).toHaveValue(IDS.toolV2);
    await expect(page.getByTestId(`hash-token-${GOOD}`)).toHaveAttribute('data-status', 'valid');
    // the same link opened fresh holds the same runs
    await page.goto('/launch');
    await page.goto(shared);
    await expect(page.getByTestId('run-item-name')).toHaveText(['scan', 'strings', 'triage']);
  });
});
