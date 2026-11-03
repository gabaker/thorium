import { describe, expect, it } from 'vitest';

// project imports
import { addableGroups, deprecatedCount, filterTools, NO_TOOL_FILTER, pipelineUses, stagesBackedBy, stillOffered } from './filters';
import { AgenticKinds } from '@models/agentic';
import { Pipeline } from '@models/pipelines';
import { Tool, ToolKind } from '@models/tools';

/** Build a tool with the given name, kind, groups, and latest version. */
function tool(name: string, kind: ToolKind, groups: string[], latest: string | null, description: string | null = null): Tool {
  return {
    id: name,
    name,
    kind: AgenticKinds.Tool,
    description,
    submitter: 'u',
    groups,
    tags: {},
    image: null,
    created: '2026-01-01T00:00:00Z',
    metadata: {
      Tool: {
        kind,
        latest: latest ? 'i1' : null,
        latest_version: latest ? { SemVer: latest } : null,
        latest_spec_hash: null,
        modified: '2026-01-01T00:00:00Z',
        instances: [
          { id: 'i1', version: { SemVer: '1.0.0' }, spec_hash: 'h', deprecated: latest === null, created: '' },
          { id: 'i2', version: { SemVer: '0.9.0' }, spec_hash: 'h2', deprecated: true, created: '' },
        ],
      },
    },
  };
}

const TOOLS = [
  tool('strings', ToolKind.Image, ['a'], '1.0.0', 'Extract printable strings'),
  tool('yara', ToolKind.Agentic, ['a', 'b'], '2.1.0'),
  tool('old', ToolKind.Image, ['b'], null),
];

describe('filterTools', () => {
  it('keeps everything without criteria', () => {
    expect(filterTools(TOOLS, NO_TOOL_FILTER)).toHaveLength(3);
  });
  it('filters by kind and group', () => {
    expect(filterTools(TOOLS, { ...NO_TOOL_FILTER, kind: ToolKind.Image }).map((t) => t.name)).toEqual(['strings', 'old']);
    expect(filterTools(TOOLS, { ...NO_TOOL_FILTER, group: 'b' }).map((t) => t.name)).toEqual(['yara', 'old']);
  });
  it('matches text on name, description, and version', () => {
    expect(filterTools(TOOLS, { ...NO_TOOL_FILTER, text: 'PRINTABLE' }).map((t) => t.name)).toEqual(['strings']);
    expect(filterTools(TOOLS, { ...NO_TOOL_FILTER, text: '2.1' }).map((t) => t.name)).toEqual(['yara']);
    expect(filterTools(TOOLS, { ...NO_TOOL_FILTER, text: 'zzz' })).toEqual([]);
  });
  it('hides tools with no live version', () => {
    expect(filterTools(TOOLS, { ...NO_TOOL_FILTER, hideDeprecated: true }).map((t) => t.name)).toEqual(['strings', 'yara']);
  });
});

describe('deprecatedCount', () => {
  it('counts deprecated instances', () => {
    expect(deprecatedCount(TOOLS[0])).toBe(1);
    expect(deprecatedCount(TOOLS[2])).toBe(2);
  });
  it('counts none on a tool the API sent without instances', () => {
    // the API leaves the instance list out when it is empty
    const emptied = tool('emptied', ToolKind.Image, ['a'], null);
    delete emptied.metadata.Tool.instances;
    expect(deprecatedCount(emptied)).toBe(0);
    expect(filterTools([emptied], NO_TOOL_FILTER)).toHaveLength(1);
  });
});

describe('stagesBackedBy', () => {
  it('lists the stages whose tool is the tool or one of its instances', () => {
    const stages = { scan: 'tool-1', report: 'inst-9', other: 'x' };
    expect(stagesBackedBy(stages, ['tool-1', 'inst-9'])).toEqual(['report', 'scan']);
    expect(stagesBackedBy(undefined, ['tool-1'])).toEqual([]);
  });
});

describe('addableGroups', () => {
  it('offers only editable groups the tool is not in yet', () => {
    expect(addableGroups(['callisto', 'minerva'], ['minerva'])).toEqual(['callisto']);
  });
  it('offers nothing when the user can edit only where the tool already is', () => {
    expect(addableGroups(['minerva'], ['minerva', 'juno'])).toEqual([]);
  });
});

describe('stillOffered', () => {
  it('drops a chosen group that is no longer offered', () => {
    expect(stillOffered(['juno', 'minerva'], ['minerva'])).toEqual(['minerva']);
  });
  it('keeps the chosen groups that are offered, in the order chosen', () => {
    expect(stillOffered(['minerva', 'callisto'], ['callisto', 'minerva'])).toEqual(['minerva', 'callisto']);
  });
  it('keeps nothing when nothing is chosen or offered', () => {
    expect(stillOffered([], ['minerva'])).toEqual([]);
    expect(stillOffered(['juno'], [])).toEqual([]);
  });
});

describe('pipelineUses', () => {
  it('lists the pipelines with a stage backed by the tool or one of its versions, with those stages', () => {
    const pipelines: Pick<Pipeline, 'name' | 'group' | 'stage_tools'>[] = [
      { name: 'p1', group: 'g', stage_tools: { b: 'v1', a: 't', c: 'other' } },
      { name: 'p2', group: 'g', stage_tools: { x: 'other' } },
      { name: 'p3', group: 'h', stage_tools: undefined },
    ];
    expect(pipelineUses(pipelines, ['t', 'v1'])).toEqual([{ name: 'p1', group: 'g', stages: ['a', 'b'] }]);
  });
});
