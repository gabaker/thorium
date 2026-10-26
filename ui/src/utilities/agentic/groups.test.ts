import { describe, expect, it } from 'vitest';

// project imports
import { describeOption, duplicateNames, keepCovering, missingGroups } from './groups';

const entity = (id: string, name: string, groups: string[]) => ({ id, name, groups });

describe('missingGroups', () => {
  it('lists the chosen groups the entity lacks', () => {
    expect(missingGroups({ groups: ['minerva'] }, ['juno', 'minerva'])).toEqual(['juno']);
  });
  it('is empty when no groups are chosen or all are covered', () => {
    expect(missingGroups({ groups: ['minerva'] }, [])).toEqual([]);
    expect(missingGroups({ groups: ['minerva', 'juno'] }, ['juno'])).toEqual([]);
  });
});

describe('keepCovering', () => {
  const entities = [entity('a', 'one', ['minerva']), entity('b', 'two', ['minerva', 'juno'])];
  it('drops selections that lack a chosen group', () => {
    expect(keepCovering(entities, ['a', 'b'], ['juno'])).toEqual(['b']);
  });
  it('keeps everything when no group is chosen', () => {
    expect(keepCovering(entities, ['a', 'b'], [])).toEqual(['a', 'b']);
  });
  it('keeps ids that have no entity in the list', () => {
    expect(keepCovering(entities, ['gone'], ['juno'])).toEqual(['gone']);
  });
});

describe('duplicateNames', () => {
  it('finds repeated names only', () => {
    const found = duplicateNames([entity('a', 'x', []), entity('b', 'x', []), entity('c', 'y', [])]);
    expect([...found]).toEqual(['x']);
  });
});

describe('describeOption', () => {
  it('shows an available entity groups', () => {
    expect(describeOption(entity('12345678-aaaa', 'x', ['minerva']), ['minerva'], false)).toEqual({ note: 'in minerva', disabled: false });
  });
  it('disables an entity that lacks a group and names it', () => {
    expect(describeOption(entity('12345678-aaaa', 'x', ['minerva']), ['juno'], false)).toEqual({ note: 'not in juno', disabled: true });
  });
  it('adds a short id to tell same-named entities apart', () => {
    expect(describeOption(entity('12345678-aaaa', 'x', ['minerva']), [], true).note).toBe('in minerva; #12345678');
  });
});
