import { describe, it, expect } from 'vitest';

// project imports
import { emptyGroupStatsText, flattenGroupStats, indexWorkloadInstances, matchesGroupStatsSearch } from './stats';
import { ToolInstance } from '@models/tools';

const counts = { created: 1, running: 2, completed: 3, failed: 0, sleeping: 0, total: 6 };

describe('flattenGroupStats', () => {
  it('makes one row per group, tool instance, and user', () => {
    const rows = flattenGroupStats({
      minerva: { workloads: { 'inst-a': { europa: counts, test: { ...counts, total: 1 } } } },
      juno: { workloads: {} },
    });
    expect(rows).toEqual([
      { group: 'minerva', toolInstance: 'inst-a', user: 'europa', ...counts },
      { group: 'minerva', toolInstance: 'inst-a', user: 'test', ...counts, total: 1 },
    ]);
  });
  it('adds no rows for a group the API sends without workloads', () => {
    expect(flattenGroupStats({ system: {} as never })).toEqual([]);
  });
});

describe('indexWorkloadInstances', () => {
  it('names each instance and points at its tool', () => {
    const instance = { id: 'inst-a', name: 'scan@1.0.0', metadata: { ToolInstance: { tool: 'tool-1' } } } as unknown as ToolInstance;
    expect(indexWorkloadInstances([instance]).get('inst-a')).toEqual({ name: 'scan@1.0.0', toolId: 'tool-1' });
    expect(indexWorkloadInstances([]).get('inst-a')).toBeUndefined();
  });
});

describe('emptyGroupStatsText', () => {
  it('explains a table with no workload at all', () => {
    expect(emptyGroupStatsText(0, '')).toMatch(/no workload/i);
    expect(emptyGroupStatsText(0, 'abc')).toMatch(/no workload/i);
  });
  it('explains a search that hides every row', () => {
    expect(emptyGroupStatsText(3, 'zzz')).toMatch(/matches the search/i);
  });
  it('has nothing to say when rows exist and no search is set', () => {
    expect(emptyGroupStatsText(3, '')).toBe('');
    expect(emptyGroupStatsText(3, '  ')).toBe('');
  });
});

describe('matchesGroupStatsSearch', () => {
  const row = { group: 'Minerva', toolInstance: 'inst-a', user: 'Europa', ...counts };
  it('matches every row when the search is empty or only whitespace', () => {
    expect(matchesGroupStatsSearch(row, '', 'tool@1')).toBe(true);
    expect(matchesGroupStatsSearch(row, ' ', 'tool@1')).toBe(true);
    expect(matchesGroupStatsSearch(row, '   ', 'tool@1')).toBe(true);
  });
  it('matches the group, instance id, instance name, or user without regard to case or surrounding spaces', () => {
    expect(matchesGroupStatsSearch(row, 'MINERVA', 'tool@1')).toBe(true);
    expect(matchesGroupStatsSearch(row, 'inst-a', 'tool@1')).toBe(true);
    expect(matchesGroupStatsSearch(row, ' tool@1 ', 'tool@1')).toBe(true);
    expect(matchesGroupStatsSearch(row, 'europa ', 'tool@1')).toBe(true);
  });
  it('does not match text found in none of those columns', () => {
    expect(matchesGroupStatsSearch(row, 'zzz', 'tool@1')).toBe(false);
  });
});
