import type { Document } from 'yaml';
import { isMap, isPair, isScalar } from 'yaml';
import type { Suggestion } from '../types';
import { buildLineIndex, offsetToLineCol, type LineIndex } from '../yaml';
import {
  IMAGE_SCALER_VALUES,
  OUTPUT_DISPLAY_TYPE_VALUES,
  DEPENDENCY_PASS_STRATEGY_VALUES,
  FILE_NAMING_STRATEGY_VALUES,
  VOLUME_TYPE_VALUES,
  LIFETIME_COUNTER_VALUES,
  OUTPUT_HANDLER_VALUES,
} from './schema';

function findKeyLine(map: unknown, key: string, lineIndex: LineIndex): number {
  if (!isMap(map)) return 1;
  for (const item of map.items) {
    if (isPair(item) && isScalar(item.key) && item.key.value === key) {
      const offset = item.key.range?.[0] ?? 0;
      return offsetToLineCol(lineIndex, offset).line;
    }
  }
  return 1;
}

function lastDocLine(lineIndex: LineIndex): number {
  return lineIndex.offsets.length;
}

export function generateImageSuggestions(doc: Document, text: string, parsed: Record<string, unknown>): Suggestion[] {
  const suggestions: Suggestion[] = [];
  const lineIndex = buildLineIndex(text);
  const endLine = lastDocLine(lineIndex);

  if ('scaler' in parsed) {
    const line = findKeyLine(doc.contents, 'scaler', lineIndex);
    if (typeof parsed['scaler'] !== 'string' || parsed['scaler'] === '') {
      suggestions.push({ line, field: 'scaler', message: 'Valid scaler values', values: IMAGE_SCALER_VALUES });
    }
  }

  if ('display_type' in parsed) {
    const line = findKeyLine(doc.contents, 'display_type', lineIndex);
    if (typeof parsed['display_type'] !== 'string' || parsed['display_type'] === '') {
      suggestions.push({
        line,
        field: 'display_type',
        message: 'Valid display type values',
        values: OUTPUT_DISPLAY_TYPE_VALUES,
      });
    }
  }

  if ('lifetime' in parsed && typeof parsed['lifetime'] === 'object' && parsed['lifetime'] !== null) {
    const lt = parsed['lifetime'] as Record<string, unknown>;
    const lifetimeLine = findKeyLine(doc.contents, 'lifetime', lineIndex);
    if (!('counter' in lt) || lt['counter'] === '' || lt['counter'] === null) {
      suggestions.push({
        line: lifetimeLine,
        field: 'lifetime.counter',
        message: 'Valid lifetime counter values',
        values: LIFETIME_COUNTER_VALUES,
      });
    }
  }

  if ('dependencies' in parsed && typeof parsed['dependencies'] === 'object' && parsed['dependencies'] !== null) {
    const deps = parsed['dependencies'] as Record<string, unknown>;
    const depLine = findKeyLine(doc.contents, 'dependencies', lineIndex);

    const strategySubSections = ['samples', 'ephemeral', 'results', 'repos', 'tags', 'children'] as const;
    for (const section of strategySubSections) {
      if (section in deps && typeof deps[section] === 'object' && deps[section] !== null) {
        const sub = deps[section] as Record<string, unknown>;
        if ('strategy' in sub && (typeof sub['strategy'] !== 'string' || sub['strategy'] === '')) {
          suggestions.push({
            line: depLine,
            field: `dependencies.${section}.strategy`,
            message: 'Valid dependency pass strategy values',
            values: DEPENDENCY_PASS_STRATEGY_VALUES,
          });
        }
      }
    }

    if ('samples' in deps && typeof deps['samples'] === 'object' && deps['samples'] !== null) {
      const samples = deps['samples'] as Record<string, unknown>;
      if ('naming' in samples && (typeof samples['naming'] !== 'string' || samples['naming'] === '')) {
        suggestions.push({
          line: depLine,
          field: 'dependencies.samples.naming',
          message: 'Valid file naming strategy values',
          values: FILE_NAMING_STRATEGY_VALUES,
        });
      }
    }
  }

  if ('output_collection' in parsed && typeof parsed['output_collection'] === 'object' && parsed['output_collection'] !== null) {
    const oc = parsed['output_collection'] as Record<string, unknown>;
    const ocLine = findKeyLine(doc.contents, 'output_collection', lineIndex);
    if ('handler' in oc && (typeof oc['handler'] !== 'string' || oc['handler'] === '')) {
      suggestions.push({
        line: ocLine,
        field: 'output_collection.handler',
        message: 'Valid output handler values',
        values: OUTPUT_HANDLER_VALUES,
      });
    }
  }

  if ('volumes' in parsed && Array.isArray(parsed['volumes'])) {
    const volLine = findKeyLine(doc.contents, 'volumes', lineIndex);
    for (const vol of parsed['volumes']) {
      if (typeof vol === 'object' && vol !== null) {
        const v = vol as Record<string, unknown>;
        if ('archetype' in v && (typeof v['archetype'] !== 'string' || v['archetype'] === '')) {
          suggestions.push({
            line: volLine,
            field: 'volumes[].archetype',
            message: 'Valid volume type values',
            values: VOLUME_TYPE_VALUES,
          });
          break;
        }
      }
    }
  }

  const missingOptional: Array<{ field: string; message: string; values?: readonly string[] }> = [];

  if (!('description' in parsed)) {
    missingOptional.push({ field: 'description', message: 'Consider adding a description for this image' });
  }
  if (!('image' in parsed)) {
    missingOptional.push({ field: 'image', message: 'Consider specifying the Docker image URL or tag' });
  }
  if (!('timeout' in parsed)) {
    missingOptional.push({ field: 'timeout', message: 'Consider setting a job timeout in seconds' });
  }
  if (!('scaler' in parsed)) {
    missingOptional.push({
      field: 'scaler',
      message: 'Consider specifying the scaler type',
      values: IMAGE_SCALER_VALUES,
    });
  }
  if (!('display_type' in parsed)) {
    missingOptional.push({
      field: 'display_type',
      message: 'Consider specifying the output display type',
      values: OUTPUT_DISPLAY_TYPE_VALUES,
    });
  }
  if (!('lifetime' in parsed)) {
    missingOptional.push({ field: 'lifetime', message: 'Consider adding a pod lifetime (jobs or time based)' });
  }
  if (!('resources' in parsed)) {
    missingOptional.push({ field: 'resources', message: 'Consider specifying resource requirements (cpu, memory)' });
  }
  if (!('dependencies' in parsed)) {
    missingOptional.push({ field: 'dependencies', message: 'Consider configuring dependency settings' });
  }

  for (const opt of missingOptional) {
    suggestions.push({
      line: endLine,
      field: opt.field,
      message: opt.message,
      values: opt.values,
    });
  }

  return suggestions;
}

export function generatePipelineSuggestions(doc: Document, text: string, parsed: Record<string, unknown>): Suggestion[] {
  const suggestions: Suggestion[] = [];
  const lineIndex = buildLineIndex(text);
  const endLine = lastDocLine(lineIndex);

  const missingOptional: Array<{ field: string; message: string }> = [];

  if (!('description' in parsed)) {
    missingOptional.push({ field: 'description', message: 'Consider adding a pipeline description' });
  }
  if (!('sla' in parsed)) {
    missingOptional.push({ field: 'sla', message: 'Consider setting an SLA in seconds (defaults to 1 week)' });
  }
  if (!('triggers' in parsed)) {
    missingOptional.push({ field: 'triggers', message: 'Consider adding event triggers for automatic pipeline execution' });
  }

  for (const opt of missingOptional) {
    suggestions.push({
      line: endLine,
      field: opt.field,
      message: opt.message,
    });
  }

  return suggestions;
}
