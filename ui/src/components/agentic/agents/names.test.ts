import { describe, expect, it } from 'vitest';

// project imports
import { linkFor, modelsText, nameIndex, versionNote } from './names';
import { AgenticKinds } from '@models/agentic';

const entity = (id: string, name: string) =>
  ({
    id,
    name,
    kind: AgenticKinds.Tool,
    metadata: {},
    description: null,
    submitter: '',
    groups: [],
    tags: {},
    image: null,
    created: '',
  }) as never;

describe('names', () => {
  it('indexes several lists', () => {
    const index = nameIndex([entity('1', 'a')], [entity('2', 'b')]);
    expect(index.get('1')).toBe('a');
    expect(index.get('2')).toBe('b');
  });
  it('describes models', () => {
    expect(modelsText([{ model_id: 'x' }, { model_id: 'y' }])).toBe('x > y');
    expect(modelsText([])).toBe('workflow default');
    expect(modelsText(undefined)).toBe('workflow default');
  });
  it('links entities by kind', () => {
    expect(linkFor({ id: '1', kind: AgenticKinds.Tool })).toBe('/tools/1');
    expect(linkFor({ id: '2', kind: AgenticKinds.ToolSet })).toBe('/toolsets/2');
    expect(linkFor({ id: '3', kind: AgenticKinds.Agent })).toBe('/agents/3');
    expect(linkFor({ id: '4', kind: AgenticKinds.WorkflowInstance })).toBe('/workflows/instances/4');
    expect(linkFor({ id: '5', kind: AgenticKinds.WorkflowCatalog })).toBe('/workflows/5');
    expect(linkFor({ id: '6', kind: AgenticKinds.ModelEndpoint })).toBe('/tools');
  });
});

describe('versionNote', () => {
  const of = (kind: AgenticKinds, metadata: Record<string, unknown>) => ({ id: '1', name: 'x', kind, metadata, groups: [] }) as never;
  it('says a tool follows its latest version', () => {
    expect(versionNote(of(AgenticKinds.Tool, { Tool: { latest_version: { SemVer: '1.0.0' } } }))).toBe('latest → 1.0.0');
    expect(versionNote(of(AgenticKinds.Tool, { Tool: { latest_version: null } }))).toBe('latest (no version)');
  });
  it('says a tool instance is pinned', () => {
    expect(versionNote(of(AgenticKinds.ToolInstance, { ToolInstance: { version: { Custom: 'beta' } } }))).toBe('pinned beta');
  });
  it('says a toolset follows its latest instance', () => {
    expect(versionNote(of(AgenticKinds.ToolSet, { ToolSet: { latest_instance: 'abc' } }))).toBe('latest');
  });
  it('says a toolset instance is pinned to its revision', () => {
    expect(versionNote(of(AgenticKinds.ToolSetInstance, { ToolSetInstance: { revision: 1 } }))).toBe('pinned r1');
    expect(versionNote(of(AgenticKinds.ToolSetInstance, {}))).toBe('pinned');
  });
  it('is empty for other kinds', () => {
    expect(versionNote(of(AgenticKinds.Agent, {}))).toBe('');
  });
});
