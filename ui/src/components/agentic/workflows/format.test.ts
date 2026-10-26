import { describe, expect, it } from 'vitest';

// project imports
import { defaultModelsText, entryText, matchersForIds, mergeAuthLevels, newestFirst, triggerText } from './format';

describe('workflow formatting', () => {
  it('names triggers', () => {
    expect(triggerText('Manual')).toBe('Requested');
    expect(triggerText('WorkflowUpdated')).toBe('Workflow changed');
    expect(triggerText({ ToolUpdated: { tool: 'x' } })).toBe('Tool updated');
    expect(triggerText({ AgentUpdated: { agent: 'x' } })).toBe('Agent changed');
    expect(triggerText({ ToolSetUpdated: { tool_set: 'x' } })).toBe('Toolset updated');
    expect(triggerText('SomethingNew')).toBe('SomethingNew');
    expect(triggerText(null)).toBe('Unknown');
  });
  it('joins default models', () => {
    expect(defaultModelsText([{ model_id: 'a' }, { model_id: 'b' }])).toBe('a > b');
    expect(defaultModelsText([])).toBe('none');
    expect(defaultModelsText(undefined)).toBe('none');
  });
  it('orders instances by revision descending', () => {
    const make = (revision: number) => ({ metadata: { WorkflowInstance: { revision } } });
    expect(newestFirst([make(1), make(3), make(2)]).map((item) => item.metadata.WorkflowInstance.revision)).toEqual([3, 2, 1]);
  });
  it('names the entry agent', () => {
    const names = new Map([['a', 'lead']]);
    expect(entryText('a', names)).toBe('lead');
    expect(entryText('z', names)).toBe('z');
    expect(entryText(null, names)).toBe('none');
  });
  it('keeps the stored matcher of a model id that stays and makes new ids bare', () => {
    const stored = [{ model_id: 'a', provider: 'p' }, { model_id: 'b' }];
    expect(matchersForIds(stored, ['b', 'a', 'c'])).toEqual([{ model_id: 'b' }, { model_id: 'a', provider: 'p' }, { model_id: 'c' }]);
    expect(matchersForIds(undefined, ['x'])).toEqual([{ model_id: 'x' }]);
    expect(matchersForIds(stored, [])).toEqual([]);
  });
  it('keeps reserved auth levels when the editable ones change', () => {
    expect(mergeAuthLevels(['Harness', 'Group'], ['User'])).toEqual(['Harness', 'User']);
    expect(mergeAuthLevels(['Group'], [])).toEqual([]);
    expect(mergeAuthLevels(undefined, ['User', 'Group'])).toEqual(['Group', 'User']);
  });
});
