import { describe, expect, it } from 'vitest';

// project imports
import { applyPins, pinnedToolInstance, toolPinChoice, toolSetPinChoice } from './pins';
import { Tool } from '@models/tools';
import { ToolSetInstance } from '@models/toolsets';

/**
 * Build a tool with some versions.
 *
 * @param instances - The versions and whether each is deprecated, newest first.
 * @returns The tool.
 */
function tool(instances: [string, boolean][]): Tool {
  return {
    id: 't',
    name: 'floss',
    kind: 'Tool',
    metadata: {
      Tool: {
        kind: 'Agentic',
        latest: instances.find(([, deprecated]) => !deprecated)?.[0] ?? null,
        latest_version: instances.length > 0 ? { SemVer: instances.find(([, deprecated]) => !deprecated)![0] } : null,
        latest_spec_hash: null,
        modified: '',
        instances: instances.map(([version, deprecated]) => ({
          id: `i-${version}`,
          version: { SemVer: version },
          spec_hash: '',
          deprecated,
          created: '',
        })),
      },
    },
  } as unknown as Tool;
}

/**
 * Build a toolset instance.
 *
 * @param set - The toolset id.
 * @param revision - The revision.
 * @param ready - Whether it is Ready.
 * @returns The instance.
 */
function setInstance(set: string, revision: number, ready: boolean): ToolSetInstance {
  return {
    id: `${set}-r${revision}`,
    name: `${set}#r${revision}`,
    metadata: { ToolSetInstance: { tool_set: set, revision, status: ready ? 'Ready' : { Invalid: { errors: ['x'] } } } },
  } as unknown as ToolSetInstance;
}

describe('pins', () => {
  it('offers the versions of a tool that are not deprecated and says what latest is now', () => {
    const choice = toolPinChoice(
      tool([
        ['2.0.0', true],
        ['1.1.0', false],
        ['1.0.0', false],
      ]),
    );
    expect(choice.latestText).toBe('latest (now 1.1.0)');
    expect(choice.options).toEqual([
      { value: 'i-1.1.0', text: 'pinned 1.1.0' },
      { value: 'i-1.0.0', text: 'pinned 1.0.0' },
    ]);
    expect(toolPinChoice(tool([])).latestText).toBe('latest');
  });
  it('offers the Ready instances of one toolset, newest first', () => {
    const all = [setInstance('a', 1, true), setInstance('a', 3, true), setInstance('a', 2, false), setInstance('b', 9, true)];
    const choice = toolSetPinChoice('a', 'set', all);
    expect(choice.latestText).toBe('latest (now r3)');
    expect(choice.options.map((option) => option.value)).toEqual(['a-r3', 'a-r1']);
    expect(toolSetPinChoice('c', 'none', all).latestText).toBe('latest');
  });
  it('sends the pinned instance in place of its root and keeps the rest at latest', () => {
    expect(applyPins(['a', 'b', 'c'], { b: 'b-1', c: '' })).toEqual(['a', 'b-1', 'c']);
  });
});

describe('pinnedToolInstance', () => {
  it('finds the Image tool instance a container backed agentic tool is pinned to', () => {
    const spec = { Agentic: { backing: { Container: { tool_instance: 'i-1', inputs: {} } } } };
    expect(pinnedToolInstance(spec)).toBe('i-1');
  });
  it('finds nothing for other backings and other kinds of tool', () => {
    expect(pinnedToolInstance({ Agentic: { backing: { ThoriumMcp: { tool: 'get_sample' } } } })).toBeNull();
    expect(pinnedToolInstance({ Image: {} })).toBeNull();
    expect(pinnedToolInstance({ Agentic: { backing: { Container: {} } } })).toBeNull();
    expect(pinnedToolInstance(undefined)).toBeNull();
  });
});
