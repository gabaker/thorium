import type { RuleChecker, CheckResult, FormatType } from '../types';
import { parseYaml } from '../yaml';
import { validateImageRequest, validatePipelineRequest } from './validate';
import { generateImageSuggestions, generatePipelineSuggestions } from './suggestions';

export class ImageChecker implements RuleChecker {
  format: FormatType = 'yaml';

  check(text: string): CheckResult {
    const { doc, value, diagnostics: syntaxDiagnostics } = parseYaml(text);

    if (syntaxDiagnostics.some((d) => d.severity === 'error') || !doc || !value) {
      return { diagnostics: syntaxDiagnostics, suggestions: [] };
    }

    if (typeof value !== 'object' || Array.isArray(value)) {
      return {
        diagnostics: [
          ...syntaxDiagnostics,
          { line: 1, column: 1, severity: 'error', message: 'Image request must be a YAML mapping (key-value pairs), not a list or scalar' },
        ],
        suggestions: [],
      };
    }

    const parsed = value as Record<string, unknown>;
    const ruleDiagnostics = validateImageRequest(doc, text, parsed);
    const suggestions = generateImageSuggestions(doc, text, parsed);

    return {
      diagnostics: [...syntaxDiagnostics, ...ruleDiagnostics],
      suggestions,
    };
  }
}

export class PipelineChecker implements RuleChecker {
  format: FormatType = 'yaml';

  check(text: string): CheckResult {
    const { doc, value, diagnostics: syntaxDiagnostics } = parseYaml(text);

    if (syntaxDiagnostics.some((d) => d.severity === 'error') || !doc || !value) {
      return { diagnostics: syntaxDiagnostics, suggestions: [] };
    }

    if (typeof value !== 'object' || Array.isArray(value)) {
      return {
        diagnostics: [
          ...syntaxDiagnostics,
          { line: 1, column: 1, severity: 'error', message: 'Pipeline request must be a YAML mapping (key-value pairs), not a list or scalar' },
        ],
        suggestions: [],
      };
    }

    const parsed = value as Record<string, unknown>;
    const ruleDiagnostics = validatePipelineRequest(doc, text, parsed);
    const suggestions = generatePipelineSuggestions(doc, text, parsed);

    return {
      diagnostics: [...syntaxDiagnostics, ...ruleDiagnostics],
      suggestions,
    };
  }
}
