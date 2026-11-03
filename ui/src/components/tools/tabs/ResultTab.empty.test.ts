import { describe, it, expect } from 'vitest';

// project imports
import { effectiveDisplayType, isEmptyResult } from './ResultTab';
import { OutputDisplayType } from '@models/results';

describe('isEmptyResult', () => {
  it('treats null/undefined as empty', () => {
    expect(isEmptyResult(null)).toBe(true);
    expect(isEmptyResult(undefined as never)).toBe(true);
  });

  it('treats empty/whitespace strings as empty', () => {
    expect(isEmptyResult('')).toBe(true);
    expect(isEmptyResult('   ')).toBe(true);
  });

  it('treats literal "{}" and "[]" strings as empty', () => {
    expect(isEmptyResult('{}')).toBe(true);
    expect(isEmptyResult(' [] ')).toBe(true);
  });

  it('treats an object with no keys as empty (json with no keys)', () => {
    expect(isEmptyResult({})).toBe(true);
  });

  it('treats an empty array as empty', () => {
    expect(isEmptyResult([])).toBe(true);
  });

  it('treats populated objects/arrays/strings as non-empty', () => {
    expect(isEmptyResult({ a: 1 })).toBe(false);
    expect(isEmptyResult([1])).toBe(false);
    expect(isEmptyResult('result text')).toBe(false);
  });

  it('treats falsy scalars (0, false) as non-empty values', () => {
    expect(isEmptyResult(0)).toBe(false);
    expect(isEmptyResult(false)).toBe(false);
  });
});

describe('effectiveDisplayType', () => {
  it('draws a Custom result with no custom renderer as JSON', () => {
    expect(effectiveDisplayType(OutputDisplayType.Custom, 'legacy-stub')).toBe(OutputDisplayType.Json);
  });

  it('keeps Custom for the tools that have a custom renderer', () => {
    for (const tool of ['symantec', 'clamav', 'vbaextraction', 'tc2', 'titanium-core2']) {
      expect(effectiveDisplayType(OutputDisplayType.Custom, tool)).toBe(OutputDisplayType.Custom);
    }
  });

  it('leaves every other display type unchanged', () => {
    expect(effectiveDisplayType(OutputDisplayType.Table, 'legacy-stub')).toBe(OutputDisplayType.Table);
    expect(effectiveDisplayType(OutputDisplayType.Json, 'symantec')).toBe(OutputDisplayType.Json);
  });
});
