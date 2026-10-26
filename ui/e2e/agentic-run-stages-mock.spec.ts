import fs from 'fs';
import path from 'path';
import { expect, test } from '@playwright/test';
import type { Page, Route } from '@playwright/test';

import { MOCK_USER } from './helpers';

// Mocked tests of the run stages, shared by the run details page and the single reaction status page: the
// stage diagram with a status node per stage, each stage's jobs and logs, readable status updates, tags,
// inputs, SLA, and the run kinds. Set RUN_STAGES_SHOTS to a directory to also save screenshots.

/** The run and the input file the mocks serve. */
const RUN = '77777777-7777-4777-8777-777777777777';
const SHA = 'c'.repeat(64);

/** Where screenshots go, when asked for. */
const SHOTS = process.env.RUN_STAGES_SHOTS ?? '';

/** Answer a route with a JSON body. */
function json(route: Route, body: unknown, status = 200) {
  return route.fulfill({ status, contentType: 'application/json', body: JSON.stringify(body) });
}

/** Sign in as a mocked user in the Light theme and answer every other API call with an empty object. */
async function mockAuth(page: Page) {
  await page.route('**/api/**', (route) => json(route, {}));
  await page.route('**/api/users/whoami', (route) => json(route, { ...MOCK_USER, settings: { theme: 'Light' } }));
  await page.context().addCookies([{ name: 'THORIUM_TOKEN', value: MOCK_USER.token, domain: 'localhost', path: '/' }]);
}

/** A three step pipeline: scan, then strings and yara in parallel, then report. */
const PLAN = {
  stages: [
    [{ name: 'scan', tool_instance: 't1' }],
    [
      { name: 'strings', tool_instance: 't2' },
      { name: 'yara', tool_instance: 't3' },
    ],
    [{ name: 'report', tool_instance: 't4' }],
  ],
};

/** The tools the pipeline's stages run. */
const PROVENANCE = {
  agents: [],
  tools: ['scan', 'strings', 'yara', 'report'].map((name, index) => ({
    tool: `tool-${name}`,
    tool_instance: `t${index + 1}`,
    name,
    kind: 'Image',
    version: '1.0.0',
    spec_hash: `h${index}`,
    image: `registry/${name}:1.0.0`,
  })),
};

/** A pipeline run with overrides. */
function makeRun(overrides: Record<string, unknown> = {}) {
  return {
    id: RUN,
    group: 'system',
    creator: 'test',
    pipeline: 'triage',
    status: 'Started',
    current_stage: 1,
    current_stage_progress: 0,
    current_stage_length: 2,
    args: {},
    sla: '2099-01-01T00:00:00Z',
    jobs: [],
    tags: [SHA, 'test', 'mytag'],
    sub_reactions: 0,
    completed_sub_reactions: 0,
    generators: [],
    samples: [SHA],
    ephemeral: [],
    parent_ephemeral: {},
    repos: [],
    has_cache: false,
    source: { Pipeline: { name: 'triage', provenance: PROVENANCE } },
    plan: PLAN,
    ...overrides,
  };
}

/** A status update a second after a fixed start. */
function update(action: unknown, values: Record<string, string>, second: number, msg: string | null = null) {
  return {
    group: 'system',
    pipeline: 'triage',
    reaction: RUN,
    action,
    timestamp: new Date(Date.parse('2026-10-29T10:00:00Z') + second * 1000).toISOString(),
    msg,
    update: values,
  };
}

/** A job created in a stage. */
const created = (job: string, stage: string, second: number) =>
  update('JobCreated', { job, id: job, stage, status: '"Created"', deadline: '2099-01-01 00:00:00.0 UTC' }, second);
/** The claim of a job by a worker. */
const claimed = (worker: string, second: number) => update('JobRunning', { worker, status: 'running' }, second);
/** A completed job. */
const completed = (job: string, second: number) => update('JobCompleted', { job, status: 'Completed' }, second);
/** A failed job. */
const failed = (job: string, second: number) => update('JobFailed', { job, status: 'Failed' }, second);

/** Scan done, strings queued, yara running: a mixed run. */
const MIXED = [
  created('j1', 'scan', 0),
  claimed('scan-1-0-0-a', 1),
  completed('j1', 2),
  created('j2', 'strings', 3),
  created('j3', 'yara', 3),
  claimed('yara-1-0-0-b', 4),
];

/** Serve a run, its updates, and its stage logs. */
async function serve(page: Page, run: Record<string, unknown>, updates: unknown[]) {
  await page.route(`**/api/reactions/system/${RUN}`, (route) => json(route, run));
  await page.route(`**/api/reactions/logs/system/${RUN}?**`, (route) => json(route, updates));
  await page.route(`**/api/reactions/logs/system/${RUN}/**`, (route) => {
    const stage = decodeURIComponent(new URL(route.request().url()).pathname.split('/').pop() ?? '');
    return json(route, { logs: [`${stage}: started`, `${stage}: working`] });
  });
}

/** Save a screenshot when asked for. */
async function shot(page: Page, name: string) {
  if (!SHOTS) return;
  fs.mkdirSync(SHOTS, { recursive: true });
  await page.waitForTimeout(400);
  await page.screenshot({ path: path.join(SHOTS, name), fullPage: true });
}

/** The status a stage node names in its accessible name. */
const node = (page: Page, stage: string) => page.getByTestId(`stage-node-${stage}`);

/** The two pages that share the stage sections. */
const PAGES: [string, string][] = [
  ['run', `/runs/system/${RUN}`],
  ['reaction', `/reaction/system/${RUN}`],
];

test.describe('run stages (mocked)', () => {
  for (const [name, url] of PAGES) {
    test(`a mixed run shows each stage's status on its node and below the diagram (${name} page)`, async ({ page }) => {
      await mockAuth(page);
      await serve(page, makeRun(), MIXED);
      await page.goto(url);
      await expect(node(page, 'scan')).toHaveAttribute('aria-label', 'scan: Completed');
      await expect(node(page, 'strings')).toHaveAttribute('aria-label', 'strings: Created');
      await expect(node(page, 'yara')).toHaveAttribute('aria-label', 'yara: Running');
      await expect(node(page, 'report')).toHaveAttribute('aria-label', 'report: Waiting');
      await expect(page.getByTestId('stage-status-yara')).toHaveText('Running');
      await expect(page.getByTestId('stage-jobs-yara')).toContainText('yara-1-0-0-b');
      // the summary and the page head name the run status in text beside its icon
      await expect(page.getByTestId('run-status')).toHaveText('Started');
      await expect(page.getByTestId('run-stage-text')).toHaveText('stage 2 of 3: 0 of 2 jobs done');
      // the user's tag shows; the creator and input tags the API adds do not
      await expect(page.getByTestId('run-tags')).toHaveText('mytag');
      await expect(page.getByTestId('run-input-samples')).toContainText(SHA);
      await expect(page.getByTestId('run-input-repos')).toHaveCount(0);
      for (const width of [1440, 1024]) {
        await page.setViewportSize({ width, height: 900 });
        const [scroll, client] = await page.evaluate(() => [document.documentElement.scrollWidth, document.documentElement.clientWidth]);
        expect(scroll, `no page overflow at ${width}`).toBeLessThanOrEqual(client);
        await shot(page, `after-${width}-${name}-pipeline-mixed.png`);
      }
    });

    test(`clicking a node selects its stage and opens its logs (${name} page)`, async ({ page }) => {
      await mockAuth(page);
      await serve(page, makeRun(), MIXED);
      await page.goto(url);
      await expect(page.getByTestId('stage-logs-strings')).toHaveCount(0);
      await node(page, 'strings').click();
      await expect(node(page, 'strings')).toHaveAttribute('aria-pressed', 'true');
      await expect(page.getByTestId('stage-logs-strings')).toContainText('strings: working');
      await expect(page.getByTestId('toggle-logs-strings')).toHaveText('Hide logs');
      // the keyboard selects a node too
      await node(page, 'report').focus();
      await page.keyboard.press('Enter');
      await expect(node(page, 'report')).toHaveAttribute('aria-pressed', 'true');
      await expect(node(page, 'strings')).toHaveAttribute('aria-pressed', 'false');
    });
  }

  test('a completed run shows every stage completed and the completed indicator', async ({ page }) => {
    await mockAuth(page);
    const updates = [
      ...MIXED,
      completed('j2', 5),
      completed('j3', 6),
      created('j4', 'report', 7),
      claimed('report-1-0-0-c', 8),
      completed('j4', 9),
    ];
    await serve(page, makeRun({ status: 'Completed', current_stage: 3 }), updates);
    await page.goto(`/runs/system/${RUN}`);
    for (const stage of ['scan', 'strings', 'yara', 'report'])
      await expect(node(page, stage)).toHaveAttribute('aria-label', `${stage}: Completed`);
    await expect(page.getByTestId('run-progress').getByRole('img', { name: 'Completed' })).toBeVisible();
    await expect(page.getByTestId('run-stage-text')).toHaveText('all 3 stages completed');
    await expect(page.getByTestId('run-cancel')).toHaveCount(0);
    for (const width of [1440, 1024]) {
      await page.setViewportSize({ width, height: 900 });
      await shot(page, `after-${width}-run-pipeline-completed.png`);
    }
  });

  test('a failed run opens the failed stage logs and marks what stopped and what was skipped', async ({ page }) => {
    await mockAuth(page);
    const updates = [...MIXED, failed('j2', 5), update('ReactionFailed', { status: 'Failed' }, 5)];
    await serve(page, makeRun({ status: 'Failed' }), updates);
    for (const [name, url] of PAGES) {
      await page.goto(url);
      await expect(node(page, 'strings')).toHaveAttribute('aria-label', 'strings: Failed');
      await expect(node(page, 'yara')).toHaveAttribute('aria-label', 'yara: Stopped');
      await expect(node(page, 'report')).toHaveAttribute('aria-label', 'report: Skipped');
      await expect(page.getByTestId('stage-logs-strings')).toBeVisible();
      await expect(page.getByTestId('run-stage-text')).toHaveText('failed at stage 2 of 3');
      for (const width of [1440, 1024]) {
        await page.setViewportSize({ width, height: 900 });
        await shot(page, `after-${width}-${name}-pipeline-failed.png`);
      }
    }
  });

  test('a running run updates its nodes as the polls read new updates, without moving them', async ({ page }) => {
    await mockAuth(page);
    let updates = MIXED.slice(0, 5);
    await page.route(`**/api/reactions/system/${RUN}`, (route) => json(route, makeRun()));
    await page.route(`**/api/reactions/logs/system/${RUN}?**`, (route) => json(route, updates));
    await page.goto(`/runs/system/${RUN}`);
    await expect(node(page, 'yara')).toHaveAttribute('aria-label', 'yara: Created');
    const before = await node(page, 'yara').boundingBox();
    updates = MIXED;
    await expect(node(page, 'yara')).toHaveAttribute('aria-label', 'yara: Running', { timeout: 10_000 });
    const after = await node(page, 'yara').boundingBox();
    expect(after).toEqual(before);
  });

  test('a pipeline with many parallel images stays inside the page at 1024', async ({ page }) => {
    await mockAuth(page);
    const wide = Array.from({ length: 12 }, (_, index) => ({ name: `analyzer-number-${index}`, tool_instance: `w${index}` }));
    await serve(
      page,
      makeRun({
        plan: { stages: [[PLAN.stages[0][0]], wide, [PLAN.stages[2][0]]] },
        source: { Pipeline: { name: 'triage', provenance: { agents: [], tools: [] } } },
      }),
      [created('j1', 'scan', 0), completed('j1', 1), ...wide.map((stage, index) => created(`w${index}`, stage.name, 2))],
    );
    await page.setViewportSize({ width: 1024, height: 900 });
    await page.goto(`/runs/system/${RUN}`);
    await expect(page.getByTestId('stage-detail')).toHaveCount(14);
    const [scroll, client] = await page.evaluate(() => [document.documentElement.scrollWidth, document.documentElement.clientWidth]);
    expect(scroll).toBeLessThanOrEqual(client);
    await shot(page, 'after-1024-run-pipeline-wide.png');
  });

  test('status updates read as labelled fields, with the raw JSON one click away', async ({ page }) => {
    await mockAuth(page);
    await serve(page, makeRun({ status: 'Failed' }), [
      ...MIXED,
      update('JobFailed', { job: 'j2', status: 'Failed' }, 5, '{"error":"<b>boom</b>","counts":{"done":1,"total":2}}'),
    ]);
    await page.goto(`/runs/system/${RUN}`);
    const row = page.getByTestId('run-update-row').last();
    await expect(row.getByTestId('update-field-error')).toContainText('<b>boom</b>');
    await expect(row.getByTestId('update-field-counts')).toContainText('total');
    await row.getByTestId('update-raw-toggle').click();
    await expect(row.getByTestId('update-raw')).toContainText('"total": 2');
    await page.getByTestId('run-updates').scrollIntoViewIfNeeded();
    await shot(page, 'after-1440-run-updates-raw.png');
  });

  test('an overdue SLA of a working run is marked, with the full time as its tooltip', async ({ page }) => {
    await mockAuth(page);
    await serve(page, makeRun({ sla: '2026-01-01T00:00:00Z' }), MIXED);
    await page.goto(`/runs/system/${RUN}`);
    const sla = page.getByTestId('run-sla');
    await expect(sla).toHaveAttribute('data-overdue', 'true');
    await expect(sla).toHaveAttribute('title', '2026-01-01T00:00:00.000Z');
    await expect(sla).toContainText('overdue');
  });

  test('a run with no inputs says so in one row', async ({ page }) => {
    await mockAuth(page);
    await serve(page, makeRun({ samples: [], tags: [] }), MIXED);
    await page.goto(`/runs/system/${RUN}`);
    await expect(page.getByTestId('run-no-inputs')).toHaveText('No inputs');
    await expect(page.getByTestId('run-input-samples')).toHaveCount(0);
  });
});

test.describe('run kinds on the run details page (mocked)', () => {
  const tool = { Tool: { tool_instance: 't1', provenance: { agents: [], tools: [PROVENANCE.tools[0]] } } };

  test('a run of one tool shows its logs open, with no plan or diagram', async ({ page }) => {
    await mockAuth(page);
    await serve(
      page,
      makeRun({ source: tool, status: 'Completed', current_stage: 1, plan: { stages: [[PLAN.stages[0][0]]] } }),
      MIXED.slice(0, 3),
    );
    await page.goto(`/runs/system/${RUN}`);
    await expect(page.getByTestId('run-stage-logs')).toBeVisible();
    await expect(page.getByTestId('stage-logs-scan')).toContainText('scan: working');
    await expect(page.getByTestId('run-plan')).toHaveCount(0);
    await expect(page.getByTestId('run-stage-diagram')).toHaveCount(0);
    await expect(page.getByTestId('run-provenance').getByRole('heading', { name: 'Versions used' })).toBeVisible();
    await expect(page.getByTestId('run-versions-intro')).toHaveText('The exact tool versions this run used.');
    for (const width of [1440, 1024]) {
      await page.setViewportSize({ width, height: 900 });
      await shot(page, `after-${width}-run-tool.png`);
    }
  });

  test('a run of several tools keeps the plan', async ({ page }) => {
    await mockAuth(page);
    await serve(page, makeRun({ source: tool }), MIXED);
    await page.goto(`/runs/system/${RUN}`);
    await expect(page.getByTestId('run-plan')).toBeVisible();
    await expect(page.getByTestId('run-stage-diagram')).toHaveCount(0);
    await shot(page, 'after-1440-run-multi-tool.png');
  });

  test('a workflow run keeps the plan', async ({ page }) => {
    await mockAuth(page);
    const provenance = {
      agents: [{ agent: 'a1', name: 'triage-agent', config_hash: 'c', toolset_digest: 'd' }],
      tools: [{ ...PROVENANCE.tools[0], kind: 'Harness' }],
    };
    const workflow = {
      WorkflowRun: { workflow_instance: 'wi', digest: 'd', agent: 'a1', harness: 't1', result_key: 'triage#r1~wi', models: [], provenance },
    };
    await serve(
      page,
      makeRun({ source: workflow, status: 'Completed', current_stage: 1, plan: { stages: [[PLAN.stages[0][0]]] } }),
      MIXED.slice(0, 3),
    );
    await page.goto(`/runs/system/${RUN}`);
    await expect(page.getByTestId('run-plan')).toBeVisible();
    await expect(page.getByTestId('run-stage-diagram')).toHaveCount(0);
    await expect(page.getByTestId('run-versions-intro')).toHaveText('The exact agent and harness versions this run used.');
    for (const width of [1440, 1024]) {
      await page.setViewportSize({ width, height: 900 });
      await shot(page, `after-${width}-run-workflow.png`);
    }
  });
});

test.describe('the single reaction status page (mocked)', () => {
  test('keeps its own fields and deletes after a confirm', async ({ page }) => {
    await mockAuth(page);
    await serve(page, makeRun({ status: 'Completed', current_stage: 3, generators: ['g1'], ephemeral: ['notes.txt'] }), MIXED);
    let deleted = false;
    await page.route(`**/api/reactions/system/${RUN}`, (route) => {
      if (route.request().method() === 'DELETE') {
        deleted = true;
        return route.fulfill({ status: 204, body: '' });
      }
      return json(route, makeRun({ status: 'Completed', current_stage: 3, generators: ['g1'], ephemeral: ['notes.txt'] }));
    });
    await page.goto(`/reaction/system/${RUN}`);
    await expect(page.getByTestId('reaction-head-status')).toHaveText('Completed');
    await expect(page.getByTestId('reaction-generators')).toContainText('g1');
    await expect(page.getByTestId('reaction-ephemeral')).toHaveText('notes.txt');
    await expect(page.getByTestId('run-banner').getByRole('link', { name: 'Open it in the run view' })).toHaveAttribute(
      'href',
      `/runs/system/${RUN}`,
    );
    for (const width of [1440, 1024]) {
      await page.setViewportSize({ width, height: 900 });
      await shot(page, `after-${width}-reaction-pipeline-completed.png`);
    }
    await page.getByTestId('reaction-delete').click();
    expect(deleted).toBe(false);
    await page.getByTestId('reaction-delete-confirm').click();
    await expect(page.getByTestId('reaction-deleted')).toBeVisible();
    expect(deleted).toBe(true);
    await expect(page.getByTestId('reaction-deleted').getByRole('link', { name: SHA })).toHaveAttribute('href', `/file/${SHA}`);
  });
});

test.describe('the runs list (mocked)', () => {
  test('shows status indicators and readable SLA times', async ({ page }) => {
    await mockAuth(page);
    const runs = [
      makeRun({ status: 'Completed', current_stage: 3 }),
      makeRun({ id: '88888888-8888-4888-8888-888888888888', sla: '2026-01-01T00:00:00Z' }),
    ];
    await page.route('**/api/reactions/tag/**', (route) => json(route, { details: [] }));
    await page.route('**/api/reactions/group/**', (route) => {
      const pathname = new URL(route.request().url()).pathname;
      if (pathname.includes('/Completed/')) return json(route, { details: [runs[0]] });
      return json(route, { details: pathname.includes('/Started/') ? [runs[1]] : [] });
    });
    await page.goto('/runs');
    const rows = page.getByTestId('run-row');
    await expect(rows.first()).toBeVisible();
    await expect(page.getByTestId('runs-table').locator('[data-status="Completed"]').first()).toBeVisible();
    await expect(page.getByTestId('runs-table').locator('time[data-overdue="true"]')).toHaveCount(1);
    for (const width of [1440, 1024]) {
      await page.setViewportSize({ width, height: 900 });
      await shot(page, `after-${width}-runs-list.png`);
    }
  });
});
