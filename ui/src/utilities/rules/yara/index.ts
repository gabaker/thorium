import type { RuleChecker, CheckResult, FormatType } from '../types';
import { parseYaraText } from './parse';
import { validateYaraRules } from './validate';
import { generateYaraSuggestions } from './suggestions';

export class YaraRuleChecker implements RuleChecker {
  format: FormatType = 'yara';

  check(text: string): CheckResult {
    if (!text.trim()) {
      return { diagnostics: [], suggestions: [] };
    }

    const parseResult = parseYaraText(text);

    const parseDiagnostics = parseResult.errors.map((e) => ({
      line: e.line,
      severity: 'error' as const,
      message: e.message,
    }));

    const ruleDiagnostics = validateYaraRules(parseResult);
    const suggestions = generateYaraSuggestions(parseResult);

    return {
      diagnostics: [...parseDiagnostics, ...ruleDiagnostics],
      suggestions,
    };
  }
}
