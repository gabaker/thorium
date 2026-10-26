import { expect, test } from '@playwright/test';
import type { Page, Route } from '@playwright/test';

import { MOCK_USER } from './helpers';

// Mocked tests: they drive the run pages and launch forms against a stubbed API (no live backend
// needed), so request ordering, failures, and wrong-kind ids can be staged exactly.

/** A tool id, its two instance ids, and a harness tool with its instance id. */
const IDS = {
  tool: '11111111-1111-4111-8111-111111111111',
  toolV1: '22222222-2222-4222-8222-222222222221',
  toolV2: '22222222-2222-4222-8222-222222222222',
  harnessTool: '33333333-3333-4333-8333-333333333333',
  harnessInstance: '44444444-4444-4444-8444-444444444444',
  workflow: '55555555-5555-4555-8555-555555555555',
  workflowInstance: '66666666-6666-4666-8666-666666666666',
  run: '77777777-7777-4777-8777-777777777777',
  result: '88888888-8888-4888-8888-888888888888',
};

/** A sample SHA256 the mocked runs read. */
const SHA = 'a'.repeat(64);

/** The result key a workflow run stores its results under. */
const RESULT_KEY = `triage#r3~${IDS.workflowInstance}`;

/** Answer a route with a JSON body. */
function json(route: Route, body: unknown, status = 200) {
  return route.fulfill({ status, contentType: 'application/json', body: JSON.stringify(body) });
}

/** Sign in as a mocked user and answer every other API call with an empty object. */
async function mockAuth(page: Page, user: Record<string, unknown> = MOCK_USER) {
  // catch-all first so the specific routes registered later take priority
  await page.route('**/api/**', (route) => json(route, {}));
  await page.route('**/api/users/whoami', (route) => json(route, user));
  await page.context().addCookies([{ name: 'THORIUM_TOKEN', value: MOCK_USER.token, domain: 'localhost', path: '/' }]);
}

/** Build a run in a group with a status, a plan, and a source. */
function makeRun(overrides: Record<string, unknown>) {
  return {
    id: IDS.run,
    group: 'system',
    creator: 'test',
    pipeline: 'triage',
    status: 'Completed',
    current_stage: 0,
    current_stage_progress: 0,
    current_stage_length: 1,
    args: {},
    sla: '2099-01-01T00:00:00Z',
    jobs: [],
    tags: [],
    sub_reactions: 0,
    completed_sub_reactions: 0,
    generators: [],
    samples: [SHA],
    ephemeral: [],
    parent_ephemeral: {},
    repos: [],
    has_cache: false,
    plan: { stages: [[{ name: 'scan', tool_instance: IDS.toolV1 }]] },
    ...overrides,
  };
}

/** A workflow run source whose harness instance belongs to the harness tool. */
const WORKFLOW_SOURCE = {
  WorkflowRun: {
    workflow_instance: IDS.workflowInstance,
    digest: 'd',
    agent: 'agent-id',
    harness: IDS.harnessInstance,
    result_key: RESULT_KEY,
    models: [],
    provenance: {
      agents: [],
      tools: [
        {
          tool: IDS.harnessTool,
          tool_instance: IDS.harnessInstance,
          name: 'uip-harness',
          kind: 'Harness',
          version: '1.0.0',
          spec_hash: 'h',
        },
      ],
    },
  },
};

/** A runnable tool with two versions; 2.0.0 is the latest. */
const TOOL = {
  id: IDS.tool,
  name: 'scan',
  kind: 'Tool',
  description: null,
  submitter: 'test',
  groups: ['system'],
  tags: {},
  image: null,
  created: '2026-01-01T00:00:00Z',
  metadata: {
    Tool: {
      kind: 'Image',
      latest: IDS.toolV2,
      latest_version: { SemVer: '2.0.0' },
      latest_spec_hash: 'b',
      modified: '2026-01-01T00:00:00Z',
      instances: [
        { id: IDS.toolV2, version: { SemVer: '2.0.0' }, spec_hash: 'b', deprecated: false, created: '2026-01-02T00:00:00Z' },
        { id: IDS.toolV1, version: { SemVer: '1.0.0' }, spec_hash: 'a', deprecated: false, created: '2026-01-01T00:00:00Z' },
      ],
    },
  },
};

test.describe('run pages (mocked)', () => {
  test('a workflow run result file downloads from its encoded result key', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ source: WORKFLOW_SOURCE })));
    await page.route(`**/api/files/results/${SHA}**`, (route) =>
      json(route, { results: { [RESULT_KEY]: [{ id: IDS.result, result: { ok: true }, files: ['report.txt'] }] } }),
    );
    const downloads: string[] = [];
    await page.route('**/api/files/result-files/**', (route) => {
      downloads.push(route.request().url());
      return route.fulfill({ status: 200, contentType: 'application/octet-stream', body: 'bytes' });
    });
    await page.goto(`/runs/system/${IDS.run}`);
    await page.getByRole('button', { name: 'Download report.txt' }).click();
    await expect.poll(() => downloads.length).toBe(1);
    expect(new URL(downloads[0]).pathname).toBe(`/api/files/result-files/${SHA}/${encodeURIComponent(RESULT_KEY)}/${IDS.result}`);
  });

  test('the harness link opens the harness tool, not its instance id', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ source: WORKFLOW_SOURCE })));
    await page.goto(`/runs/system/${IDS.run}`);
    await expect(page.getByTestId('run-source').getByRole('link', { name: 'uip-harness' })).toHaveAttribute(
      'href',
      `/tools/${IDS.harnessTool}`,
    );
  });

  test('the source names the workflow instance and the harness without their ids', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ source: WORKFLOW_SOURCE })));
    await page.goto(`/runs/system/${IDS.run}`);
    const source = page.getByTestId('run-source');
    await expect(source.getByRole('link', { name: 'triage#r3' })).toHaveAttribute('href', `/workflows/instances/${IDS.workflowInstance}`);
    await expect(source.getByRole('link', { name: 'uip-harness' })).toBeVisible();
    await expect(source.getByText(IDS.harnessInstance)).toHaveCount(0);
    // the instance cell holds only its name; the result key below still carries the id
    await expect(source.locator('dd').first()).toHaveText('triage#r3');
  });

  test('a user whose role cannot run is not offered Retry', async ({ page }) => {
    await mockAuth(page, { ...MOCK_USER, role: 'User' });
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ status: 'Failed', source: WORKFLOW_SOURCE })));
    await page.goto(`/runs/system/${IDS.run}`);
    await expect(page.getByTestId('run-retry')).toBeDisabled();
    await expect(page.getByTestId('run-retry-role-hint')).toBeVisible();
  });

  test('a refused retry reads as a sentence with the trace kept as a hover hint', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ status: 'Failed', source: WORKFLOW_SOURCE })));
    await page.route(`**/api/workflows/instances/${IDS.workflowInstance}/runs/${IDS.run}/retry`, (route) =>
      json(route, { error: "you can't run in `system`: you aren't a developer for K8s", trace: 'abc123' }, 401),
    );
    await page.goto(`/runs/system/${IDS.run}`);
    await expect(page.getByTestId('run-retry-role-hint')).toHaveCount(0);
    await page.getByTestId('run-retry').click();
    const refusal = page.getByTestId('run-action-error');
    await expect(refusal).toContainText("you can't run in 'system'");
    await expect(refusal).not.toContainText('`');
    await expect(refusal).not.toContainText('trace');
    await expect(refusal).toHaveAttribute('title', 'trace: abc123');
  });

  test('a tool page opened with a tool instance id reports it instead of spinning', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/entities/${IDS.harnessInstance}`, (route) =>
      json(route, { ...TOOL, id: IDS.harnessInstance, kind: 'ToolInstance', metadata: { ToolInstance: { tool: IDS.harnessTool } } }),
    );
    await page.route(`**/api/entities/tools/${IDS.harnessInstance}/instances`, (route) => json(route, {}, 404));
    await page.goto(`/tools/${IDS.harnessInstance}`);
    await expect(page.getByTestId('entity-not-found')).toContainText(`${IDS.harnessInstance} is a tool version, not a tool`, {
      timeout: 15_000,
    });
    // the entity exists, so the page does not claim it cannot be seen
    await expect(page.getByTestId('entity-not-found')).not.toContainText('does not exist');
  });

  test('hide logs collapses a stage of a failed run', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ status: 'Failed' })));
    await page.route(`**/api/reactions/logs/system/${IDS.run}/scan**`, (route) => json(route, { logs: ['boom'] }));
    await page.goto(`/runs/system/${IDS.run}`);
    const toggle = page.getByTestId('toggle-logs-scan');
    // a failed run opens its stage logs at first
    await expect(toggle).toHaveText('Hide logs');
    await expect(page.getByTestId('stage-logs-scan')).toBeVisible();
    await toggle.click();
    await expect(toggle).toHaveText('Show logs');
    await expect(page.getByTestId('stage-logs-scan')).toHaveCount(0);
  });

  test('a failed log poll does not hide the logs a later poll reads', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ status: 'Started' })));
    let calls = 0;
    await page.route(`**/api/reactions/logs/system/${IDS.run}/scan**`, (route) => {
      calls += 1;
      // the first poll fails, every later poll succeeds
      return calls === 1 ? json(route, { error: 'down' }, 500) : json(route, { logs: ['line one'] });
    });
    await page.goto(`/runs/system/${IDS.run}`);
    await page.getByTestId('toggle-logs-scan').click();
    await expect(page.getByTestId('stage-logs-scan')).toHaveText('line one', { timeout: 15_000 });
  });

  test('the runs page settles to the empty state for a user in no groups', async ({ page }) => {
    await mockAuth(page, { ...MOCK_USER, groups: [] });
    await page.goto('/runs');
    await expect(page.getByTestId('no-runs')).toBeVisible({ timeout: 15_000 });
  });

  test('a slow sweep started before a group change does not overwrite the new group', async ({ page }) => {
    await mockAuth(page, { ...MOCK_USER, groups: ['a', 'b'] });
    // group b answers slowly so the all-groups sweep finishes after the one-group sweep
    const listFor = async (route: Route, group: string) => {
      if (group === 'b') await new Promise((resolve) => setTimeout(resolve, 600));
      const runs = [makeRun({ id: `${group}0000000-0000-4000-8000-000000000000`, group, pipeline: `run-${group}` })];
      return json(route, { details: runs });
    };
    await page.route('**/api/reactions/tag/*/WorkflowRun/details/**', (route) =>
      listFor(route, new URL(route.request().url()).pathname.split('/')[4]),
    );
    await page.route('**/api/reactions/group/*/*/details/**', (route) =>
      listFor(route, new URL(route.request().url()).pathname.split('/')[4]),
    );
    await page.goto('/runs');
    await page.getByTestId('runs-group').selectOption('a');
    await expect(page.getByTestId('runs-table')).toBeVisible({ timeout: 15_000 });
    // wait past the slow sweep: five slow requests of group b
    await page.waitForTimeout(4_000);
    await expect(page.getByRole('cell', { name: 'b', exact: true })).toHaveCount(0);
    await expect(page.getByRole('cell', { name: 'a', exact: true })).toHaveCount(1);
  });

  test('refreshes of a slow runs list never overlap', async ({ page }) => {
    await mockAuth(page, { ...MOCK_USER, groups: ['a'] });
    let inFlight = 0;
    let most = 0;
    // each request takes a second, so one sweep of five requests outlasts the refresh period
    const slow = async (route: Route) => {
      inFlight += 1;
      most = Math.max(most, inFlight);
      await new Promise((resolve) => setTimeout(resolve, 1_000));
      inFlight -= 1;
      return json(route, { details: [makeRun({ group: 'a', status: 'Started' })] });
    };
    await page.route('**/api/reactions/tag/*/WorkflowRun/details/**', slow);
    await page.route('**/api/reactions/group/*/*/details/**', slow);
    await page.goto('/runs');
    await expect(page.getByTestId('run-row')).toHaveCount(1, { timeout: 15_000 });
    // development mode mounts the page twice, so count overlaps only once the refreshes begin
    await page.waitForTimeout(500);
    most = inFlight;
    await page.waitForTimeout(12_000);
    expect(most).toBe(1);
  });

  test('a sweep where every request fails keeps the rows and keeps polling', async ({ page }) => {
    await mockAuth(page, { ...MOCK_USER, groups: ['a'] });
    let down = false;
    const list = (route: Route) =>
      down ? json(route, { error: 'down' }, 503) : json(route, { details: [makeRun({ group: 'a', status: 'Started' })] });
    await page.route('**/api/reactions/tag/*/WorkflowRun/details/**', list);
    await page.route('**/api/reactions/group/*/*/details/**', list);
    await page.goto('/runs');
    await expect(page.getByTestId('run-row')).toHaveCount(1, { timeout: 15_000 });
    down = true;
    // one refresh of the list fails entirely
    await page.waitForTimeout(4_000);
    await expect(page.getByTestId('run-row')).toHaveCount(1);
    down = false;
    let polled = false;
    await page.route('**/api/reactions/group/*/*/details/**', (route) => {
      polled = true;
      return list(route);
    });
    await expect.poll(() => polled, { timeout: 10_000 }).toBe(true);
  });

  test('the runs list error clears once a later read succeeds', async ({ page }) => {
    await mockAuth(page, { ...MOCK_USER, groups: ['a'] });
    let down = true;
    // reads of the Completed listing fail until the test lets them through
    const list = (route: Route) =>
      down && route.request().url().includes('/Completed/')
        ? json(route, { error: 'uiloop transient' }, 503)
        : json(route, { details: [makeRun({ group: 'a', status: 'Started' })] });
    await page.route('**/api/reactions/tag/*/WorkflowRun/details/**', list);
    await page.route('**/api/reactions/group/*/*/details/**', list);
    await page.goto('/runs');
    await expect(page.getByText('uiloop transient')).toBeVisible({ timeout: 15_000 });
    down = false;
    // the background refresh reads every listing again and clears the error
    await expect(page.getByText('uiloop transient')).toHaveCount(0, { timeout: 15_000 });
    await expect(page.getByTestId('run-row')).toHaveCount(1);
  });

  test('a group change where every read fails clears the old group rows and paging', async ({ page }) => {
    await mockAuth(page, { ...MOCK_USER, groups: ['a', 'b'] });
    let down = false;
    // group a lists a run with a further page; every read fails once the test says so
    const list = (route: Route) => {
      const url = new URL(route.request().url());
      if (down) return json(route, { error: 'uiloop list failure' }, 500);
      const group = url.pathname.split('/')[4];
      if (!url.pathname.includes('/Completed/')) return json(route, { details: [] });
      return json(route, { details: [makeRun({ group, pipeline: `run-${group}` })], cursor: 50 });
    };
    await page.route('**/api/reactions/tag/*/WorkflowRun/details/**', list);
    await page.route('**/api/reactions/group/*/*/details/**', list);
    await page.goto('/runs');
    await expect(page.getByTestId('run-row')).toHaveCount(1, { timeout: 15_000 });
    await expect(page.getByTestId('runs-load-more')).toBeVisible();
    down = true;
    await page.getByTestId('runs-group').selectOption('b');
    // the error is shown, and nothing of the previous group stays under the new one
    await expect(page.getByText('uiloop list failure').first()).toBeVisible({ timeout: 15_000 });
    await expect(page.getByTestId('run-row')).toHaveCount(0);
    await expect(page.getByTestId('runs-load-more')).toHaveCount(0);
    await expect(page.getByRole('tab', { name: /^All 0$/ })).toBeVisible();
  });

  test('the cancel confirm goes away when the run finishes', async ({ page }) => {
    await mockAuth(page);
    let status = 'Started';
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ status })));
    await page.goto(`/runs/system/${IDS.run}`);
    await page.getByTestId('run-cancel').click();
    await expect(page.getByTestId('run-cancel-confirm')).toBeVisible();
    status = 'Completed';
    await expect(page.getByTestId('run-status')).toHaveText('Completed', { timeout: 15_000 });
    await expect(page.getByTestId('run-cancel-confirm')).toHaveCount(0);
    await expect(page.getByText('Fail this run?')).toHaveCount(0);
  });

  test('a stage log error is an error block, not text beside the logs button', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ status: 'Failed' })));
    await page.route(`**/api/reactions/logs/system/${IDS.run}/scan**`, (route) => json(route, { error: 'uiloop logs failure' }, 500));
    await page.goto(`/runs/system/${IDS.run}`);
    const error = page.getByTestId('stage-logs-error-scan');
    await expect(error).toContainText('uiloop logs failure');
    // the message sits below the button instead of on its line
    const button = await page.getByTestId('toggle-logs-scan').boundingBox();
    const message = await error.boundingBox();
    expect(message!.y).toBeGreaterThanOrEqual(button!.y + button!.height);
  });

  test('the run inputs name the file each SHA256 was submitted as', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ source: WORKFLOW_SOURCE })));
    await page.route(`**/api/files/sample/${SHA}`, (route) =>
      json(route, { sha256: SHA, submissions: [{ id: 'x', name: 'prompt.txt', groups: ['system'], submitter: 'test' }], comments: [] }),
    );
    await page.goto(`/runs/system/${IDS.run}`);
    await expect(page.getByTestId('run-input-name')).toHaveText('(prompt.txt)');
  });
});

test.describe('launch forms (mocked)', () => {
  /** Answer the entity listing with the tool, or the workflow, by the kind asked for. */
  async function mockCatalog(page: Page, workflowGroups: string[]) {
    const workflow = {
      ...TOOL,
      id: IDS.workflow,
      name: 'triage',
      kind: 'WorkflowCatalog',
      groups: workflowGroups,
      metadata: { WorkflowCatalog: { latest_instance: IDS.workflowInstance } },
    };
    await page.route('**/api/entities/details/**', (route) => {
      const url = route.request().url();
      const data = url.includes('WorkflowCatalog') ? [workflow] : url.includes('Tool') ? [TOOL] : [];
      return json(route, { data, cursor: null });
    });
    await page.route(`**/api/entities/${IDS.workflowInstance}`, (route) =>
      json(route, { ...workflow, id: IDS.workflowInstance, kind: 'WorkflowInstance', metadata: { WorkflowInstance: {} } }),
    );
  }

  test('a tool deep link preselects the tool and the linked version', async ({ page }) => {
    await mockAuth(page);
    await mockCatalog(page, ['system']);
    await page.goto(`/launch?tab=tool&tool=${IDS.tool}&instance=${IDS.toolV1}`);
    await expect(page.getByTestId(`run-tool-${IDS.tool}`)).toBeChecked({ timeout: 15_000 });
    await expect(page.getByTestId('run-tool-version')).toHaveValue(IDS.toolV1);
  });

  test('a tool link without a version preselects the tool and its latest version', async ({ page }) => {
    await mockAuth(page);
    await mockCatalog(page, ['system']);
    await page.goto(`/launch?tab=tool&tool=${IDS.tool}`);
    await expect(page.getByTestId(`run-tool-${IDS.tool}`)).toBeChecked({ timeout: 15_000 });
    await expect(page.getByTestId('run-tool-version')).toHaveValue(IDS.toolV2);
  });

  test('a launch in a group the user is not in never links to /runs/undefined', async ({ page }) => {
    // an admin may run in a group they are not a member of; the API picks the group
    await mockAuth(page, { ...MOCK_USER, groups: ['other'] });
    await mockCatalog(page, ['wfonly']);
    await page.route(`**/api/workflows/instances/${IDS.workflowInstance}/runs`, (route) => json(route, { id: IDS.run }));
    await page.goto(`/launch?workflow=${IDS.workflow}`);
    await page.getByTestId('run-samples').fill(SHA);
    await page.getByRole('button', { name: 'Launch 1 run' }).click();
    const link = page.getByTestId('launch-result-link').getByRole('link');
    await expect(link).toHaveAttribute('href', `/runs/wfonly/${IDS.run}`, { timeout: 15_000 });
  });
});

test.describe('run pages polish (mocked)', () => {
  /** A user whose role can't start runs. */
  const PLAIN_USER = { ...MOCK_USER, role: 'User' };

  /** Answer the entity listing with one workflow (when given) and the tool. */
  async function mockLists(page: Page, withWorkflow: boolean) {
    const workflow = {
      ...TOOL,
      id: IDS.workflow,
      name: 'triage',
      kind: 'WorkflowCatalog',
      groups: ['system'],
      metadata: { WorkflowCatalog: { latest_instance: IDS.workflowInstance } },
    };
    await page.route('**/api/entities/details/**', (route) => {
      const url = route.request().url();
      const data = url.includes('WorkflowCatalog') ? (withWorkflow ? [workflow] : []) : url.includes('Tool') ? [TOOL] : [];
      return json(route, { data, cursor: null });
    });
    await page.route(`**/api/entities/${IDS.workflowInstance}`, (route) =>
      json(route, {
        ...workflow,
        id: IDS.workflowInstance,
        kind: 'WorkflowInstance',
        metadata: { WorkflowInstance: { revision: 1, status: 'Ready' } },
      }),
    );
  }

  test('a junk SLA is flagged beside its field and the run is not started', async ({ page }) => {
    await mockAuth(page);
    await mockLists(page, true);
    let posted = 0;
    await page.route(`**/api/tools/instances/${IDS.toolV2}/runs`, (route) => {
      posted += 1;
      return json(route, { id: IDS.run });
    });
    await page.goto(`/launch?tab=tool&tool=${IDS.tool}`);
    await expect(page.getByTestId('run-tool-version')).toHaveValue(IDS.toolV2, { timeout: 15_000 });
    await page.getByTestId('run-samples').fill(SHA);
    for (const sla of ['abc', '-5', '0', '1.5']) {
      await page.getByTestId('run-sla').fill(sla);
      await expect(page.getByTestId('run-sla-problem')).toContainText('positive whole number of seconds');
      await expect(page.getByTestId('run-submit')).toBeDisabled();
    }
    await page.getByTestId('run-sla').fill('120');
    await expect(page.getByTestId('run-sla-problem')).toHaveCount(0);
    await expect(page.getByTestId('run-submit')).toBeEnabled();
    expect(posted).toBe(0);
  });

  test('a user whose role cannot run is told before the form is filled, and the refusal reads cleanly', async ({ page }) => {
    await mockAuth(page, PLAIN_USER);
    await mockLists(page, true);
    await page.route(`**/api/workflows/instances/${IDS.workflowInstance}/runs`, (route) =>
      json(route, { error: "you can't run in `system`: you aren't a developer for K8s", trace: 'abc123' }, 401),
    );
    await page.goto(`/launch?workflow=${IDS.workflow}`);
    await expect(page.getByTestId('run-role-hint')).toBeVisible();
    await page.getByTestId('run-samples').fill(SHA);
    await page.getByTestId('run-submit').click();
    const refusal = page.getByTestId('launch-error');
    await expect(refusal).toContainText("you can't run in 'system'");
    await expect(refusal).not.toContainText('`');
    await expect(refusal).not.toContainText('trace');
    await expect(refusal).toHaveAttribute('title', 'trace: abc123');
    // the refusal shows beside the Run button rather than above the form, out of view
    await expect(refusal).toBeInViewport();
    // the refused workflow stays in the list to retry
    await expect(page.getByTestId('run-item')).toHaveCount(1);
    await expect(page.getByTestId('launch-results-summary')).toContainText('Started 0 of 1 run');
  });

  test('a pipeline list that fails for one group is reported, and the other groups still list', async ({ page }) => {
    await mockAuth(page, { ...MOCK_USER, groups: ['system', 'broken'] });
    await page.route('**/api/pipelines/list/system/details/**', (route) =>
      json(route, { details: [{ group: 'system', name: 'triage', order: [['scan']], sla: 3600 }] }),
    );
    await page.route('**/api/pipelines/list/broken/details/**', (route) => json(route, { error: 'pipelines unavailable' }, 500));
    await page.goto('/launch?tab=pipeline');
    await expect(page.getByTestId('pipeline-list-error')).toContainText('pipelines unavailable');
    await expect(page.getByTestId('run-pipeline-system/triage')).toHaveCount(1);
  });

  test('a chosen workflow names its instance, links to it, and shows that it is Ready', async ({ page }) => {
    await mockAuth(page);
    await mockLists(page, true);
    await page.goto(`/launch?workflow=${IDS.workflow}`);
    const line = page.getByTestId('run-instance-name');
    await expect(line.getByRole('link', { name: 'triage#r1' })).toHaveAttribute('href', `/workflows/instances/${IDS.workflowInstance}`);
    await expect(line).toContainText('Ready');
  });

  test('a typed sample that is not a SHA256 is refused before anything is sent', async ({ page }) => {
    await mockAuth(page);
    await mockLists(page, true);
    let posted = 0;
    await page.route(`**/api/workflows/instances/${IDS.workflowInstance}/runs`, (route) => {
      posted += 1;
      return json(route, { id: IDS.run });
    });
    await page.goto(`/launch?workflow=${IDS.workflow}`);
    await page.getByTestId('run-samples').fill('nothex');
    await page.getByTestId('run-samples').press('Enter');
    // the entry becomes an error token at once, and the Run button says why it is disabled
    await expect(page.getByTestId('hash-token-nothex')).toHaveAttribute('data-status', 'malformed');
    await expect(page.getByTestId('run-submit')).toBeDisabled();
    await expect(page.getByTestId('run-submit-hint')).toContainText('"nothex" is not a SHA256');
    expect(posted).toBe(0);
  });

  test('an admin sees no role hint', async ({ page }) => {
    await mockAuth(page);
    await mockLists(page, true);
    await page.goto(`/launch?workflow=${IDS.workflow}`);
    await expect(page.getByTestId('run-submit')).toBeVisible();
    await expect(page.getByTestId('run-role-hint')).toHaveCount(0);
  });

  test('a user who sees no workflows is pointed at the workflow list and the tool tab', async ({ page }) => {
    await mockAuth(page);
    await mockLists(page, false);
    await page.goto('/launch');
    const empty = page.getByTestId('no-workflows');
    await expect(empty).toBeVisible();
    await expect(empty.getByRole('link', { name: 'Browse workflows' })).toHaveAttribute('href', '/workflows');
    await expect(empty.getByRole('link', { name: 'run a single tool' })).toHaveAttribute('href', '/launch?tab=tool');
    // the link switches the open page to the Tool tab
    await empty.getByRole('link', { name: 'run a single tool' }).click();
    await expect(page.getByRole('heading', { name: 'Choose tools' })).toBeVisible();
    await expect(page.getByTestId('no-workflows')).toHaveCount(0);
  });

  test('an error from a workflow the user switched away from is not shown over the new one', async ({ page }) => {
    await mockAuth(page);
    const other = { ...TOOL, id: IDS.tool, name: 'other', kind: 'WorkflowCatalog', groups: ['system'] };
    const slow = {
      ...TOOL,
      id: IDS.workflow,
      name: 'slow',
      kind: 'WorkflowCatalog',
      groups: ['system'],
      metadata: { WorkflowCatalog: { latest_instance: IDS.workflowInstance } },
    };
    other.metadata = { WorkflowCatalog: { latest_instance: IDS.result } } as unknown as typeof TOOL.metadata;
    await page.route('**/api/entities/details/**', (route) =>
      json(route, { data: route.request().url().includes('WorkflowCatalog') ? [slow, other] : [], cursor: null }),
    );
    // the first workflow's instance read fails late; the second one's succeeds at once
    await page.route(`**/api/entities/${IDS.workflowInstance}`, async (route) => {
      await new Promise((resolve) => setTimeout(resolve, 1_500));
      return json(route, { error: 'stale instance read' }, 500);
    });
    await page.route(`**/api/entities/${IDS.result}`, (route) =>
      json(route, { ...other, id: IDS.result, kind: 'WorkflowInstance', metadata: { WorkflowInstance: { revision: 1 } } }),
    );
    await page.goto(`/launch?workflow=${IDS.workflow}`);
    await page.getByTestId(`run-workflow-${IDS.tool}`).check();
    await page.getByTestId(`run-workflow-${IDS.workflow}`).uncheck();
    await expect(page.getByTestId('run-instance-name')).toBeVisible();
    await page.waitForTimeout(2_500);
    await expect(page.getByTestId('launch-error')).toHaveCount(0);
  });

  test('a run the user cannot read shows a titled not-found page, reads it once, and links back to the list', async ({ page }) => {
    await mockAuth(page);
    let reads = 0;
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => {
      reads += 1;
      return json(route, { error: 'Unauthorized' }, 401);
    });
    await page.goto(`/runs/system/${IDS.run}`);
    await expect(page.getByTestId('entity-not-found-title')).toHaveText('Run not found');
    await expect(page.getByRole('link', { name: 'Back to runs' })).toHaveAttribute('href', '/runs');
    // polling is every two seconds, so wait past two periods for a stray retry
    await page.waitForTimeout(5_000);
    // development mode mounts the page twice, so a single read per mount is the most there can be
    expect(reads).toBeLessThanOrEqual(2);
  });

  test('a run page names its agents, shows readable times and updates, and counts steps from one', async ({ page }) => {
    await mockAuth(page);
    const endpoint = '99999999-9999-4999-8999-999999999999';
    const source = {
      WorkflowRun: {
        ...WORKFLOW_SOURCE.WorkflowRun,
        agent: 'agent-id',
        models: [
          {
            agent: 'agent-id',
            level: 'Group',
            endpoint: { Group: endpoint },
            model_id: 'm',
            api: 'OpenAiChat',
            priority: 0,
            profile: null,
          },
        ],
        provenance: {
          ...WORKFLOW_SOURCE.WorkflowRun.provenance,
          agents: [{ agent: 'agent-id', name: 'demo-agent', config_hash: 'c', toolset_digest: 't' }],
        },
      },
    };
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) =>
      json(route, makeRun({ source, current_stage: 1, current_stage_progress: 1, sla: '2026-10-28T17:52:39.990698273Z' })),
    );
    await page.route(`**/api/entities/${endpoint}`, (route) =>
      json(route, { ...TOOL, id: endpoint, name: 'demo-endpoint', kind: 'ModelEndpoint', metadata: { ModelEndpoint: {} } }),
    );
    await page.route(`**/api/reactions/logs/system/${IDS.run}**`, (route) =>
      json(route, [
        {
          group: 'system',
          pipeline: 'triage',
          reaction: IDS.run,
          action: 'ReactionCreated',
          timestamp: '2026-10-28T15:52:39.993492946Z',
          msg: null,
          update: { id: IDS.run, group: 'system', status: '"Created"', current_stage: '0' },
        },
      ]),
    );
    await page.goto(`/runs/system/${IDS.run}`);
    const source_ = page.getByTestId('run-source');
    await expect(source_.getByRole('link', { name: 'demo-agent' }).first()).toHaveAttribute('href', '/agents/agent-id');
    await expect(page.getByTestId('run-models')).toContainText('demo-endpoint');
    await expect(page.getByTestId('run-models')).not.toContainText(endpoint);
    await expect(page.getByTestId('run-stage-text')).toHaveText('the stage completed');
    const updates = page.getByTestId('run-updates');
    await expect(updates.getByTestId('update-field-status')).toContainText('Created');
    await expect(updates).not.toContainText('993492946');
    await expect(updates).not.toContainText('\\"');
    await expect(page.getByTestId('run-summary')).not.toContainText('990698273');
  });

  test('the single reaction status page of a workflow run reads no pipeline and shows the plan stages', async ({ page }) => {
    await mockAuth(page);
    let pipelineReads = 0;
    await page.route('**/api/pipelines/data/**', (route) => {
      pipelineReads += 1;
      return json(route, { error: 'not found' }, 404);
    });
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ source: WORKFLOW_SOURCE })));
    await page.goto(`/reaction/system/${IDS.run}`);
    await expect(page.getByTestId('run-banner')).toBeVisible();
    await expect(page.locator('[data-testid="stage-detail"][data-stage="scan"]')).toBeVisible();
    expect(pipelineReads).toBe(0);
    // the banner does not touch the details card below it
    // the banner sits in the page's banner slot, so measure from the slot to the card that follows it
    const banner = page.getByTestId('run-banner').locator('xpath=ancestor::*[following-sibling::*][1]');
    const card = banner.locator('xpath=following-sibling::*[1]');
    const above = await banner.boundingBox();
    const below = await card.boundingBox();
    expect((below?.y ?? 0) - ((above?.y ?? 0) + (above?.height ?? 0))).toBeGreaterThanOrEqual(8);
  });

  test('the single reaction status page keeps a loaded reaction through a failed poll and recovers', async ({ page }) => {
    await mockAuth(page);
    await page.route('**/api/pipelines/data/**', (route) => json(route, { group: 'system', name: 'triage', order: [['scan']], sla: 3600 }));
    let reads = 0;
    // the first read succeeds, the second fails, and later reads succeed again
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => {
      reads += 1;
      if (reads === 2) return json(route, { error: 'upstream unavailable' }, 503);
      return json(route, makeRun({ source: undefined, status: 'Started' }));
    });
    await page.goto(`/reaction/system/${IDS.run}`);
    await expect(page.getByTestId('run-banner')).toBeVisible();
    await expect(page.getByTestId('reaction-refresh-error')).toBeVisible({ timeout: 10_000 });
    await expect(page.getByTestId('entity-not-found')).toHaveCount(0);
    await expect(page.getByTestId('run-banner')).toBeVisible();
    await expect(page.getByTestId('reaction-refresh-error')).toHaveCount(0, { timeout: 10_000 });
  });

  test('the single reaction status page of a reaction that never loads says it was not found', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, { error: 'not found' }, 404));
    await page.goto(`/reaction/system/${IDS.run}`);
    await expect(page.getByTestId('entity-not-found')).toBeVisible();
  });

  test('a pipeline reaction page links to its run page', async ({ page }) => {
    await mockAuth(page);
    await page.route('**/api/pipelines/data/**', (route) => json(route, { group: 'system', name: 'triage', order: [['scan']], sla: 3600 }));
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ source: undefined })));
    await page.goto(`/reaction/system/${IDS.run}`);
    await expect(page.getByTestId('run-banner').getByRole('link', { name: 'Open it in the run view' })).toHaveAttribute(
      'href',
      `/runs/system/${IDS.run}`,
    );
  });

  test('the empty runs list notice sits below the group select, not beside it', async ({ page }) => {
    await mockAuth(page, { ...MOCK_USER, groups: [] });
    await page.goto('/runs');
    const select = page.getByTestId('runs-group');
    const notice = page.getByTestId('no-runs');
    await expect(notice).toBeVisible();
    const [selectBox, noticeBox] = [await select.boundingBox(), await notice.boundingBox()];
    expect(noticeBox!.y).toBeGreaterThanOrEqual(selectBox!.y + selectBox!.height);
  });

  test('the status updates table stays inside the card at a narrow width', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 900 });
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ source: WORKFLOW_SOURCE })));
    await page.route(`**/api/reactions/logs/system/${IDS.run}**`, (route) =>
      json(route, [
        {
          group: 'system',
          pipeline: 'triage',
          reaction: IDS.run,
          action: 'JobCreated',
          timestamp: '2026-10-28T15:52:39.993492946Z',
          msg: 'x'.repeat(400),
          update: {},
        },
      ]),
    );
    await page.goto(`/runs/system/${IDS.run}`);
    await expect(page.getByTestId('run-updates')).toContainText('xxxx');
    const [scroll, client] = await page.evaluate(() => [document.documentElement.scrollWidth, document.documentElement.clientWidth]);
    expect(scroll).toBeLessThanOrEqual(client);
  });

  test('the status updates Time and Action cells keep their text whole at 1024 and 1440', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ source: WORKFLOW_SOURCE })));
    await page.route(`**/api/reactions/logs/system/${IDS.run}**`, (route) =>
      json(route, [
        {
          group: 'system',
          pipeline: 'triage',
          reaction: IDS.run,
          action: 'ReactionCompleted',
          timestamp: '2026-10-28T15:52:39.993492946Z',
          msg: 'x'.repeat(400),
          update: {},
        },
      ]),
    );
    for (const width of [1024, 1440]) {
      await page.setViewportSize({ width, height: 900 });
      await page.goto(`/runs/system/${IDS.run}`);
      await expect(page.getByTestId('run-updates')).toContainText('xxxx');
      const cells = page.getByTestId('run-updates').locator('tbody tr').first().locator('td');
      // a cell whose text stays on one line has all its client rects on one line (an element inside the cell adds a rect of its own)
      for (const index of [0, 1]) {
        const wrapped = await cells.nth(index).evaluate((cell) => {
          const range = document.createRange();
          range.selectNodeContents(cell);
          const tops = [...range.getClientRects()].filter((rect) => rect.width > 0).map((rect) => Math.round(rect.top));
          return new Set(tops).size > 1;
        });
        expect(wrapped, `cell ${index} at ${width}`).toBe(false);
      }
    }
  });
});

test.describe('run results, tags, sub reactions, and paging (mocked)', () => {
  /** The status updates of a run created at 05:00:00 and failed at 05:01:00. */
  const UPDATES = [
    { group: 'system', pipeline: 'triage', reaction: IDS.run, action: 'ReactionCreated', timestamp: '2026-10-29T05:00:00Z', update: {} },
    { group: 'system', pipeline: 'triage', reaction: IDS.run, action: 'ReactionFailed', timestamp: '2026-10-29T05:01:00Z', update: {} },
  ];

  test('a run shows only the results stored while it ran', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ status: 'Failed', source: WORKFLOW_SOURCE })));
    await page.route(`**/api/reactions/logs/system/${IDS.run}**`, (route) => json(route, UPDATES));
    await page.route(`**/api/files/results/${SHA}**`, (route) =>
      json(route, {
        results: {
          [RESULT_KEY]: [
            { id: 'newer', result: { from: 'a later run' }, uploaded: '2026-10-29T06:00:00Z' },
            { id: 'own', result: { from: 'this run' }, uploaded: '2026-10-29T05:00:30Z', tool_version: { SemVer: '1.0.0' } },
            { id: 'older', result: { from: 'an earlier run' }, uploaded: '2026-10-29T04:00:00Z' },
          ],
        },
      }),
    );
    await page.goto(`/runs/system/${IDS.run}`);
    await expect(page.getByTestId('run-result-json')).toHaveCount(1);
    await expect(page.getByTestId('run-result-json')).toContainText('this run');
    await expect(page.getByTestId('run-result-meta')).toContainText('by version v1.0.0');
    await expect(page.getByTestId('run-other-results')).toContainText('2 results stored under this key by other runs are not shown');
  });

  test('a run with no result of its own says so, whatever other runs stored', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ status: 'Failed', source: WORKFLOW_SOURCE })));
    await page.route(`**/api/reactions/logs/system/${IDS.run}**`, (route) => json(route, UPDATES));
    await page.route(`**/api/files/results/${SHA}**`, (route) =>
      json(route, { results: { [RESULT_KEY]: [{ id: 'older', result: { from: 'an earlier run' }, uploaded: '2026-10-29T04:00:00Z' }] } }),
    );
    await page.goto(`/runs/system/${IDS.run}`);
    await expect(page.getByTestId('run-no-results')).toBeVisible();
    await expect(page.getByTestId('run-no-results')).toHaveText('This run stored no results on this input.');
    await expect(page.getByTestId('run-result-json')).toHaveCount(0);
    // the two notes are separate blocks, not one run-together line
    const [none, others] = await Promise.all(
      ['run-no-results', 'run-other-results'].map((id) => page.getByTestId(id).evaluate((el) => getComputedStyle(el).display)),
    );
    expect([none, others]).toEqual(['block', 'block']);
  });

  test('a completed run whose result was pruned says the result is not kept', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) =>
      json(route, makeRun({ status: 'Completed', source: WORKFLOW_SOURCE })),
    );
    await page.route(`**/api/reactions/logs/system/${IDS.run}**`, (route) => json(route, UPDATES));
    await page.route(`**/api/files/results/${SHA}**`, (route) =>
      json(route, { results: { [RESULT_KEY]: [{ id: 'newer', result: { from: 'a later run' }, uploaded: '2026-10-29T06:00:00Z' }] } }),
    );
    await page.goto(`/runs/system/${IDS.run}`);
    await expect(page.getByTestId('run-no-results')).toContainText('No result from this run is kept on this input');
    await expect(page.getByTestId('run-no-results')).not.toContainText('stored no results');
  });

  test('a run warns when several results fall inside its lifetime', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ status: 'Failed', source: WORKFLOW_SOURCE })));
    await page.route(`**/api/reactions/logs/system/${IDS.run}**`, (route) => json(route, UPDATES));
    await page.route(`**/api/files/results/${SHA}**`, (route) =>
      json(route, {
        results: {
          [RESULT_KEY]: [
            { id: 'fast', result: { from: 'an overlapping run' }, uploaded: '2026-10-29T05:00:10Z' },
            { id: 'own', result: { from: 'this run' }, uploaded: '2026-10-29T05:00:30Z' },
          ],
        },
      }),
    );
    await page.goto(`/runs/system/${IDS.run}`);
    await expect(page.getByTestId('run-result-json')).toHaveCount(2);
    await expect(page.getByTestId('run-overlapping-results')).toContainText('another run on this input may have stored some of them');
  });

  test('a run with one result in its lifetime shows no overlap warning', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ status: 'Failed', source: WORKFLOW_SOURCE })));
    await page.route(`**/api/reactions/logs/system/${IDS.run}**`, (route) => json(route, UPDATES));
    await page.route(`**/api/files/results/${SHA}**`, (route) =>
      json(route, { results: { [RESULT_KEY]: [{ id: 'own', result: { from: 'this run' }, uploaded: '2026-10-29T05:00:30Z' }] } }),
    );
    await page.goto(`/runs/system/${IDS.run}`);
    await expect(page.getByTestId('run-result-json')).toHaveCount(1);
    await expect(page.getByTestId('run-overlapping-results')).toHaveCount(0);
  });

  test('a failed run whose job never completed marks a result in its lifetime as possibly another run', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ status: 'Failed', source: WORKFLOW_SOURCE })));
    await page.route(`**/api/reactions/logs/system/${IDS.run}**`, (route) => json(route, UPDATES));
    await page.route(`**/api/files/results/${SHA}**`, (route) =>
      json(route, {
        results: { [RESULT_KEY]: [{ id: 'fast', result: { from: 'an overlapping run' }, uploaded: '2026-10-29T05:00:30Z' }] },
      }),
    );
    await page.goto(`/runs/system/${IDS.run}`);
    await expect(page.getByTestId('run-result-json')).toHaveCount(1);
    await expect(page.getByTestId('run-unconfirmed-results')).toContainText('This run failed, so it may not have stored the result below');
  });

  test('a completed run whose job completed shows its result without a caveat', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) =>
      json(route, makeRun({ status: 'Completed', source: WORKFLOW_SOURCE })),
    );
    const completed = [
      UPDATES[0],
      { ...UPDATES[0], action: 'JobCompleted', timestamp: '2026-10-29T05:00:40Z' },
      { ...UPDATES[1], action: 'ReactionCompleted' },
    ];
    await page.route(`**/api/reactions/logs/system/${IDS.run}**`, (route) => json(route, completed));
    await page.route(`**/api/files/results/${SHA}**`, (route) =>
      json(route, { results: { [RESULT_KEY]: [{ id: 'own', result: { from: 'this run' }, uploaded: '2026-10-29T05:00:30Z' }] } }),
    );
    await page.goto(`/runs/system/${IDS.run}`);
    await expect(page.getByTestId('run-result-json')).toHaveCount(1);
    await expect(page.getByTestId('run-unconfirmed-results')).toHaveCount(0);
  });

  test('a failed results read shows only the error, not a no-results note', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) =>
      json(route, makeRun({ status: 'Completed', source: WORKFLOW_SOURCE })),
    );
    await page.route(`**/api/reactions/logs/system/${IDS.run}**`, (route) => json(route, UPDATES));
    await page.route(`**/api/files/results/${SHA}**`, (route) =>
      route.fulfill({ status: 500, contentType: 'application/json', body: JSON.stringify({ error: 'uiloop injected results failure' }) }),
    );
    await page.goto(`/runs/system/${IDS.run}`);
    await expect(page.getByTestId('run-results-error')).toContainText('Get Run Results');
    await expect(page.getByTestId('run-no-results')).toHaveCount(0);
    await expect(page.getByTestId('run-results')).not.toContainText('pruned');
  });

  test('a failed status updates read shows only the error, not unfiltered results', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) =>
      json(route, makeRun({ status: 'Completed', source: WORKFLOW_SOURCE })),
    );
    await page.route(`**/api/reactions/logs/system/${IDS.run}**`, (route) =>
      route.fulfill({ status: 500, contentType: 'application/json', body: JSON.stringify({ error: 'uiloop injected updates failure' }) }),
    );
    await page.goto(`/runs/system/${IDS.run}`);
    await expect(page.getByTestId('run-results-updates-error')).toContainText('Get Run Status Updates');
    await expect(page.getByTestId('run-result-json')).toHaveCount(0);
    await expect(page.getByTestId('run-unconfirmed-results')).toHaveCount(0);
    await expect(page.getByTestId('run-overlapping-results')).toHaveCount(0);
  });

  test('the tags row leaves out the tags the run page shows elsewhere', async ({ page }) => {
    await mockAuth(page);
    const tags = ['mine=1', 'WorkflowRun', 'Workflow=w1', `WorkflowInstance=${IDS.workflowInstance}`, SHA, 'test'];
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ tags, source: WORKFLOW_SOURCE })));
    await page.goto(`/runs/system/${IDS.run}`);
    const summary = page.getByTestId('run-summary');
    await expect(summary).toContainText('mine=1');
    await expect(summary.getByText('WorkflowRun', { exact: true })).toHaveCount(0);
    await expect(summary.getByText('Workflow=w1')).toHaveCount(0);
  });

  test('the sub reactions of a run link to their own pages', async ({ page }) => {
    await mockAuth(page);
    const sub = makeRun({ id: 'abcdef00-0000-4000-8000-000000000000', pipeline: 'tool:ghidra v1.0.0', parent: IDS.run });
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) =>
      json(route, makeRun({ source: WORKFLOW_SOURCE, sub_reactions: 1, completed_sub_reactions: 1 })),
    );
    await page.route(`**/api/reactions/sub/system/${IDS.run}/details/**`, (route) => json(route, { details: [sub] }));
    await page.goto(`/runs/system/${IDS.run}`);
    const link = page.getByTestId('sub-run-row').getByRole('link', { name: 'tool:ghidra v1.0.0' });
    await expect(link).toHaveAttribute('href', `/runs/system/${sub.id}`);
  });

  test('the runs list pages on with Load more instead of dropping runs', async ({ page }) => {
    await mockAuth(page, { ...MOCK_USER, groups: ['a'] });
    const run = (id: string) => makeRun({ id, group: 'a', pipeline: `run-${id.slice(0, 2)}` });
    const requested: (string | null)[] = [];
    await page.route('**/api/reactions/tag/*/WorkflowRun/details/**', (route) => json(route, { details: [] }));
    await page.route('**/api/reactions/group/*/*/details/**', (route) => {
      const url = new URL(route.request().url());
      if (!url.pathname.includes('/Completed/')) return json(route, { details: [] });
      requested.push(url.searchParams.get('cursor'));
      // the first page of the Completed listing has a cursor, and the second is the last
      return url.searchParams.get('cursor') === '50'
        ? json(route, { details: [run('22000000-0000-4000-8000-000000000000')] })
        : json(route, { details: [run('11000000-0000-4000-8000-000000000000')], cursor: 50 });
    });
    await page.goto('/runs');
    await expect(page.getByTestId('run-row')).toHaveCount(1, { timeout: 15_000 });
    await expect(page.getByTestId('runs-more-note')).toBeVisible();
    await page.getByTestId('runs-load-more').click();
    await expect(page.getByTestId('run-row')).toHaveCount(2);
    await expect(page.getByTestId('runs-load-more')).toHaveCount(0);
    expect(requested).toContain('50');
  });

  test('a kind tab with none of the runs read so far does not claim there are no more runs', async ({ page }) => {
    await mockAuth(page, { ...MOCK_USER, groups: ['a'] });
    await page.route('**/api/reactions/tag/*/WorkflowRun/details/**', (route) => json(route, { details: [] }));
    await page.route('**/api/reactions/group/*/*/details/**', (route) => {
      const url = new URL(route.request().url());
      if (!url.pathname.includes('/Completed/')) return json(route, { details: [] });
      return json(route, { details: [makeRun({ id: '11000000-0000-4000-8000-000000000000', group: 'a' })], cursor: 50 });
    });
    await page.goto('/runs');
    await expect(page.getByTestId('run-row')).toHaveCount(1, { timeout: 15_000 });
    await page.getByRole('tab', { name: /Workflow/ }).click();
    await expect(page.getByTestId('run-row')).toHaveCount(0);
    await expect(page.getByTestId('no-runs')).toContainText('None of the 1 runs read so far are Workflow runs');
    await expect(page.getByTestId('runs-more-note')).toContainText('More runs exist than the 1 read so far; 0 of them are Workflow runs');
  });

  test('the provenance name cells stay on one line', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 900 });
    await mockAuth(page);
    // long hashes and image refs wrap anywhere and would squeeze the short name cells in an auto table layout
    const hash = 'a1b2c3d4'.repeat(8);
    const provenance = {
      agents: [{ agent: 'agent-id', name: 'jup-stub-agent-slow', config_hash: hash, toolset_digest: hash }],
      tools: [
        {
          tool: IDS.harnessTool,
          tool_instance: IDS.harnessInstance,
          name: 'jup-stub-harness@1.0.0',
          kind: 'Harness',
          version: '1.0.0',
          spec_hash: hash,
          image: `registry.example.com/team/${'long-image-name-'.repeat(4)}:1.0.0`,
        },
      ],
    };
    const source = { WorkflowRun: { ...WORKFLOW_SOURCE.WorkflowRun, provenance } };
    await page.route(`**/api/reactions/system/${IDS.run}`, (route) => json(route, makeRun({ source })));
    await page.goto(`/runs/system/${IDS.run}`);
    const names = page.getByTestId('run-provenance').locator('tbody tr td:first-child');
    await expect(names.first()).toBeVisible();
    for (const index of [...Array(await names.count()).keys()]) {
      const wrapped = await names.nth(index).evaluate((cell) => {
        // the name is the link when the cell holds one (a badge may follow it on the same row), else the cell's text wrapper
        const range = document.createRange();
        range.selectNodeContents(cell.querySelector('a') ?? cell.firstElementChild ?? cell);
        return range.getClientRects().length > 1;
      });
      expect(wrapped, `name cell ${index}`).toBe(false);
    }
  });

  test('an unfinished list that does not change backs off its reloads', async ({ page }) => {
    await mockAuth(page, { ...MOCK_USER, groups: ['a'] });
    let sweeps = 0;
    await page.route('**/api/reactions/tag/*/WorkflowRun/details/**', (route) => {
      sweeps += 1;
      return json(route, { details: [makeRun({ group: 'a', status: 'Created' })] });
    });
    await page.route('**/api/reactions/group/*/*/details/**', (route) => json(route, { details: [] }));
    await page.goto('/runs');
    await expect(page.getByTestId('run-row')).toHaveCount(1, { timeout: 15_000 });
    await page.waitForTimeout(500);
    const before = sweeps;
    // reloads at a flat 3 s would make seven in 20 s; backing off makes four or fewer (3, 4.5, 6.75, 10)
    await page.waitForTimeout(20_000);
    expect(sweeps - before).toBeLessThanOrEqual(4);
    expect(sweeps - before).toBeGreaterThanOrEqual(2);
  });
});
