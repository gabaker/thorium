import React, { ReactElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { MemoryRouter } from 'react-router-dom';
import { ServerStyleSheet } from 'styled-components';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { describe, expect, it } from 'vitest';

// project imports
import { RunSections } from './RunDetails';
import { ReactionSections } from '../reactions/ReactionStatus';
import { stageDecorations } from '@components/agentic/runs/RunStages';
import { buildStageViews } from '@components/agentic/runs/stages';
import { UpdatesContent } from '@components/agentic/runs/RunUpdatesPanel';
import { reactionLogsKey, reactionUpdatesKey } from '@utilities/queries/runs';
import { Reaction, ReactionStatus, StatusUpdate } from '@models/reactions';
import { ReactionSource } from '@models/runs';

/** The input file of the fixture runs. */
const SHA = 'b'.repeat(64);

/** A pipeline run with three steps, a user tag, and the API's automatic creator and input tags. */
const PIPELINE_RUN: Reaction = {
  id: '77777777-7777-4777-8777-777777777777',
  group: 'system',
  creator: 'test',
  pipeline: 'triage',
  status: ReactionStatus.Failed,
  current_stage: 1,
  current_stage_progress: 0,
  current_stage_length: 2,
  args: {},
  sla: '2026-11-05T20:26:06Z',
  jobs: ['j1', 'j2', 'j3'],
  tags: [SHA, 'test', 'mytag'],
  sub_reactions: 0,
  completed_sub_reactions: 0,
  generators: [],
  samples: [SHA],
  ephemeral: [],
  parent_ephemeral: {},
  repos: [],
  has_cache: false,
  source: { Pipeline: { name: 'triage', provenance: { agents: [], tools: [] } } },
  plan: {
    stages: [
      [{ name: 'scan', tool_instance: 't1' }],
      [
        { name: 'strings', tool_instance: 't2' },
        { name: 'yara', tool_instance: 't3' },
      ],
      [{ name: 'report', tool_instance: 't4' }],
    ],
  },
};

/** A status update of the fixture run. */
const update = (action: string, values: Record<string, string>, second: number): StatusUpdate => ({
  group: 'system',
  pipeline: 'triage',
  reaction: PIPELINE_RUN.id,
  action,
  timestamp: new Date(Date.parse('2026-10-29T10:00:00Z') + second * 1000).toISOString(),
  update: values,
});

/** The fixture run's updates: scan completed, strings failed while yara ran. */
const UPDATES: StatusUpdate[] = [
  update('JobCreated', { job: 'j1', stage: 'scan', status: '"Created"' }, 0),
  update('JobCompleted', { job: 'j1', status: 'Completed' }, 1),
  update('JobCreated', { job: 'j2', stage: 'strings', status: '"Created"' }, 2),
  update('JobCreated', { job: 'j3', stage: 'yara', status: '"Created"' }, 2),
  update('JobRunning', { worker: 'yara-1-0-0-abc', status: 'running' }, 3),
  update('JobFailed', { job: 'j2', status: 'Failed' }, 4),
  update('ReactionFailed', { status: 'Failed' }, 4),
];

/**
 * Render a page section from a seeded query cache, as the page draws it once its reads finish.
 *
 * @param element - The element to render.
 * @param run - The run whose updates are seeded.
 * @param updates - The run's status updates.
 * @returns The markup and the collected styles.
 */
function render(element: ReactElement, run: Reaction, updates: StatusUpdate[] = UPDATES): { markup: string; css: string } {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  client.setQueryData(reactionUpdatesKey(run.group, run.id), updates);
  for (const stage of (run.plan?.stages ?? []).flat())
    client.setQueryData(reactionLogsKey(run.group, run.id, stage.name), [`${stage.name} log line`]);
  const sheet = new ServerStyleSheet();
  try {
    const markup = renderToStaticMarkup(
      sheet.collectStyles(
        <QueryClientProvider client={client}>
          <MemoryRouter>{element}</MemoryRouter>
        </QueryClientProvider>,
      ),
    );
    return { markup, css: sheet.getStyleTags() };
  } finally {
    sheet.seal();
  }
}

/**
 * Tell whether the table with a test id ends without a line under its last row.
 *
 * @param markup - The rendered markup.
 * @param css - The collected styles.
 * @param testId - The table's test id.
 * @returns Whether one of the table's classes drops the last row's bottom border.
 */
function tableEndsClean(markup: string, css: string, testId: string): boolean {
  const tag = new RegExp(`<table[^>]*data-testid="${testId}"[^>]*>`).exec(markup)?.[0] ?? '';
  const classes = /class="([^"]+)"/.exec(tag)?.[1]?.split(' ') ?? [];
  return classes.some((name) => css.includes(`.${name} tbody tr:last-child td{border-bottom:none;}`));
}

/** The test ids of the sections a markup holds, in order. */
const sections = (markup: string) => [...markup.matchAll(/<section[^>]*data-testid="([^"]+)"/g)].map((match) => match[1]);

describe('the run details and single reaction status pages', () => {
  it('render the same shared sections from the same reaction', () => {
    const details = sections(render(<RunSections run={PIPELINE_RUN} />, PIPELINE_RUN).markup);
    const single = sections(render(<ReactionSections reaction={PIPELINE_RUN} />, PIPELINE_RUN).markup);
    expect(details).toEqual(['run-summary', 'run-stages', 'run-results', 'run-source', 'run-provenance', 'run-updates']);
    expect(single).toEqual(['run-summary', 'run-stages', 'run-updates']);
    // every section the single reaction status page shows is a shared one the run page shows too
    for (const id of single) expect(details).toContain(id);
  });
  it('draw the stage diagram and the same stage details and jobs on both pages', () => {
    for (const markup of [
      render(<RunSections run={PIPELINE_RUN} />, PIPELINE_RUN).markup,
      render(<ReactionSections reaction={PIPELINE_RUN} />, PIPELINE_RUN).markup,
    ]) {
      expect(markup).toContain('data-testid="run-stage-diagram"');
      expect([...markup.matchAll(/data-testid="stage-detail" data-stage="([^"]+)"/g)].map((match) => match[1])).toEqual([
        'scan',
        'strings',
        'yara',
        'report',
      ]);
      expect(markup).toMatch(/data-testid="stage-status-strings"[^>]*data-status="Failed"/);
      expect(markup).toMatch(/data-testid="stage-status-yara"[^>]*data-status="Stopped"/);
      expect(markup).toMatch(/data-testid="stage-status-report"[^>]*data-status="Skipped"/);
      // a failed run opens the logs of its failed and stopped stages
      expect(markup).toContain('data-testid="stage-logs-strings"');
      expect(markup).not.toContain('data-testid="stage-logs-scan"');
      expect(markup).toContain('href="/reactions/logs/system/77777777-7777-4777-8777-777777777777/strings"');
    }
  });
  it('show only the tags a user gave, with the creator and the input labelled', () => {
    for (const markup of [
      render(<RunSections run={PIPELINE_RUN} />, PIPELINE_RUN).markup,
      render(<ReactionSections reaction={PIPELINE_RUN} />, PIPELINE_RUN).markup,
    ]) {
      const tags = /data-testid="run-tags"[^]*?<\/div>/.exec(markup)?.[0] ?? '';
      expect(tags).toContain('mytag');
      expect(tags).not.toContain(SHA);
      expect(tags).not.toContain('>test<');
      expect(markup).toMatch(/<dt>Creator<\/dt><dd data-testid="run-creator">test<\/dd>/);
      expect(markup).toContain(`href="/file/${SHA}"`);
    }
  });
  it('show the run status as an indicator instead of a progress bar', () => {
    const { markup } = render(<RunSections run={{ ...PIPELINE_RUN, status: ReactionStatus.Completed, current_stage: 3 }} />, PIPELINE_RUN);
    expect(markup).toMatch(/data-testid="run-progress"><span[^>]*role="img" aria-label="Completed"/);
    expect(markup).toContain('all 3 stages completed');
    // the bar's fill was the only element sized inline
    expect(markup).not.toMatch(/data-testid="run-progress"[^>]*style=/);
  });
  it('hide empty input rows and say No inputs only when there are none', () => {
    const repoOnly = render(
      <ReactionSections reaction={{ ...PIPELINE_RUN, samples: [], repos: [{ url: 'github.com/a/b' }] }} />,
      PIPELINE_RUN,
    ).markup;
    expect(repoOnly).toContain('data-testid="run-input-repos"');
    expect(repoOnly).not.toContain('data-testid="run-input-samples"');
    expect(repoOnly).not.toContain('No inputs');
    const fileOnly = render(<RunSections run={PIPELINE_RUN} />, PIPELINE_RUN).markup;
    expect(fileOnly).toContain('data-testid="run-input-samples"');
    expect(fileOnly).not.toContain('Input repos');
    const none = render(<RunSections run={{ ...PIPELINE_RUN, samples: [], tags: [] }} />, PIPELINE_RUN).markup;
    expect(none).toContain('data-testid="run-no-inputs"');
    expect(none).not.toContain('Input files');
  });
  it('keep the single reaction status page fields only it shows', () => {
    const { markup } = render(
      <ReactionSections
        reaction={{
          ...PIPELINE_RUN,
          args: { scan: { positionals: ['-v'], kwargs: {}, switches: [], opts: { override_positionals: false, override_kwargs: false } } },
          generators: ['g1'],
          ephemeral: ['e.txt'],
        }}
      />,
      PIPELINE_RUN,
    );
    expect(markup).toContain('data-testid="reaction-args"');
    expect(markup).toContain('data-testid="reaction-generators"');
    expect(markup).toContain('data-testid="reaction-ephemeral"');
  });
});

describe('run kinds on the run details page', () => {
  const provenance = {
    agents: [],
    tools: [{ tool: 'tool-id', tool_instance: 't1', name: 'scan', kind: 'Image' as never, version: '1.0.0', spec_hash: 'h' }],
  };
  const toolRun = (stages: string[][]): Reaction => ({
    ...PIPELINE_RUN,
    status: ReactionStatus.Completed,
    current_stage: stages.length,
    source: { Tool: { tool_instance: 't1', provenance } },
    plan: { stages: stages.map((step) => step.map((name) => ({ name, tool_instance: 't1' }))) },
  });
  const workflow: ReactionSource = {
    WorkflowRun: { workflow_instance: 'w', digest: 'd', agent: 'a', harness: 't1', result_key: 'k', models: [], provenance },
  };
  it('shows a pipeline run as a diagram and no plan', () => {
    const { markup } = render(<RunSections run={PIPELINE_RUN} />, PIPELINE_RUN);
    expect(markup).toContain('data-testid="run-stage-diagram"');
    expect(markup).not.toContain('data-testid="run-plan"');
  });
  it('keeps the plan for a workflow run', () => {
    const run = { ...PIPELINE_RUN, source: workflow, plan: { stages: [[{ name: 'harness', tool_instance: 't1' }]] } };
    const { markup } = render(<RunSections run={run} />, run, []);
    expect(markup).toContain('data-testid="run-plan"');
    expect(markup).not.toContain('data-testid="run-stage-diagram"');
  });
  it('keeps the plan for a run of several tools', () => {
    const run = toolRun([['scan'], ['report']]);
    const { markup } = render(<RunSections run={run} />, run, []);
    expect(markup).toContain('data-testid="run-plan"');
  });
  it('shows a run of one tool without a plan or diagram, with its logs still on the page', () => {
    const run = toolRun([['scan']]);
    const { markup } = render(<RunSections run={run} />, run, []);
    expect(markup).not.toContain('data-testid="run-plan"');
    expect(markup).not.toContain('data-testid="run-stage-diagram"');
    expect(markup).toContain('data-testid="run-stage-logs"');
    expect(markup).toContain('data-testid="stage-logs-scan"');
  });
  it('ends the Versions used card of a run of one tool without a line under its last row', () => {
    const run = toolRun([['scan']]);
    const { markup, css } = render(<RunSections run={run} />, run, []);
    expect(markup).toContain('<h2>Versions used</h2>');
    expect(markup).toContain('The exact tool versions this run used.');
    expect(tableEndsClean(markup, css, 'provenance-tools')).toBe(true);
  });
  it('says so when a run recorded no versions', () => {
    const run = { ...PIPELINE_RUN, source: undefined };
    expect(render(<RunSections run={run} />, run).markup).toContain('No versions were recorded for this run.');
  });
});

describe('the status updates card', () => {
  /** Render the updates body with its styles. */
  const renderUpdates = (updates: StatusUpdate[] | null) => {
    const sheet = new ServerStyleSheet();
    try {
      const markup = renderToStaticMarkup(sheet.collectStyles(<UpdatesContent updates={updates} error="" />));
      return { markup, css: sheet.getStyleTags() };
    } finally {
      sheet.seal();
    }
  };
  it('ends without a line under its last update', () => {
    const { markup, css } = renderUpdates(UPDATES);
    expect(markup).toContain('data-testid="run-update-row"');
    const classes = /<table class="([^"]+)"/.exec(markup)?.[1]?.split(' ') ?? [];
    expect(classes.some((name) => css.includes(`.${name} tbody tr:last-child td{border-bottom:none;}`))).toBe(true);
  });
  it('draws no table, and so no line, when there are no updates', () => {
    expect(renderUpdates([]).markup).not.toContain('<table');
  });
  it('puts a Show more control after the table when there are many updates', () => {
    const many = Array.from({ length: 105 }, (_, index) => update('JobCreated', { job: `j${index}`, stage: 'scan' }, index));
    const { markup } = renderUpdates(many);
    expect([...markup.matchAll(/data-testid="run-update-row"/g)]).toHaveLength(100);
    expect(markup).toContain('Show 5 more updates');
    expect(markup.indexOf('run-updates-more')).toBeGreaterThan(markup.indexOf('</table>'));
  });
});

describe('stageDecorations', () => {
  it('decorates each node with its status, selection, and the running step', () => {
    const run = { ...PIPELINE_RUN, status: ReactionStatus.Started, current_stage: 1 };
    const decorations = stageDecorations(buildStageViews(run, [['scan'], ['strings', 'yara'], ['report']], UPDATES.slice(0, 5)), 'yara');
    expect(decorations.yara).toMatchObject({
      label: 'yara: Running',
      selected: true,
      active: true,
      testId: 'stage-node-yara',
      accent: 'var(--thorium-badge-active, var(--thorium-link-text))',
    });
    expect(decorations.scan).toMatchObject({ label: 'scan: Completed', selected: false, active: false });
    expect(decorations.report.label).toBe('report: Waiting');
  });
});
