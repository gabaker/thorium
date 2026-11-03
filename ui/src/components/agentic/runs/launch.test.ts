import { describe, expect, it } from 'vitest';

// project imports
import {
  buildJobArgs,
  buildToolRequest,
  buildWorkflowRequest,
  candidateGroups,
  cleanRunError,
  LaunchTab,
  launchTabFromParam,
  parseKwargs,
  runPagePath,
  slaProblem,
} from './launch';

const BASE = { samples: ['abc'], repos: [{ url: 'u' }], group: '', tags: '', sla: '' };

describe('parseKwargs', () => {
  it('collects repeated keys and keeps = inside values', () => {
    expect(parseKwargs('--a=1\n--a=2\n\n--b=x=y').kwargs).toEqual({ '--a': ['1', '2'], '--b': ['x=y'] });
  });
  it('allows an empty value', () => {
    expect(parseKwargs('--a=').kwargs).toEqual({ '--a': [''] });
  });
  it('refuses a line without a key', () => {
    expect(parseKwargs('novalue').error).toMatch(/KEY=VALUE/);
    expect(parseKwargs('=x').error).toMatch(/KEY=VALUE/);
  });
});

describe('buildJobArgs', () => {
  it('builds positionals, kwargs, and switches', () => {
    const { args, error } = buildJobArgs('a\n b \n', '--k=v', '--s1, --s2');
    expect(error).toBeNull();
    expect(args).toEqual({
      positionals: ['a', 'b'],
      kwargs: { '--k': ['v'] },
      switches: ['--s1', '--s2'],
      opts: { override_positionals: false, override_kwargs: false },
    });
  });
  it('reports malformed kwargs', () => {
    expect(buildJobArgs('', 'bad', '').args).toBeNull();
  });
});

describe('run requests', () => {
  it('leaves an unset group and SLA out', () => {
    const request = buildWorkflowRequest(BASE);
    expect(request).toEqual({ inputs: { samples: ['abc'], repos: [{ url: 'u' }] }, tags: [] });
    expect('group' in request).toBe(false);
  });
  it('sends group, tags, and SLA when set', () => {
    expect(buildWorkflowRequest({ ...BASE, group: 'g', tags: 'a, b', sla: '60' })).toMatchObject({ group: 'g', tags: ['a', 'b'], sla: 60 });
  });
  it('keeps tags that start with #', () => {
    expect(buildWorkflowRequest({ ...BASE, tags: '#incident-42, triage' }).tags).toEqual(['#incident-42', 'triage']);
    const args = { positionals: [], kwargs: {}, switches: [], opts: { override_positionals: false, override_kwargs: false } };
    expect(buildToolRequest({ ...BASE, tags: '#incident-42\ntriage' }, args).tags).toEqual(['#incident-42', 'triage']);
  });
  it('leaves out an SLA that is not a whole number', () => {
    expect(buildWorkflowRequest({ ...BASE, sla: '1e3' })).not.toHaveProperty('sla');
  });
  it('adds the job args to a tool request', () => {
    const args = { positionals: [], kwargs: {}, switches: [], opts: { override_positionals: false, override_kwargs: false } };
    expect(buildToolRequest(BASE, args).args).toBe(args);
  });
  it('keeps the instance groups the user is in', () => {
    expect(candidateGroups(['a', 'b', 'c'], ['c', 'a'])).toEqual(['a', 'c']);
    expect(candidateGroups(['a'], [])).toEqual([]);
  });
});

describe('runPagePath', () => {
  it('opens the run in the chosen group, else the only candidate', () => {
    expect(runPagePath('id', 'b', ['a', 'b'], ['a', 'b'])).toBe('/runs/b/id');
    expect(runPagePath('id', '', ['a'], ['a', 'b'])).toBe('/runs/a/id');
  });
  it('opens the run in the instance group the API picked for a user outside it', () => {
    expect(runPagePath('id', '', [], ['wfonly'])).toBe('/runs/wfonly/id');
  });
  it('opens the runs list when the run group is unknown', () => {
    expect(runPagePath('id', '', [], ['x', 'y'])).toBe('/runs');
  });
});

describe('slaProblem', () => {
  it('accepts a blank SLA and a positive whole number', () => {
    expect(slaProblem('')).toBe('');
    expect(slaProblem('  ')).toBe('');
    expect(slaProblem('3600')).toBe('');
  });
  it('names the problem of anything else, so no SLA is dropped silently', () => {
    for (const sla of ['abc', '-5', '0', '1.5', '1e3', '1,000']) expect(slaProblem(sla)).toMatch(/positive whole number of seconds/);
  });
});

describe('cleanRunError', () => {
  it('splits off the trace id and turns backticks into quotes', () => {
    expect(cleanRunError("Run Workflow: you can't run in `minerva`: you aren't a developer for K8s trace: 16084666420bd23e")).toEqual({
      text: "Run Workflow: you can't run in 'minerva': you aren't a developer for K8s",
      trace: '16084666420bd23e',
    });
  });
  it('leaves a message without a trace as it is', () => {
    expect(cleanRunError('Choose the group to run in')).toEqual({ text: 'Choose the group to run in', trace: '' });
  });
});

describe('launchTabFromParam', () => {
  it('reads each tab and falls back to the workflow tab', () => {
    expect(launchTabFromParam('tool')).toBe(LaunchTab.Tool);
    expect(launchTabFromParam('pipeline')).toBe(LaunchTab.Pipeline);
    expect(launchTabFromParam('workflow')).toBe(LaunchTab.Workflow);
    expect(launchTabFromParam(null)).toBe(LaunchTab.Workflow);
    expect(launchTabFromParam('bogus')).toBe(LaunchTab.Workflow);
  });
});
