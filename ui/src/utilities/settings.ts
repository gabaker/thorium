// project imports
import { SystemSettingValue } from '@models/system';

/**
 * Format one system setting's value for display.
 *
 * Scalars are shown as text; lists and nested settings (such as the models settings) are shown as
 * JSON, since their default string conversion would read `[object Object]` or drop the brackets.
 *
 * @param value - The setting's value as the settings API returns it.
 * @returns The text to display (an empty string for a null value).
 */
export function formatSettingValue(value: SystemSettingValue): string {
  if (value === null) return '';
  if (typeof value === 'object') return JSON.stringify(value);
  return String(value);
}

/**
 * Tell whether a setting's value is a list or nested settings rather than a scalar.
 *
 * @param value - The setting's value as the settings API returns it.
 * @returns `true` for a list or an object.
 */
export function isStructuredSetting(value: SystemSettingValue): boolean {
  return value !== null && typeof value === 'object';
}

/**
 * Describe one scalar or list item as a single line.
 *
 * An item with a provider and a base URL (an allowed model endpoint) reads `<provider>  <base_url>`;
 * any other object reads as `key: value` pairs.
 *
 * @param value - The item to describe.
 * @returns The item's text.
 */
function settingItemText(value: SystemSettingValue): string {
  if (value === null || typeof value !== 'object') return formatSettingValue(value);
  if (Array.isArray(value)) return value.map((item) => settingItemText(item)).join(', ');
  const { provider, base_url: baseUrl } = value;
  if (typeof provider === 'string' && typeof baseUrl === 'string') return `${provider}  ${baseUrl}`;
  return Object.entries(value)
    .map(([key, item]) => `${key}: ${settingItemText(item)}`)
    .join(', ');
}

/** One line of a list or nested setting. */
export interface SettingLine {
  /** How many levels this line is nested below the setting */
  depth: number;
  /** The line's text, without any indent */
  text: string;
}

/**
 * Tell whether a setting's value is an empty list or empty nested settings.
 *
 * @param value - The setting's value as the settings API returns it.
 * @returns `true` for an empty list or an object with no keys.
 */
export function isEmptySetting(value: SystemSettingValue): boolean {
  if (Array.isArray(value)) return value.length === 0;
  return value !== null && typeof value === 'object' && Object.keys(value).length === 0;
}

/**
 * Lay a list or nested setting out as lines with the values' own case kept.
 *
 * Each line carries its depth rather than a run of leading spaces, so a line that wraps can keep a hanging indent.
 *
 * @param value - The setting's value as the settings API returns it.
 * @param depth - How many levels this value is nested below the setting.
 * @returns The lines to show, one list item or key per line.
 */
export function formatSettingLines(value: SystemSettingValue, depth = 0): SettingLine[] {
  if (value === null || typeof value !== 'object') return [{ depth, text: formatSettingValue(value) }];
  if (isEmptySetting(value)) return [{ depth, text: '(none)' }];
  if (Array.isArray(value)) return value.map((item) => ({ depth, text: settingItemText(item) }));
  return Object.entries(value).flatMap(([key, item]) => {
    // a key holding a list or nested settings gets its own line with the contents nested below
    if (item !== null && typeof item === 'object') return [{ depth, text: `${key}:` }, ...formatSettingLines(item, depth + 1)];
    return [{ depth, text: `${key}: ${formatSettingValue(item)}` }];
  });
}
