import { describe, expect, it } from 'vitest';

// project imports
import { buildPipelineRunRequest, pipelineBlocker } from './pipelineRequest';
import { PipelineBan } from '@models/pipelines';

const BASE = { group: 'g', pipeline: 'p', samples: ['abc'], repos: [], tags: '', sla: '' };

describe('buildPipelineRunRequest', () => {
  it('builds a request with empty args, buffers, and cache', () => {
    const request = buildPipelineRunRequest(BASE);
    expect(request).toMatchObject({ group: 'g', pipeline: 'p', samples: ['abc'], args: {}, buffers: {}, tags: [] });
    expect(request.cache).toEqual({ generic: {}, files: [] });
    expect(request).not.toHaveProperty('sla');
  });
  it('splits tags on commas and reads the SLA', () => {
    const request = buildPipelineRunRequest({ ...BASE, tags: ' a, b ,,', sla: '60' });
    expect(request.tags).toEqual(['a', 'b']);
    expect(request.sla).toBe(60);
  });
  it('ignores a bad SLA', () => {
    expect(buildPipelineRunRequest({ ...BASE, sla: 'x' })).not.toHaveProperty('sla');
  });
});

describe('pipelineBlocker', () => {
  const ban = { id: '1', time_banned: '', ban_kind: { Generic: { msg: 'm' } } } as PipelineBan;
  it('allows a pipeline whose stages all have tools and no bans', () => {
    expect(pipelineBlocker({ order: [['a'], ['b']], stage_tools: { a: 't', b: 't' } })).toBeNull();
  });
  it('blocks a banned pipeline', () => {
    expect(pipelineBlocker({ order: [['a']], stage_tools: { a: 't' }, bans: { x: ban } })).toMatch(/bans/);
  });
  it('names the stages with no tool', () => {
    expect(pipelineBlocker({ order: [['a'], ['b', 'c']], stage_tools: { a: 't' } })).toBe(
      'These stages are legacy images with no tool: b, c',
    );
    expect(pipelineBlocker({ order: [['a']] })).toMatch(/a$/);
  });
});
