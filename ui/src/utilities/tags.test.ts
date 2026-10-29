import { describe, it, expect } from 'vitest';

// project imports
import { diffTagUpdate, requestTagsToTagEntryList, tagEntriesToRequestTags } from './tags';

describe('tagEntriesToRequestTags', () => {
  it('groups values under their keys and dedupes repeated pairs', () => {
    const entries = [
      { key: 'family', value: 'emotet' },
      { key: 'family', value: 'qakbot' },
      { key: 'family', value: 'emotet' },
      { key: 'tlp', value: 'green' },
    ];
    expect(tagEntriesToRequestTags(entries)).toEqual({ family: ['emotet', 'qakbot'], tlp: ['green'] });
  });

  // the regression every caller leans on: tag keys are arbitrary user strings, so keys
  // that collide with Object.prototype names must never resolve to inherited members
  it('folds tag keys that collide with Object.prototype names', () => {
    const entries = [
      { key: 'toString', value: 'v1' },
      { key: 'constructor', value: 'v2' },
      { key: 'toString', value: 'v1' },
    ];
    expect(tagEntriesToRequestTags(entries)).toEqual({ toString: ['v1'], constructor: ['v2'] });
  });

  it('keeps every pair verbatim by default', () => {
    const entries = [{ key: ' family ', value: ' emotet ' }];
    expect(tagEntriesToRequestTags(entries)).toEqual({ ' family ': [' emotet '] });
  });

  it('trims pairs and skips blank sides when trim is opted into', () => {
    const entries = [
      { key: ' family ', value: ' emotet ' },
      { key: '', value: 'ignored' },
      { key: 'ignored', value: '   ' },
    ];
    expect(tagEntriesToRequestTags(entries, true)).toEqual({ family: ['emotet'] });
  });
});

describe('requestTagsToTagEntryList', () => {
  it('flattens a request tag map into key/value entries', () => {
    expect(requestTagsToTagEntryList({ family: ['emotet', 'qakbot'], tlp: ['green'] })).toEqual([
      { key: 'family', value: 'emotet' },
      { key: 'family', value: 'qakbot' },
      { key: 'tlp', value: 'green' },
    ]);
  });

  it('flattens an empty map to an empty list', () => {
    expect(requestTagsToTagEntryList({})).toEqual([]);
  });
});

describe('diffTagUpdate', () => {
  it('computes added and removed values per key', () => {
    const { toAdd, toDelete } = diffTagUpdate({ family: ['emotet', 'qakbot'] }, { family: ['emotet', 'pikabot'], tlp: ['green'] });
    expect(toAdd).toEqual({ family: ['pikabot'], tlp: ['green'] });
    expect(toDelete).toEqual({ family: ['qakbot'] });
  });

  // keys only one side holds must look up as own properties: a user-typed prototype key
  // like `toString` would otherwise resolve to the inherited member and crash the diff
  it('diffs tag keys that collide with Object.prototype names', () => {
    const added = diffTagUpdate({}, { toString: ['v1'] });
    expect(added.toAdd).toEqual({ toString: ['v1'] });
    expect(added.toDelete).toEqual({});
    const removed = diffTagUpdate({ constructor: ['v2'] }, {});
    expect(removed.toAdd).toEqual({});
    expect(removed.toDelete).toEqual({ constructor: ['v2'] });
  });

  it('treats undefined maps as empty', () => {
    const { toAdd, toDelete } = diffTagUpdate(undefined, undefined);
    expect(toAdd).toEqual({});
    expect(toDelete).toEqual({});
  });
});
