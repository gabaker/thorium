import type { Document } from 'yaml';
import { isMap, isPair, isScalar } from 'yaml';
import { FieldValueType, type Suggestion, type FieldSchema } from '../types';
import { buildLineIndex, offsetToLineCol, type LineIndex } from '../yaml';
import {
  IMAGE_SCALER_VALUES,
  OUTPUT_DISPLAY_TYPE_VALUES,
  DEPENDENCY_PASS_STRATEGY_VALUES,
  FILE_NAMING_STRATEGY_VALUES,
  VOLUME_TYPE_VALUES,
  LIFETIME_COUNTER_VALUES,
  OUTPUT_HANDLER_VALUES,
  IMAGE_FIELD_SCHEMAS,
  PIPELINE_FIELD_SCHEMAS,
} from './schema';

function lookupSchema(schemas: Record<string, FieldSchema>, dottedField: string): FieldSchema | undefined {
  const parts = dottedField.split('.');
  let schema = schemas[parts[0]];
  for (let i = 1; i < parts.length && schema; i++) {
    if (schema.type === FieldValueType.Object && schema.fields) {
      schema = schema.fields[parts[i]];
    } else {
      return undefined;
    }
  }
  return schema;
}

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
      suggestions.push({
        line,
        field: 'scaler',
        message: 'Valid scaler values',
        values: IMAGE_SCALER_VALUES,
        schema: lookupSchema(IMAGE_FIELD_SCHEMAS, 'scaler'),
      });
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
        schema: lookupSchema(IMAGE_FIELD_SCHEMAS, 'display_type'),
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
        schema: lookupSchema(IMAGE_FIELD_SCHEMAS, 'lifetime.counter'),
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
            schema: { type: FieldValueType.Enum, enumValues: DEPENDENCY_PASS_STRATEGY_VALUES },
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
          schema: { type: FieldValueType.Enum, enumValues: FILE_NAMING_STRATEGY_VALUES },
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
        schema: lookupSchema(IMAGE_FIELD_SCHEMAS, 'output_collection.handler'),
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
            schema: { type: FieldValueType.Enum, enumValues: VOLUME_TYPE_VALUES },
          });
          break;
        }
      }
    }
  }

  const missingOptional: Array<{ field: string; message: string; values?: readonly string[]; schema?: FieldSchema }> = [];

  if (!('description' in parsed)) {
    missingOptional.push({
      field: 'description',
      message: 'Consider adding a description for this image',
      schema: lookupSchema(IMAGE_FIELD_SCHEMAS, 'description'),
    });
  }
  if (!('image' in parsed)) {
    missingOptional.push({
      field: 'image',
      message: 'Consider specifying the Docker image URL or tag',
      schema: lookupSchema(IMAGE_FIELD_SCHEMAS, 'image'),
    });
  }
  if (!('timeout' in parsed)) {
    missingOptional.push({
      field: 'timeout',
      message: 'Consider setting a job timeout in seconds',
      schema: lookupSchema(IMAGE_FIELD_SCHEMAS, 'timeout'),
    });
  }
  if (!('scaler' in parsed)) {
    missingOptional.push({
      field: 'scaler',
      message: 'Consider specifying the scaler type',
      values: IMAGE_SCALER_VALUES,
      schema: lookupSchema(IMAGE_FIELD_SCHEMAS, 'scaler'),
    });
  }
  if (!('display_type' in parsed)) {
    missingOptional.push({
      field: 'display_type',
      message: 'Consider specifying the output display type',
      values: OUTPUT_DISPLAY_TYPE_VALUES,
      schema: lookupSchema(IMAGE_FIELD_SCHEMAS, 'display_type'),
    });
  }
  if (!('lifetime' in parsed)) {
    missingOptional.push({
      field: 'lifetime',
      message: 'Consider adding a pod lifetime (jobs or time based)',
      schema: lookupSchema(IMAGE_FIELD_SCHEMAS, 'lifetime'),
    });
  }
  if (!('resources' in parsed)) {
    missingOptional.push({
      field: 'resources',
      message: 'Consider specifying resource requirements (cpu, memory)',
      schema: lookupSchema(IMAGE_FIELD_SCHEMAS, 'resources'),
    });
  }
  if (!('dependencies' in parsed)) {
    missingOptional.push({
      field: 'dependencies',
      message: 'Consider configuring dependency settings',
      schema: lookupSchema(IMAGE_FIELD_SCHEMAS, 'dependencies'),
    });
  }

  for (const opt of missingOptional) {
    suggestions.push({
      line: endLine,
      field: opt.field,
      message: opt.message,
      values: opt.values,
      schema: opt.schema,
    });
  }

  return suggestions;
}

export function generatePipelineSuggestions(doc: Document, text: string, parsed: Record<string, unknown>): Suggestion[] {
  const suggestions: Suggestion[] = [];
  const lineIndex = buildLineIndex(text);
  const endLine = lastDocLine(lineIndex);

  const missingOptional: Array<{ field: string; message: string; schema?: FieldSchema }> = [];

  if (!('description' in parsed)) {
    missingOptional.push({
      field: 'description',
      message: 'Consider adding a pipeline description',
      schema: lookupSchema(PIPELINE_FIELD_SCHEMAS, 'description'),
    });
  }
  if (!('sla' in parsed)) {
    missingOptional.push({
      field: 'sla',
      message: 'Consider setting an SLA in seconds (defaults to 1 week)',
      schema: lookupSchema(PIPELINE_FIELD_SCHEMAS, 'sla'),
    });
  }
  if (!('triggers' in parsed)) {
    missingOptional.push({
      field: 'triggers',
      message: 'Consider adding event triggers for automatic pipeline execution',
      schema: lookupSchema(PIPELINE_FIELD_SCHEMAS, 'triggers'),
    });
  }

  for (const opt of missingOptional) {
    suggestions.push({
      line: endLine,
      field: opt.field,
      message: opt.message,
      schema: opt.schema,
    });
  }

  return suggestions;
}
