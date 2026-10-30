import { describe, expect, it } from 'vitest';

// project imports
import {
  buildPipelineRequest,
  isStageConverted,
  listedGroup,
  pipelineProblem,
  PipelineFormValues,
  pinStageInstances,
  retainStageTools,
  stageBacking,
  stageToolChoices,
  pinnedVersionName,
  stageToolOptions,
  stageToolRef,
  stepsFromPipeline,
  uniqueStageName,
} from './stages';
import { AgenticKinds } from '@models/agentic';
import { Pipeline } from '@models/pipelines';
import { Tool, ToolInstance, ToolKind } from '@models/tools';

const VALID: PipelineFormValues = {
  name: ' scan ',
  group: 'g',
  description: ' d ',
  sla: '',
  steps: [
    [{ name: 'extract', tool: 't1', instance: '' }],
    [
      { name: 'a', tool: 't2', instance: 'i2' },
      { name: 'b', tool: 't3', instance: '' },
    ],
  ],
};

describe('listedGroup', () => {
  it('keeps a group the form offers', () => {
    expect(listedGroup('minerva', ['io', 'minerva'])).toBe('minerva');
  });

  it('drops a group the form does not offer, such as a copied pipeline group the user cannot create in', () => {
    expect(listedGroup('static', ['minerva'])).toBe('');
    expect(listedGroup('static', [])).toBe('');
  });

  it('leaves an unchosen group unchosen', () => {
    expect(listedGroup('', ['minerva'])).toBe('');
  });
});

describe('pipelineProblem', () => {
  it('accepts a complete form', () => {
    expect(pipelineProblem(VALID)).toBe('');
  });
  it('names the first missing piece', () => {
    expect(pipelineProblem({ ...VALID, name: ' ' })).toMatch(/name/);
    expect(pipelineProblem({ ...VALID, group: '' })).toMatch(/group/);
    expect(pipelineProblem({ ...VALID, steps: [] })).toMatch(/at least one stage/);
    expect(pipelineProblem({ ...VALID, steps: [[]] })).toMatch(/at least one stage/);
  });
  it('refuses unnamed, repeated, and unbound stages', () => {
    expect(pipelineProblem({ ...VALID, steps: [[{ name: '', tool: 't', instance: '' }]] })).toMatch(/needs a name/);
    expect(
      pipelineProblem({ ...VALID, steps: [[{ name: 'a', tool: 't', instance: '' }], [{ name: ' a', tool: 't', instance: '' }]] }),
    ).toMatch(/used twice/);
    expect(pipelineProblem({ ...VALID, steps: [[{ name: 'a', tool: '', instance: '' }]] })).toMatch(/Choose a tool for stage a/);
  });
  it('refuses a bad SLA', () => {
    expect(pipelineProblem({ ...VALID, sla: 'x' })).toMatch(/SLA/);
    expect(pipelineProblem({ ...VALID, sla: '0' })).toMatch(/SLA/);
    expect(pipelineProblem({ ...VALID, sla: '3600' })).toBe('');
    expect(pipelineProblem({ ...VALID, sla: '1e3' })).toMatch(/SLA/);
    expect(pipelineProblem({ ...VALID, sla: '300 sec' })).toMatch(/SLA/);
  });
});

describe('buildPipelineRequest', () => {
  it('writes single stages as names, parallel stages as lists, and binds each stage to a tool or pinned instance', () => {
    const request = buildPipelineRequest(VALID);
    expect(request.order).toEqual(['extract', ['a', 'b']]);
    expect(request.stage_tools).toEqual({ extract: 't1', a: 'i2', b: 't3' });
    expect(request.name).toBe('scan');
    expect(request.description).toBe('d');
    expect(request.sla).toBe(604800);
  });
  it('uses the SLA given and omits a blank description', () => {
    const request = buildPipelineRequest({ ...VALID, sla: '120', description: '' });
    expect(request.sla).toBe(120);
    expect(request).not.toHaveProperty('description');
  });
  it('drops empty steps', () => {
    expect(buildPipelineRequest({ ...VALID, steps: [[], VALID.steps[0]] }).order).toEqual(['extract']);
  });
  it('carries copied triggers through unchanged', () => {
    const triggers = { 'on-new-sample': 'NewSample' as const };
    expect(buildPipelineRequest({ ...VALID, triggers }).triggers).toEqual(triggers);
    expect(buildPipelineRequest({ ...VALID, triggers: {} })).not.toHaveProperty('triggers');
    expect(buildPipelineRequest(VALID)).not.toHaveProperty('triggers');
  });
});

describe('stepsFromPipeline', () => {
  const pipeline = {
    group: 'g',
    name: 'p',
    creator: 'u',
    order: ['one', ['two', 'three']],
    sla: 1,
    stage_tools: { one: 'tool-1', two: 'inst-2' },
  } as Pipeline;
  it('pins stages bound to an instance, follows stages bound to a tool, and leaves unbound stages empty', () => {
    expect(stepsFromPipeline(pipeline, { 'inst-2': 'tool-2' })).toEqual([
      [{ name: 'one', tool: 'tool-1', instance: '' }],
      [
        { name: 'two', tool: 'tool-2', instance: 'inst-2' },
        { name: 'three', tool: '', instance: '' },
      ],
    ]);
  });
});

describe('isStageConverted', () => {
  it('is true only for stages with a stage tool', () => {
    expect(isStageConverted({ stage_tools: { a: 't' } }, 'a')).toBe(true);
    expect(isStageConverted({ stage_tools: { a: 't' } }, 'b')).toBe(false);
    expect(isStageConverted({}, 'a')).toBe(false);
  });
});

/** Build a tool of one kind in the given groups. */
function tool(id: string, kind: ToolKind, groups: string[]): Tool {
  return {
    id,
    name: id,
    kind: AgenticKinds.Tool,
    description: null,
    submitter: 'u',
    groups,
    tags: {},
    image: null,
    created: '',
    metadata: { Tool: { kind, latest: null, latest_version: null, latest_spec_hash: null, modified: '' } },
  };
}

describe('stageToolChoices', () => {
  const tools = [tool('a', ToolKind.Image, ['g1']), tool('b', ToolKind.Image, ['g2']), tool('c', ToolKind.Agentic, ['g1'])];
  it("offers only the Image tools of the pipeline's group", () => {
    expect(stageToolChoices(tools, 'g1').map((entry) => entry.id)).toEqual(['a']);
    expect(stageToolChoices(tools, 'g2').map((entry) => entry.id)).toEqual(['b']);
  });
  it('offers every Image tool before a group is chosen', () => {
    expect(stageToolChoices(tools, '').map((entry) => entry.id)).toEqual(['a', 'b']);
  });
  it('sorts the choices by name without reordering the input', () => {
    const unsorted = [tool('zeta', ToolKind.Image, ['g1']), tool('alpha', ToolKind.Image, ['g1']), tool('mid', ToolKind.Image, ['g1'])];
    expect(stageToolChoices(unsorted, 'g1').map((entry) => entry.name)).toEqual(['alpha', 'mid', 'zeta']);
    expect(stageToolChoices(unsorted, '').map((entry) => entry.name)).toEqual(['alpha', 'mid', 'zeta']);
    expect(unsorted.map((entry) => entry.name)).toEqual(['zeta', 'alpha', 'mid']);
  });
});

describe('retainStageTools', () => {
  const tools = [tool('t1', ToolKind.Image, ['g1', 'g2']), tool('t2', ToolKind.Image, ['g1']), tool('t3', ToolKind.Image, ['g2'])];
  it('keeps the tool and pinned version of a stage whose tool is in the new group', () => {
    expect(retainStageTools(VALID.steps, tools, 'g1')).toEqual([
      [{ name: 'extract', tool: 't1', instance: '' }],
      [
        { name: 'a', tool: 't2', instance: 'i2' },
        { name: 'b', tool: '', instance: '' },
      ],
    ]);
  });
  it('clears only the stages whose tool is not in the new group and keeps every stage name', () => {
    expect(retainStageTools(VALID.steps, tools, 'g2')).toEqual([
      [{ name: 'extract', tool: 't1', instance: '' }],
      [
        { name: 'a', tool: '', instance: '' },
        { name: 'b', tool: 't3', instance: '' },
      ],
    ]);
  });
  it('clears a stage bound to a tool that is not known', () => {
    expect(retainStageTools([[{ name: 's', tool: 'gone', instance: 'i' }]], tools, 'g1')).toEqual([
      [{ name: 's', tool: '', instance: '' }],
    ]);
  });
});

describe('pinStageInstances', () => {
  it('pins a stage bound to a tool instance and keeps every other stage', () => {
    const steps = [[{ name: 'scan', tool: 'inst-1', instance: '' }], [{ name: 'mine', tool: 't2', instance: '' }]];
    expect(pinStageInstances(steps, { 'inst-1': 't1' })).toEqual([
      [{ name: 'scan', tool: 't1', instance: 'inst-1' }],
      [{ name: 'mine', tool: 't2', instance: '' }],
    ]);
  });
  it('leaves already pinned stages alone', () => {
    const steps = [[{ name: 'scan', tool: 't1', instance: 'inst-1' }]];
    expect(pinStageInstances(steps, { 'inst-1': 't1', t1: 'x' })).toEqual(steps);
  });
});

describe('stageToolRef', () => {
  it("is the pinned instance's id, or the tool's id when the stage follows latest", () => {
    expect(stageToolRef({ tool: 't1', instance: 'i1' })).toBe('i1');
    expect(stageToolRef({ tool: 't1', instance: '' })).toBe('t1');
  });
});

describe('stageToolOptions', () => {
  /** Build an Image tool in group g with the given versions. */
  function versioned(id: string, name: string, versions: [string, string, boolean][]): Tool {
    const base = tool(id, ToolKind.Image, ['g']);
    base.name = name;
    base.metadata.Tool.instances = versions.map(([instance, version, deprecated]) => ({
      id: instance,
      version: { SemVer: version },
      spec_hash: '',
      deprecated,
      created: '',
    }));
    return base;
  }
  it('offers each tool at latest and at every version, by tool name', () => {
    const tools = [
      versioned('t2', 'zeta', []),
      versioned('t1', 'alpha', [
        ['i1', '1.0.0', false],
        ['i2', '2.0.0', true],
      ]),
    ];
    expect(stageToolOptions(tools, 'g')).toEqual([
      { label: 'alpha · follow latest', value: 't1', base: 'alpha' },
      { label: 'alpha · pin 1.0.0', value: 'i1', base: 'alpha' },
      { label: 'alpha · pin 2.0.0 (deprecated)', value: 'i2', base: 'alpha' },
      { label: 'zeta · follow latest', value: 't2', base: 'zeta' },
    ]);
  });
  it('quotes a version named latest so it is not mistaken for following latest', () => {
    const tools = [versioned('t1', 'alpha', [['i1', 'latest', false]])];
    tools[0].metadata.Tool.instances![0].version = { Custom: 'latest' };
    expect(stageToolOptions(tools, 'g').map((option) => option.label)).toEqual(['alpha · follow latest', 'alpha · pin "latest"']);
    expect(pinnedVersionName({ Custom: 'latest' })).toBe('"latest"');
    expect(pinnedVersionName({ SemVer: '1.0.0' })).toBe('1.0.0');
  });
  it("offers the same tools as the create page's select: Image tools of the group only", () => {
    const tools = [tool('a', ToolKind.Image, ['g1']), tool('b', ToolKind.Image, ['g2']), tool('c', ToolKind.Agentic, ['g1'])];
    expect(stageToolOptions(tools, 'g1').map((option) => option.value)).toEqual(stageToolChoices(tools, 'g1').map((entry) => entry.id));
  });
  it('is empty for a group with no stage tools', () => {
    expect(stageToolOptions([tool('c', ToolKind.Agentic, ['g1']), tool('b', ToolKind.Image, ['g2'])], 'g1')).toEqual([]);
  });
});

describe('uniqueStageName', () => {
  it('keeps a free name and numbers a taken one', () => {
    expect(uniqueStageName('scan', ['other'])).toBe('scan');
    expect(uniqueStageName('scan', ['scan'])).toBe('scan-2');
    expect(uniqueStageName('scan', ['scan', 'scan-2'])).toBe('scan-3');
  });
});

describe('stageBacking', () => {
  it('names a tool that a stage follows at latest, and the tool and version of a pinned instance', () => {
    const tool = { kind: AgenticKinds.Tool, id: 't1', name: 'scan' } as unknown as Tool;
    expect(stageBacking(tool)).toEqual({ name: 'scan', toolId: 't1', pinned: null });
    const instance = {
      kind: AgenticKinds.ToolInstance,
      id: 'i1',
      name: 'scan@1.2.0',
      metadata: { ToolInstance: { tool: 't1', version: { SemVer: '1.2.0' } } },
    } as unknown as ToolInstance;
    expect(stageBacking(instance)).toEqual({ name: 'scan', toolId: 't1', pinned: '1.2.0' });
    // an id of another kind, or an answer without metadata, backs nothing
    expect(stageBacking({ kind: AgenticKinds.Agent } as unknown as Tool)).toBeNull();
    expect(stageBacking({ kind: AgenticKinds.ToolInstance } as unknown as ToolInstance)).toBeNull();
  });
});
