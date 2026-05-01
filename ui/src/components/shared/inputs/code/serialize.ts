import { stringify as yamlStringify } from 'yaml';
import type { FormatType } from '@utilities/rules/types';

export function toText(value: unknown, format: FormatType): string {
  if (value === null || value === undefined) {
    return '';
  }

  switch (format) {
    case 'yaml':
      return yamlStringify(value, { indent: 4, lineWidth: 0 });
    case 'json':
      return JSON.stringify(value, null, 2);
    case 'yara':
      return typeof value === 'string' ? value : '';
    default:
      return JSON.stringify(value);
  }
}
