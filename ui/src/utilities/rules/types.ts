export function includes<T extends string>(arr: readonly T[], val: string): val is T {
  return (arr as readonly string[]).includes(val);
}

export enum Severity {
  Error = 'error',
  Warning = 'warning',
  Info = 'info',
}

export interface Diagnostic {
  line: number;
  column?: number;
  endLine?: number;
  endColumn?: number;
  severity: Severity;
  message: string;
}

export enum FieldValueType {
  String = 'string',
  Number = 'number',
  Boolean = 'boolean',
  Enum = 'enum',
  Object = 'object',
  StringArray = 'string_array',
}

export interface FieldSchema {
  type: FieldValueType;
  required?: boolean;
  placeholder?: string;
  enumValues?: readonly string[];
  fields?: Record<string, FieldSchema>;
}

export interface Suggestion {
  line: number;
  lineEnd?: number;
  field: string;
  message: string;
  values?: readonly string[];
  isList?: boolean;
  schema?: FieldSchema;
}

export interface CheckResult {
  diagnostics: Diagnostic[];
  suggestions: Suggestion[];
}

export enum FormatType {
  YAML = 'yaml',
  JSON = 'json',
  YARA = 'yara',
}

export interface RuleChecker {
  format: FormatType;
  check(text: string): CheckResult;
}
