import { describe, expect, it } from 'vitest';

// project imports
import { addToolStage, PipelineEdit, removeToolStage, reorderStages, syncStageTools } from './diagramEdit';
import { StageToolOption } from './stages';
import { STEP_WIDTH, TERMINAL_OFFSET } from '@components/shared/pipeline/order';
import { editorObjectToPipelineUpdate } from '@utilities/transforms/pipeline';

const SCANNER: StageToolOption = { label: 'scanner · follow latest', value: 'tool-scanner', base: 'scanner' };
const PINNED: StageToolOption = { label: 'scanner · pin 1.0.0', value: 'inst-scanner-1', base: 'scanner' };
const UNPACK: StageToolOption = { label: 'unpack · follow latest', value: 'tool-unpack', base: 'unpack' };

// the x of the first and second stage, and a click past the last one
const FIRST = TERMINAL_OFFSET;
const SECOND = TERMINAL_OFFSET + STEP_WIDTH;
const PAST_END = TERMINAL_OFFSET + 5 * STEP_WIDTH;

describe('addToolStage', () => {
  it('adds the first stage of an empty pipeline with its tool', () => {
    expect(addToolStage({ order: [], stageTools: {} }, SCANNER, 0)).toEqual({
      order: ['scanner'],
      stageTools: { scanner: 'tool-scanner' },
    });
  });
  it('appends a stage after the last one and records its tool', () => {
    const edit: PipelineEdit = { order: ['unpack'], stageTools: { unpack: 'tool-unpack' } };
    expect(addToolStage(edit, SCANNER, PAST_END)).toEqual({
      order: ['unpack', 'scanner'],
      stageTools: { unpack: 'tool-unpack', scanner: 'tool-scanner' },
    });
  });
  it('inserts a stage before the first one when the gap after Start is clicked', () => {
    const edit: PipelineEdit = { order: ['unpack'], stageTools: { unpack: 'tool-unpack' } };
    expect(addToolStage(edit, SCANNER, 0).order).toEqual(['scanner', 'unpack']);
  });
  it('adds a parallel stage when a step is clicked', () => {
    const edit: PipelineEdit = { order: ['unpack', 'scanner'], stageTools: { unpack: 'tool-unpack', scanner: 'tool-scanner' } };
    const added = addToolStage(edit, UNPACK, SECOND);
    expect(added.order).toEqual(['unpack', ['scanner', 'unpack-2']]);
    expect(added.stageTools['unpack-2']).toBe('tool-unpack');
  });
  it('records a pinned instance id when a version is chosen', () => {
    expect(addToolStage({ order: [], stageTools: {} }, PINNED, FIRST).stageTools).toEqual({ scanner: 'inst-scanner-1' });
  });
  it('names a second stage of the same tool apart from the first', () => {
    const first = addToolStage({ order: [], stageTools: {} }, SCANNER, FIRST);
    const second = addToolStage(first, SCANNER, PAST_END);
    expect(second.order).toEqual(['scanner', 'scanner-2']);
    expect(second.stageTools).toEqual({ scanner: 'tool-scanner', 'scanner-2': 'tool-scanner' });
  });
  it('avoids the name of a legacy image stage that has no tool', () => {
    const legacy: PipelineEdit = { order: ['scanner'], stageTools: {} };
    const added = addToolStage(legacy, SCANNER, PAST_END);
    expect(added.order).toEqual(['scanner', 'scanner-2']);
    // the legacy stage stays without a tool and only the new stage gets one
    expect(added.stageTools).toEqual({ 'scanner-2': 'tool-scanner' });
  });
  it('does not change the edit it was given', () => {
    const edit: PipelineEdit = { order: ['unpack'], stageTools: { unpack: 'tool-unpack' } };
    addToolStage(edit, SCANNER, PAST_END);
    expect(edit).toEqual({ order: ['unpack'], stageTools: { unpack: 'tool-unpack' } });
  });
});

describe('removeToolStage', () => {
  const edit: PipelineEdit = {
    order: ['unpack', ['scanner', 'scanner-2'], 'report'],
    stageTools: { unpack: 'tool-unpack', scanner: 'tool-scanner', 'scanner-2': 'inst-scanner-1', report: 'tool-report' },
  };
  it('removes a single stage and its tool', () => {
    const removed = removeToolStage(edit, 0, 0);
    expect(removed.order).toEqual([['scanner', 'scanner-2'], 'report']);
    expect(removed.stageTools).not.toHaveProperty('unpack');
    expect(Object.keys(removed.stageTools)).toEqual(['scanner', 'scanner-2', 'report']);
  });
  it('removes one stage of a parallel step, leaving the other with its tool', () => {
    const removed = removeToolStage(edit, 1, 1);
    expect(removed.order).toEqual(['unpack', 'scanner', 'report']);
    expect(removed.stageTools).toEqual({ unpack: 'tool-unpack', scanner: 'tool-scanner', report: 'tool-report' });
  });
  it('removes a legacy stage that has no tool entry', () => {
    const legacy: PipelineEdit = { order: ['old', 'unpack'], stageTools: { unpack: 'tool-unpack' } };
    expect(removeToolStage(legacy, 0, 0)).toEqual({ order: ['unpack'], stageTools: { unpack: 'tool-unpack' } });
  });
});

describe('reorderStages', () => {
  it('keeps every stage mapped to its tool across a reorder', () => {
    const edit: PipelineEdit = { order: ['a', 'b', 'c'], stageTools: { a: 'ta', b: 'tb', c: 'tc' } };
    expect(reorderStages(edit, [['c', 'a'], 'b'])).toEqual({ order: [['c', 'a'], 'b'], stageTools: { a: 'ta', b: 'tb', c: 'tc' } });
  });
});

describe('syncStageTools', () => {
  it('drops the tools of stages that are not in the order', () => {
    expect(syncStageTools(['a', ['b', 'c']], { a: 'ta', b: 'tb', gone: 'tg' })).toEqual({ a: 'ta', b: 'tb' });
  });
});

describe('the update an edited diagram saves', () => {
  const original = {
    group: 'g',
    name: 'p',
    order: ['unpack'],
    sla: 100,
    stage_tools: { unpack: 'tool-unpack' },
  };
  it('carries the new order and the new stage tool', () => {
    const added = addToolStage({ order: original.order, stageTools: original.stage_tools }, SCANNER, PAST_END);
    const body = editorObjectToPipelineUpdate({ ...original, order: added.order, stage_tools: added.stageTools }, original)!.data;
    expect(body).toEqual({ order: ['unpack', 'scanner'], add_stage_tools: { scanner: 'tool-scanner' } });
  });
  it('carries no stage tool when a stage is removed', () => {
    const two = { ...original, order: ['unpack', 'scanner'], stage_tools: { unpack: 'tool-unpack', scanner: 'tool-scanner' } };
    const removed = removeToolStage({ order: two.order, stageTools: two.stage_tools }, 1, 0);
    const body = editorObjectToPipelineUpdate({ ...two, order: removed.order, stage_tools: removed.stageTools }, two)!.data;
    expect(body).toEqual({ order: ['unpack'] });
  });
});
