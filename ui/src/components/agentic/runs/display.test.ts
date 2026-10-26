import { describe, expect, it } from 'vitest';

// project imports
import { agentName, filterRunsByKind, formatTimestamp, mergeRuns, sampleFileNames, statusTone, workflowInstanceLabel } from './display';
import { BadgeTone } from '@components/agentic/shared';
import { Reaction, ReactionStatus } from '@models/reactions';
import { RunKind } from '@models/runs';

const tool = {
  id: 'a',
  sla: '2026-01-01',
  source: { Tool: { tool_instance: 'i', provenance: { agents: [], tools: [] } } },
} as unknown as Reaction;
const pipe = { id: 'b', sla: '2026-02-01' } as unknown as Reaction;

describe('statusTone', () => {
  it('colours each status', () => {
    expect(statusTone(ReactionStatus.Completed)).toBe(BadgeTone.Ok);
    expect(statusTone(ReactionStatus.Failed)).toBe(BadgeTone.Error);
    expect(statusTone(ReactionStatus.Started)).toBe(BadgeTone.Active);
    expect(statusTone(ReactionStatus.Created)).toBe(BadgeTone.Neutral);
  });
});

describe('run lists', () => {
  it('filters by kind', () => {
    expect(filterRunsByKind([tool, pipe], RunKind.Tool)).toEqual([tool]);
    expect(filterRunsByKind([tool, pipe], RunKind.Pipeline)).toEqual([pipe]);
    expect(filterRunsByKind([tool, pipe], null)).toHaveLength(2);
  });
  it('merges without repeats, latest SLA expiry first', () => {
    expect(mergeRuns([[tool, pipe], [tool]]).map((run) => run.id)).toEqual(['b', 'a']);
  });
  it('orders SLA times of different fractional precision by instant', () => {
    // 500 ms after the whole second, though it sorts before it as text
    const later = { id: 'c', sla: '2026-03-01T00:00:00.500Z' } as unknown as Reaction;
    const earlier = { id: 'd', sla: '2026-03-01T00:00:00Z' } as unknown as Reaction;
    expect(mergeRuns([[earlier, later]]).map((run) => run.id)).toEqual(['c', 'd']);
  });
});

describe('formatTimestamp', () => {
  it('shows an RFC 3339 instant with nanoseconds in the local format', () => {
    const shown = formatTimestamp('2026-10-28T15:52:39.993492946Z');
    expect(shown).toBe(new Date(Date.parse('2026-10-28T15:52:39.993Z')).toLocaleString());
    expect(shown).not.toContain('993492946');
  });
  it('keeps text that is not a date', () => {
    expect(formatTimestamp('soon')).toBe('soon');
  });
});

describe('agentName', () => {
  const provenance = { agents: [{ agent: 'id1', name: 'demo-agent', config_hash: '', toolset_digest: '' }], tools: [] };
  it('names an agent from the provenance and falls back to its id', () => {
    expect(agentName(provenance, 'id1')).toBe('demo-agent');
    expect(agentName(provenance, 'id2')).toBe('id2');
    expect(agentName(null, 'id1')).toBe('id1');
  });
});

describe('workflowInstanceLabel', () => {
  it('names the instance from a result key that names it, and falls back to the id', () => {
    expect(workflowInstanceLabel('triage#r4~inst-1', 'inst-1')).toBe('triage#r4');
    expect(workflowInstanceLabel('triage#r4~inst-2', 'inst-1')).toBe('inst-1');
    expect(workflowInstanceLabel('scanner', 'inst-1')).toBe('inst-1');
  });
});

describe('sampleFileNames', () => {
  const submission = (name?: string) => ({ id: 'x', name }) as never;
  it('lists the distinct names a sample was submitted under', () => {
    expect(sampleFileNames({ submissions: [submission('prompt.txt'), submission('prompt.txt'), submission('a.bin')] })).toBe(
      'prompt.txt, a.bin',
    );
  });
  it('is empty when no submission was named or none was returned', () => {
    expect(sampleFileNames({ submissions: [submission()] })).toBe('');
    expect(sampleFileNames({} as never)).toBe('');
  });
});
