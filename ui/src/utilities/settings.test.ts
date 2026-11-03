import { describe, it, expect } from 'vitest';

// project imports
import { formatSettingLines, formatSettingValue, isEmptySetting, isStructuredSetting, SettingLine } from './settings';

describe('formatSettingValue', () => {
  it('shows scalars as text', () => {
    expect(formatSettingValue(0)).toBe('0');
    expect(formatSettingValue(false)).toBe('false');
    expect(formatSettingValue('abc')).toBe('abc');
  });
  it('shows null as an empty string', () => {
    expect(formatSettingValue(null)).toBe('');
  });
  it('shows lists and nested settings as JSON', () => {
    expect(formatSettingValue([])).toBe('[]');
    expect(formatSettingValue(['/a', '/b'])).toBe('["/a","/b"]');
    expect(formatSettingValue({ allowed_endpoints: [{ provider: 'OpenAiCompatible' }] })).toBe(
      '{"allowed_endpoints":[{"provider":"OpenAiCompatible"}]}',
    );
  });
});

/** Describe lines as `depth:text` so a test reads like the layout. */
const flat = (lines: SettingLine[]): string[] => lines.map(({ depth, text }) => `${depth}:${text}`);

describe('formatSettingLines', () => {
  it('tells structured settings from scalars', () => {
    expect(isStructuredSetting([])).toBe(true);
    expect(isStructuredSetting({})).toBe(true);
    expect(isStructuredSetting(null)).toBe(false);
    expect(isStructuredSetting('x')).toBe(false);
  });
  it('lists the models allow list as provider and base URL pairs with their case kept', () => {
    expect(
      flat(
        formatSettingLines({
          allowed_endpoints: [
            { provider: 'OpenAiCompatible', base_url: 'http://Jup-Mock.svc:8080/m/stub-model-a/v1' },
            { provider: 'Anthropic', base_url: 'https://api.anthropic.com' },
          ],
        }),
      ),
    ).toEqual([
      '0:allowed_endpoints:',
      '1:OpenAiCompatible  http://Jup-Mock.svc:8080/m/stub-model-a/v1',
      '1:Anthropic  https://api.anthropic.com',
    ]);
  });
  it('lists a plain list one item per line and marks an empty one', () => {
    expect(flat(formatSettingLines(['/a', '/b']))).toEqual(['0:/a', '0:/b']);
    expect(flat(formatSettingLines([]))).toEqual(['0:(none)']);
    expect(flat(formatSettingLines({}))).toEqual(['0:(none)']);
  });
  it('tells an empty list or object from one with contents', () => {
    expect(isEmptySetting([])).toBe(true);
    expect(isEmptySetting({})).toBe(true);
    expect(isEmptySetting(['/a'])).toBe(false);
    expect(isEmptySetting({ a: 1 })).toBe(false);
    expect(isEmptySetting(null)).toBe(false);
    expect(isEmptySetting(0)).toBe(false);
  });
  it('shows other nested objects as key and value pairs', () => {
    expect(flat(formatSettingLines({ limit: 3, nested: { on: true }, rows: [{ a: 1, b: 'x' }] }))).toEqual([
      '0:limit: 3',
      '0:nested:',
      '1:on: true',
      '0:rows:',
      '1:a: 1, b: x',
    ]);
  });
});
