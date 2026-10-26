import { describe, expect, it } from 'vitest';

// project imports
import {
  buildSampleForm,
  inputsProblem,
  isRunFinished,
  isUnknownStage,
  noResultsMessage,
  noStageLogsMessage,
  parseRepoInput,
  parseWorkflowResultKey,
  plannedStageNames,
  promptFile,
  ReactionReadView,
  reactionReadView,
  reactionPath,
  resultsDuringRun,
  resultDisplayName,
  runKind,
  runProvenance,
  runResultKey,
  runResultsConfirmed,
  runWindow,
  sampleHashProblem,
  splitEntries,
  splitPlainEntries,
  userRunTags,
} from './runs';
import { CommitishKinds, ReactionStatus } from '@models/reactions';
import { ReactionPlan, ReactionSource, RunKind, RunProvenance } from '@models/runs';

const PROVENANCE: RunProvenance = { agents: [], tools: [] };
const PLAN: ReactionPlan = {
  stages: [
    [{ name: 'a', tool_instance: 'i1' }],
    [
      { name: 'b', tool_instance: 'i2' },
      { name: 'c', tool_instance: 'i3' },
    ],
  ],
};
const WORKFLOW: ReactionSource = {
  WorkflowRun: { workflow_instance: 'w', digest: 'd', agent: 'a', harness: 'h', result_key: 'wf#r1~w', models: [], provenance: PROVENANCE },
};
const TOOL: ReactionSource = { Tool: { tool_instance: 'i1', provenance: PROVENANCE } };
const PIPELINE: ReactionSource = { Pipeline: { name: 'p', provenance: PROVENANCE } };

describe('run status', () => {
  it('finishes on Completed and Failed only', () => {
    expect(isRunFinished(ReactionStatus.Completed)).toBe(true);
    expect(isRunFinished(ReactionStatus.Failed)).toBe(true);
    expect(isRunFinished(ReactionStatus.Created)).toBe(false);
    expect(isRunFinished(ReactionStatus.Started)).toBe(false);
  });
});

describe('run source', () => {
  it('names the kind of run', () => {
    expect(runKind({ source: WORKFLOW })).toBe(RunKind.Workflow);
    expect(runKind({ source: TOOL })).toBe(RunKind.Tool);
    expect(runKind({ source: PIPELINE })).toBe(RunKind.Pipeline);
    expect(runKind({})).toBe(RunKind.Pipeline);
  });
  it('reads the provenance of each kind', () => {
    expect(runProvenance({ source: WORKFLOW })).toBe(PROVENANCE);
    expect(runProvenance({ source: TOOL })).toBe(PROVENANCE);
    expect(runProvenance({ source: PIPELINE })).toBe(PROVENANCE);
    expect(runProvenance({})).toBeNull();
  });
  it('finds the result key', () => {
    expect(runResultKey({ source: WORKFLOW, plan: PLAN })).toBe('wf#r1~w');
    expect(runResultKey({ source: TOOL, plan: PLAN })).toBe('a');
    expect(runResultKey({ source: TOOL })).toBeNull();
    expect(runResultKey({ source: PIPELINE, plan: PLAN })).toBeNull();
  });
  it('lists the planned stages in order', () => {
    expect(plannedStageNames({ plan: PLAN })).toEqual(['a', 'b', 'c']);
    expect(plannedStageNames({})).toEqual([]);
  });
  it('splits a workflow result key', () => {
    expect(parseWorkflowResultKey('malware-triage#r3~abc-1')).toEqual({ name: 'malware-triage', revision: 3, instance: 'abc-1' });
    expect(parseWorkflowResultKey('strings')).toBeNull();
  });
  it('shows a workflow result key as its instance name and leaves a tool name alone', () => {
    expect(resultDisplayName('demo-workflow#r1~6a30751d-cbc3-447e-9f55-795a6dc29dea')).toBe('demo-workflow#r1');
    expect(resultDisplayName('strings')).toBe('strings');
  });
  it('opens tool and workflow runs in the run view and pipeline reactions in the reaction page', () => {
    const base = { group: 'minerva', id: 'r-1' };
    const provenance = { agents: [], tools: [] };
    expect(reactionPath({ ...base, source: { Tool: { tool_instance: 'i', provenance } } })).toBe('/runs/minerva/r-1');
    expect(reactionPath({ ...base, source: { Pipeline: { name: 'p', provenance } } })).toBe('/reaction/minerva/r-1');
    expect(reactionPath(base)).toBe('/reaction/minerva/r-1');
  });
});

describe('run inputs', () => {
  it('splits entries on lines and commas and skips comments', () => {
    expect(splitEntries('a, b\n\n# skip\n c ')).toEqual(['a', 'b', 'c']);
  });
  it('splits plain entries and keeps entries that start with #', () => {
    expect(splitPlainEntries('#a, b\n\n c ')).toEqual(['#a', 'b', 'c']);
  });
  it('reads a repo with and without a commitish', () => {
    expect(parseRepoInput('')).toBeNull();
    expect(parseRepoInput('github.com/org/repo')).toEqual({ url: 'github.com/org/repo' });
    expect(parseRepoInput('github.com/org/repo:main')).toEqual({
      url: 'github.com/org/repo',
      commitish: 'main',
      kind: CommitishKinds.Branch,
    });
    expect(parseRepoInput('github.com/org/repo:abc123', CommitishKinds.Commit)).toEqual({
      url: 'github.com/org/repo',
      commitish: 'abc123',
      kind: CommitishKinds.Commit,
    });
  });
  it('keeps a scheme or port colon in the url', () => {
    expect(parseRepoInput('https://github.com/org/repo')).toEqual({ url: 'https://github.com/org/repo' });
    expect(parseRepoInput('https://host:8443/org/repo:v1')).toEqual({
      url: 'https://host:8443/org/repo',
      commitish: 'v1',
      kind: CommitishKinds.Branch,
    });
    expect(parseRepoInput('github.com/org/repo:')).toEqual({ url: 'github.com/org/repo' });
  });
  it('refuses a run with no input', () => {
    expect(inputsProblem([], [], false)).toMatch(/at least one input/);
    expect(inputsProblem([], [], true)).toBeNull();
    expect(inputsProblem(['abc'], [], false)).toBeNull();
    expect(inputsProblem([], [{ url: 'u' }], false)).toBeNull();
  });
  it('refuses typed sample hashes that are not SHA256s', () => {
    expect(sampleHashProblem([])).toBeNull();
    expect(sampleHashProblem(['a'.repeat(64), 'B'.repeat(64)])).toBeNull();
    expect(sampleHashProblem(['a'.repeat(64), 'nothex'])).toMatch(/"nothex" is not a sample SHA256/);
    expect(sampleHashProblem(['g'.repeat(64)])).toMatch(/64 hex characters/);
  });
  it('turns a prompt into a text file', async () => {
    const file = promptFile('hello');
    expect(file.name).toBe('prompt.txt');
    expect(file.type).toBe('text/plain');
    expect(await file.text()).toBe('hello');
  });
  it('builds the sample upload form', () => {
    const form = buildSampleForm(promptFile('x'), ['g1', 'g2'], ' note ');
    expect(form.getAll('groups')).toEqual(['g1', 'g2']);
    expect(form.get('description')).toBe('note');
    expect(form.get('data')).toBeInstanceOf(File);
    expect(buildSampleForm(promptFile('x'), ['g']).has('description')).toBe(false);
  });
});

describe('run result window', () => {
  const updates = [
    { action: 'ReactionCreated', timestamp: '2026-10-29T05:00:00Z' },
    { action: 'JobRunning', timestamp: '2026-10-29T05:00:10Z' },
    { action: 'ReactionFailed', timestamp: '2026-10-29T05:01:00Z' },
  ];
  it('reads when a run was created and ended', () => {
    expect(runWindow(updates)).toEqual({ start: Date.parse('2026-10-29T05:00:00Z'), end: Date.parse('2026-10-29T05:01:00Z') });
    expect(runWindow(updates.slice(0, 2)).end).toBeNull();
    expect(runWindow([])).toEqual({ start: null, end: null });
  });
  it('keeps only the results stored while the run was alive', () => {
    const before = { id: 'before', uploaded: '2026-10-29T04:59:00Z' };
    const during = { id: 'during', uploaded: '2026-10-29T05:00:30Z' };
    const after = { id: 'after', uploaded: '2026-10-29T05:05:00Z' };
    expect(resultsDuringRun([after, during, before], runWindow(updates)).map((out) => out.id)).toEqual(['during']);
    // a run still working has no end, so later results are kept
    expect(resultsDuringRun([after, during, before], runWindow(updates.slice(0, 2))).map((out) => out.id)).toEqual(['after', 'during']);
  });
  it('keeps every result when the lifetime or the upload time is unknown', () => {
    const results: { id: string; uploaded?: string }[] = [{ id: 'a', uploaded: '2020-01-01T00:00:00Z' }, { id: 'b' }];
    expect(resultsDuringRun(results, { start: null, end: null })).toHaveLength(2);
    const lone: { id: string; uploaded?: string }[] = [{ id: 'b' }];
    expect(resultsDuringRun(lone, runWindow(updates))).toHaveLength(1);
  });
});

describe('runResultsConfirmed', () => {
  const ran = [{ action: 'ReactionCreated' }, { action: 'JobRunning' }];
  it('trusts a run whose job completed', () => {
    expect(runResultsConfirmed(ReactionStatus.Completed, [...ran, { action: 'JobCompleted' }, { action: 'ReactionCompleted' }])).toBe(true);
  });
  it('does not trust a run cancelled before its job completed', () => {
    expect(runResultsConfirmed(ReactionStatus.Failed, [...ran, { action: 'ReactionFailed' }])).toBe(false);
  });
  it('does not trust a failed run or a run whose updates could not be read', () => {
    expect(runResultsConfirmed(ReactionStatus.Failed, [...ran, { action: 'JobCompleted' }, { action: 'ReactionFailed' }])).toBe(false);
    expect(runResultsConfirmed(ReactionStatus.Completed, [])).toBe(false);
  });
});

describe('userRunTags', () => {
  const run = { creator: 'io', samples: ['sha1', 'sha2'], repos: [{ url: 'git.example/r' }], source: WORKFLOW } as never;
  const tags = ['mine=1', 'WorkflowRun', 'Workflow=w1', 'WorkflowInstance=i1', 'sha1', 'sha2', 'git.example/r', 'io'];
  it('hides the tags the API adds to a workflow run', () => {
    expect(userRunTags({ ...(run as object), tags } as never)).toEqual(['mine=1']);
  });
  it('keeps a tool run user tag that only looks like a workflow tag', () => {
    expect(userRunTags({ ...(run as object), source: TOOL, tags } as never)).toEqual([
      'mine=1',
      'WorkflowRun',
      'Workflow=w1',
      'WorkflowInstance=i1',
    ]);
  });
});

describe('noResultsMessage', () => {
  it("says a completed run's result may have been pruned", () => {
    expect(noResultsMessage(ReactionStatus.Completed)).toContain('may have been pruned');
    expect(noResultsMessage(ReactionStatus.Completed)).not.toContain('stored no results');
  });
  it('says a failed run stored no results', () => {
    expect(noResultsMessage(ReactionStatus.Failed)).toBe('This run stored no results on this input.');
  });
});

describe('reactionReadView', () => {
  it('shows the details while the reads succeed or the first read is pending', () => {
    expect(reactionReadView(false, '')).toBe(ReactionReadView.Details);
    expect(reactionReadView(true, '')).toBe(ReactionReadView.Details);
  });
  it('shows not found only when the reaction was never read', () => {
    expect(reactionReadView(false, 'Get Reaction: not found')).toBe(ReactionReadView.NotFound);
  });
  it('keeps the loaded details with a warning when a later poll fails', () => {
    expect(reactionReadView(true, 'Get Reaction: upstream unavailable')).toBe(ReactionReadView.StaleDetails);
  });
});

describe('isUnknownStage', () => {
  it('is true only when the stage names are known and lack the stage', () => {
    expect(isUnknownStage('nope', ['build', 'scan'])).toBe(true);
    expect(isUnknownStage('build', ['build', 'scan'])).toBe(false);
  });
  it('is false when the stage names could not be read or are empty', () => {
    expect(isUnknownStage('nope', null)).toBe(false);
    expect(isUnknownStage('nope', [])).toBe(false);
  });
});

describe('noStageLogsMessage', () => {
  it('says the stage may still log while the reaction is running', () => {
    expect(noStageLogsMessage('build', ReactionStatus.Started, ['build', 'scan'])).toBe(
      'No logs have been recorded for the build stage of this reaction yet',
    );
  });
  it('drops "yet" once the reaction has finished', () => {
    expect(noStageLogsMessage('build', ReactionStatus.Completed, ['build'])).toBe(
      'No logs were recorded for the build stage of this reaction',
    );
    expect(noStageLogsMessage('build', ReactionStatus.Failed, ['build'])).not.toContain('yet');
  });
  it('names the reaction stages when the stage is not one of them', () => {
    expect(noStageLogsMessage('nope', ReactionStatus.Completed, ['stub-stage'])).toBe(
      'This reaction has no stage named nope. Its stages are: stub-stage.',
    );
  });
  it('falls back to the running wording when the reaction could not be read', () => {
    expect(noStageLogsMessage('build', null, null)).toContain('yet');
  });
});
