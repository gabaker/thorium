export function includes<T extends string>(arr: readonly T[], val: string): val is T {
  return (arr as readonly string[]).includes(val);
}

export type DiagnosticSeverity = 'error' | 'warning' | 'info';

export interface Diagnostic {
  line: number;
  column?: number;
  endLine?: number;
  endColumn?: number;
  severity: DiagnosticSeverity;
  message: string;
}

export interface Suggestion {
  line: number;
  lineEnd?: number;
  field: string;
  message: string;
  values?: readonly string[];
  isList?: boolean;
}

export interface CheckResult {
  diagnostics: Diagnostic[];
  suggestions: Suggestion[];
}

export type FormatType = 'yaml' | 'json' | 'yara';

export interface RuleChecker {
  format: FormatType;
  check(text: string): CheckResult;
}
