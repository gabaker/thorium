import type { Document } from 'yaml';
import { isMap, isPair, isScalar, isSeq } from 'yaml';
import { includes, type Diagnostic } from '../types';
import { buildLineIndex, offsetToLineCol, type LineIndex } from '../yaml';
import {
  REQUIRED_IMAGE_FIELDS,
  REQUIRED_PIPELINE_FIELDS,
  KNOWN_IMAGE_FIELDS,
  KNOWN_PIPELINE_FIELDS,
  IMAGE_SCALER_VALUES,
  DEPENDENCY_PASS_STRATEGY_VALUES,
  FILE_NAMING_STRATEGY_VALUES,
  OUTPUT_DISPLAY_TYPE_VALUES,
  VOLUME_TYPE_VALUES,
  HOST_PATH_TYPE_VALUES,
  LIFETIME_COUNTER_VALUES,
  OUTPUT_HANDLER_VALUES,
  KNOWN_RESOURCES_FIELDS,
  KNOWN_ARGS_FIELDS,
  KNOWN_DEPENDENCIES_FIELDS,
  KNOWN_SAMPLE_DEP_FIELDS,
  KNOWN_REPO_DEP_FIELDS,
  KNOWN_TAG_DEP_FIELDS,
  KNOWN_CHILDREN_DEP_FIELDS,
  KNOWN_EPHEMERAL_DEP_FIELDS,
  KNOWN_RESULT_DEP_FIELDS,
  KNOWN_CACHE_DEP_FIELDS,
  KNOWN_OUTPUT_COLLECTION_FIELDS,
  KNOWN_FILES_HANDLER_FIELDS,
  KNOWN_CHILD_FILTERS_FIELDS,
  KNOWN_CLEANUP_FIELDS,
  KNOWN_SECURITY_CONTEXT_FIELDS,
  KNOWN_LIFETIME_FIELDS,
  KNOWN_VOLUME_FIELDS,
  KNOWN_KVM_FIELDS,
  KNOWN_BURSTABLE_FIELDS,
} from './schema';

function nodeLineCol(
  node: { range?: [number, number, number] | [number, number] | null | undefined },
  lineIndex: LineIndex,
): { line: number; column: number } {
  const offset = node.range?.[0] ?? 0;
  return offsetToLineCol(lineIndex, offset);
}

function findMapKey(map: unknown, key: string) {
  if (!isMap(map)) return null;
  for (const item of map.items) {
    if (isPair(item) && isScalar(item.key) && item.key.value === key) {
      return item.key;
    }
  }
  return null;
}

function findMapValue(map: unknown, key: string) {
  if (!isMap(map)) return null;
  for (const item of map.items) {
    if (isPair(item) && isScalar(item.key) && item.key.value === key) {
      return item.value;
    }
  }
  return null;
}

function nodePosition(node: unknown, lineIndex: LineIndex): { line: number; column: number } {
  if (node && typeof node === 'object' && 'range' in node) {
    const range = (node as Record<string, unknown>).range;
    if (Array.isArray(range) && typeof range[0] === 'number') {
      return offsetToLineCol(lineIndex, range[0]);
    }
  }
  return { line: 1, column: 1 };
}

function validateUnknownFields(
  map: unknown,
  knownFields: readonly string[],
  parentLabel: string,
  parsed: Record<string, unknown>,
  lineIndex: LineIndex,
  diagnostics: Diagnostic[],
) {
  const fieldList = knownFields.join(', ');
  for (const key of Object.keys(parsed)) {
    const fieldName: string = key;
    if (!includes(knownFields, key)) {
      const node = findMapKey(map, fieldName);
      const pos = node ? nodeLineCol(node, lineIndex) : { line: 1, column: 1 };
      diagnostics.push({
        ...pos,
        severity: 'warning',
        message: `Unknown ${parentLabel} field: '${fieldName}'. Known fields: ${fieldList}`,
      });
    }
  }
}

function validateEnumField(
  map: unknown,
  parsed: Record<string, unknown>,
  field: string,
  values: readonly string[],
  lineIndex: LineIndex,
  diagnostics: Diagnostic[],
) {
  if (!(field in parsed)) return;
  const val = parsed[field];
  const valueList = values.join(', ');
  if (typeof val === 'string') {
    const strVal: string = val;
    if (!includes(values, val)) {
      const node = findMapValue(map, field);
      const pos = nodePosition(node, lineIndex);
      diagnostics.push({
        ...pos,
        severity: 'error',
        message: `Invalid ${field} value: '${strVal}'. Must be one of: ${valueList}`,
      });
    }
  }
}

function validateNumberField(
  map: unknown,
  parsed: Record<string, unknown>,
  field: string,
  lineIndex: LineIndex,
  diagnostics: Diagnostic[],
) {
  if (!(field in parsed)) return;
  if (typeof parsed[field] !== 'number') {
    const node = findMapValue(map, field);
    const pos = nodePosition(node, lineIndex);
    diagnostics.push({
      ...pos,
      severity: 'error',
      message: `'${field}' must be a number`,
    });
  }
}

function validateBooleanField(
  map: unknown,
  parsed: Record<string, unknown>,
  field: string,
  lineIndex: LineIndex,
  diagnostics: Diagnostic[],
) {
  if (!(field in parsed)) return;
  if (typeof parsed[field] !== 'boolean') {
    const node = findMapValue(map, field);
    const pos = nodePosition(node, lineIndex);
    diagnostics.push({
      ...pos,
      severity: 'error',
      message: `'${field}' must be a boolean (true/false)`,
    });
  }
}

function validateStringField(
  map: unknown,
  parsed: Record<string, unknown>,
  field: string,
  lineIndex: LineIndex,
  diagnostics: Diagnostic[],
) {
  if (!(field in parsed)) return;
  if (typeof parsed[field] !== 'string') {
    const node = findMapValue(map, field);
    const pos = nodePosition(node, lineIndex);
    diagnostics.push({
      ...pos,
      severity: 'error',
      message: `'${field}' must be a string`,
    });
  }
}

function validateObjectField(
  map: unknown,
  parsed: Record<string, unknown>,
  field: string,
  lineIndex: LineIndex,
  diagnostics: Diagnostic[],
): Record<string, unknown> | null {
  if (!(field in parsed)) return null;
  const val = parsed[field];
  if (typeof val !== 'object' || val === null || Array.isArray(val)) {
    const node = findMapValue(map, field);
    const pos = nodePosition(node, lineIndex);
    diagnostics.push({
      ...pos,
      severity: 'error',
      message: `'${field}' must be an object`,
    });
    return null;
  }
  return val as Record<string, unknown>;
}

function validateSubObject(
  parentMap: unknown,
  parsed: Record<string, unknown>,
  field: string,
  knownFields: readonly string[],
  lineIndex: LineIndex,
  diagnostics: Diagnostic[],
) {
  const obj = validateObjectField(parentMap, parsed, field, lineIndex, diagnostics);
  if (!obj) return;
  const subMap = findMapValue(parentMap, field);
  validateUnknownFields(subMap, knownFields, field, obj, lineIndex, diagnostics);
}

function validateResources(
  parentMap: unknown,
  parsed: Record<string, unknown>,
  lineIndex: LineIndex,
  diagnostics: Diagnostic[],
) {
  const obj = validateObjectField(parentMap, parsed, 'resources', lineIndex, diagnostics);
  if (!obj) return;
  const subMap = findMapValue(parentMap, 'resources');
  validateUnknownFields(subMap, KNOWN_RESOURCES_FIELDS, 'resources', obj, lineIndex, diagnostics);
  for (const numField of ['cpu', 'memory', 'ephemeral_storage', 'worker_slots', 'nvidia_gpu', 'amd_gpu'] as const) {
    validateNumberField(subMap, obj, numField, lineIndex, diagnostics);
  }
  if ('burstable' in obj) {
    const burst = validateObjectField(subMap, obj, 'burstable', lineIndex, diagnostics);
    if (burst) {
      const burstMap = findMapValue(subMap, 'burstable');
      validateUnknownFields(burstMap, KNOWN_BURSTABLE_FIELDS, 'burstable', burst, lineIndex, diagnostics);
      validateNumberField(burstMap, burst, 'cpu', lineIndex, diagnostics);
      validateNumberField(burstMap, burst, 'memory', lineIndex, diagnostics);
    }
  }
}

function validateArgs(
  parentMap: unknown,
  parsed: Record<string, unknown>,
  lineIndex: LineIndex,
  diagnostics: Diagnostic[],
) {
  const obj = validateObjectField(parentMap, parsed, 'args', lineIndex, diagnostics);
  if (!obj) return;
  const subMap = findMapValue(parentMap, 'args');
  validateUnknownFields(subMap, KNOWN_ARGS_FIELDS, 'args', obj, lineIndex, diagnostics);
}

function validateDepSubSection(
  depMap: unknown,
  deps: Record<string, unknown>,
  field: string,
  knownFields: readonly string[],
  lineIndex: LineIndex,
  diagnostics: Diagnostic[],
) {
  const obj = validateObjectField(depMap, deps, field, lineIndex, diagnostics);
  if (!obj) return;
  const subMap = findMapValue(depMap, field);
  validateUnknownFields(subMap, knownFields, `dependencies.${field}`, obj, lineIndex, diagnostics);
  if ('strategy' in obj) {
    validateEnumField(subMap, obj, 'strategy', DEPENDENCY_PASS_STRATEGY_VALUES, lineIndex, diagnostics);
  }
  if ('naming' in obj) {
    validateEnumField(subMap, obj, 'naming', FILE_NAMING_STRATEGY_VALUES, lineIndex, diagnostics);
  }
}

function validateDependencies(
  parentMap: unknown,
  parsed: Record<string, unknown>,
  lineIndex: LineIndex,
  diagnostics: Diagnostic[],
) {
  const obj = validateObjectField(parentMap, parsed, 'dependencies', lineIndex, diagnostics);
  if (!obj) return;
  const depMap = findMapValue(parentMap, 'dependencies');
  validateUnknownFields(depMap, KNOWN_DEPENDENCIES_FIELDS, 'dependencies', obj, lineIndex, diagnostics);
  validateDepSubSection(depMap, obj, 'samples', KNOWN_SAMPLE_DEP_FIELDS, lineIndex, diagnostics);
  validateDepSubSection(depMap, obj, 'repos', KNOWN_REPO_DEP_FIELDS, lineIndex, diagnostics);
  validateDepSubSection(depMap, obj, 'tags', KNOWN_TAG_DEP_FIELDS, lineIndex, diagnostics);
  validateDepSubSection(depMap, obj, 'children', KNOWN_CHILDREN_DEP_FIELDS, lineIndex, diagnostics);
  validateDepSubSection(depMap, obj, 'ephemeral', KNOWN_EPHEMERAL_DEP_FIELDS, lineIndex, diagnostics);
  validateDepSubSection(depMap, obj, 'results', KNOWN_RESULT_DEP_FIELDS, lineIndex, diagnostics);
  validateDepSubSection(depMap, obj, 'cache', KNOWN_CACHE_DEP_FIELDS, lineIndex, diagnostics);
}

function validateOutputCollection(
  parentMap: unknown,
  parsed: Record<string, unknown>,
  lineIndex: LineIndex,
  diagnostics: Diagnostic[],
) {
  const obj = validateObjectField(parentMap, parsed, 'output_collection', lineIndex, diagnostics);
  if (!obj) return;
  const subMap = findMapValue(parentMap, 'output_collection');
  validateUnknownFields(subMap, KNOWN_OUTPUT_COLLECTION_FIELDS, 'output_collection', obj, lineIndex, diagnostics);
  validateEnumField(subMap, obj, 'handler', OUTPUT_HANDLER_VALUES, lineIndex, diagnostics);
  if ('files' in obj) {
    validateSubObject(subMap, obj, 'files', KNOWN_FILES_HANDLER_FIELDS, lineIndex, diagnostics);
  }
}

function validateChildFilters(
  parentMap: unknown,
  parsed: Record<string, unknown>,
  lineIndex: LineIndex,
  diagnostics: Diagnostic[],
) {
  validateSubObject(parentMap, parsed, 'child_filters', KNOWN_CHILD_FILTERS_FIELDS, lineIndex, diagnostics);
}

function validateVolumes(
  parentMap: unknown,
  parsed: Record<string, unknown>,
  lineIndex: LineIndex,
  diagnostics: Diagnostic[],
) {
  if (!('volumes' in parsed)) return;
  if (!Array.isArray(parsed['volumes'])) {
    const node = findMapValue(parentMap, 'volumes');
    const pos = nodePosition(node, lineIndex);
    diagnostics.push({ ...pos, severity: 'error', message: "'volumes' must be an array" });
    return;
  }
  const volsNode = findMapValue(parentMap, 'volumes');
  if (!isSeq(volsNode)) return;
  const volumes = parsed['volumes'] as unknown[];
  for (let i = 0; i < volsNode.items.length; i++) {
    const item = volsNode.items[i];
    const vol = volumes[i];
    if (typeof vol !== 'object' || vol === null || Array.isArray(vol)) {
      const pos = nodePosition(item, lineIndex);
      diagnostics.push({ ...pos, severity: 'error', message: 'Each volume entry must be an object' });
      continue;
    }
    const volObj = vol as Record<string, unknown>;
    validateUnknownFields(item, KNOWN_VOLUME_FIELDS, 'volume', volObj, lineIndex, diagnostics);
    if (!('name' in volObj)) {
      const pos = nodePosition(item, lineIndex);
      diagnostics.push({ ...pos, severity: 'error', message: "Volume is missing required field: 'name'" });
    }
    if (!('mount_path' in volObj)) {
      const pos = nodePosition(item, lineIndex);
      diagnostics.push({ ...pos, severity: 'error', message: "Volume is missing required field: 'mount_path'" });
    }
    if ('archetype' in volObj) {
      validateEnumField(item, volObj, 'archetype', VOLUME_TYPE_VALUES, lineIndex, diagnostics);
    }
    if ('host_path' in volObj && typeof volObj['host_path'] === 'object' && volObj['host_path'] !== null) {
      const hp = volObj['host_path'] as Record<string, unknown>;
      if ('path_type' in hp) {
        const hpMap = findMapValue(item, 'host_path');
        validateEnumField(hpMap, hp, 'path_type', HOST_PATH_TYPE_VALUES, lineIndex, diagnostics);
      }
    }
  }
}

function validateLifetime(
  parentMap: unknown,
  parsed: Record<string, unknown>,
  lineIndex: LineIndex,
  diagnostics: Diagnostic[],
) {
  const obj = validateObjectField(parentMap, parsed, 'lifetime', lineIndex, diagnostics);
  if (!obj) return;
  const subMap = findMapValue(parentMap, 'lifetime');
  validateUnknownFields(subMap, KNOWN_LIFETIME_FIELDS, 'lifetime', obj, lineIndex, diagnostics);
  if ('counter' in obj) {
    validateEnumField(subMap, obj, 'counter', LIFETIME_COUNTER_VALUES, lineIndex, diagnostics);
  }
  validateNumberField(subMap, obj, 'amount', lineIndex, diagnostics);
}

export function validateImageRequest(
  doc: Document,
  text: string,
  parsed: Record<string, unknown>,
): Diagnostic[] {
  const diagnostics: Diagnostic[] = [];
  const lineIndex = buildLineIndex(text);
  const contents = doc.contents;

  const lastLine = lineIndex.offsets.length;
  for (const field of REQUIRED_IMAGE_FIELDS) {
    if (!(field in parsed)) {
      diagnostics.push({
        line: 1,
        column: 1,
        endLine: lastLine,
        severity: 'error',
        message: `Missing required field: '${field}'`,
      });
    }
  }

  validateUnknownFields(contents, KNOWN_IMAGE_FIELDS, 'image', parsed, lineIndex, diagnostics);

  validateStringField(contents, parsed, 'group', lineIndex, diagnostics);
  validateStringField(contents, parsed, 'name', lineIndex, diagnostics);
  validateStringField(contents, parsed, 'image', lineIndex, diagnostics);
  validateStringField(contents, parsed, 'modifiers', lineIndex, diagnostics);
  validateStringField(contents, parsed, 'description', lineIndex, diagnostics);
  validateNumberField(contents, parsed, 'timeout', lineIndex, diagnostics);
  validateBooleanField(contents, parsed, 'collect_logs', lineIndex, diagnostics);
  validateBooleanField(contents, parsed, 'generator', lineIndex, diagnostics);

  validateEnumField(contents, parsed, 'scaler', IMAGE_SCALER_VALUES, lineIndex, diagnostics);
  validateEnumField(contents, parsed, 'display_type', OUTPUT_DISPLAY_TYPE_VALUES, lineIndex, diagnostics);

  validateResources(contents, parsed, lineIndex, diagnostics);
  validateArgs(contents, parsed, lineIndex, diagnostics);
  validateDependencies(contents, parsed, lineIndex, diagnostics);
  validateOutputCollection(contents, parsed, lineIndex, diagnostics);
  validateChildFilters(contents, parsed, lineIndex, diagnostics);
  validateVolumes(contents, parsed, lineIndex, diagnostics);
  validateLifetime(contents, parsed, lineIndex, diagnostics);
  validateSubObject(contents, parsed, 'security_context', KNOWN_SECURITY_CONTEXT_FIELDS, lineIndex, diagnostics);
  validateSubObject(contents, parsed, 'clean_up', KNOWN_CLEANUP_FIELDS, lineIndex, diagnostics);
  validateSubObject(contents, parsed, 'kvm', KNOWN_KVM_FIELDS, lineIndex, diagnostics);

  if ('network_policies' in parsed && !Array.isArray(parsed['network_policies'])) {
    const node = findMapValue(contents, 'network_policies');
    const pos = nodePosition(node, lineIndex);
    diagnostics.push({ ...pos, severity: 'error', message: "'network_policies' must be an array of strings" });
  }

  if ('env' in parsed) {
    validateObjectField(contents, parsed, 'env', lineIndex, diagnostics);
  }

  if ('clean_up' in parsed) {
    const obj = parsed['clean_up'];
    if (typeof obj === 'object' && obj !== null && !Array.isArray(obj)) {
      const cu = obj as Record<string, unknown>;
      if (!('script' in cu)) {
        const node = findMapKey(contents, 'clean_up');
        const pos = node ? nodeLineCol(node, lineIndex) : { line: 1, column: 1 };
        diagnostics.push({ ...pos, severity: 'error', message: "clean_up is missing required field: 'script'" });
      }
    }
  }

  if ('kvm' in parsed) {
    const obj = parsed['kvm'];
    if (typeof obj === 'object' && obj !== null && !Array.isArray(obj)) {
      const kvm = obj as Record<string, unknown>;
      const kvmMap = findMapValue(contents, 'kvm');
      if (!('xml' in kvm)) {
        const pos = nodePosition(kvmMap, lineIndex);
        diagnostics.push({ ...pos, severity: 'error', message: "kvm is missing required field: 'xml'" });
      }
      if (!('qcow2' in kvm)) {
        const pos = nodePosition(kvmMap, lineIndex);
        diagnostics.push({ ...pos, severity: 'error', message: "kvm is missing required field: 'qcow2'" });
      }
    }
  }

  return diagnostics;
}

export function validatePipelineRequest(
  doc: Document,
  text: string,
  parsed: Record<string, unknown>,
): Diagnostic[] {
  const diagnostics: Diagnostic[] = [];
  const lineIndex = buildLineIndex(text);
  const contents = doc.contents;

  const lastLine = lineIndex.offsets.length;
  for (const field of REQUIRED_PIPELINE_FIELDS) {
    if (!(field in parsed)) {
      diagnostics.push({
        line: 1,
        column: 1,
        endLine: lastLine,
        severity: 'error',
        message: `Missing required field: '${field}'`,
      });
    }
  }

  validateUnknownFields(contents, KNOWN_PIPELINE_FIELDS, 'pipeline', parsed, lineIndex, diagnostics);

  validateStringField(contents, parsed, 'group', lineIndex, diagnostics);
  validateStringField(contents, parsed, 'name', lineIndex, diagnostics);
  validateStringField(contents, parsed, 'description', lineIndex, diagnostics);
  validateNumberField(contents, parsed, 'sla', lineIndex, diagnostics);

  if ('order' in parsed) {
    if (!Array.isArray(parsed['order'])) {
      const node = findMapValue(contents, 'order');
      const pos = nodePosition(node, lineIndex);
      diagnostics.push({ ...pos, severity: 'error', message: "'order' must be an array" });
    } else {
      const orderNode = findMapValue(contents, 'order');
      const orderArr = parsed['order'] as unknown[];
      if (isSeq(orderNode)) {
        for (let i = 0; i < orderNode.items.length; i++) {
          const item = orderNode.items[i];
          const val = orderArr[i];
          if (typeof val === 'string') continue;
          if (Array.isArray(val)) {
            for (let j = 0; j < val.length; j++) {
              if (typeof val[j] !== 'string') {
                const pos = nodePosition(item, lineIndex);
                diagnostics.push({
                  ...pos,
                  severity: 'error',
                  message: `order[${i}][${j}] must be a string (image name)`,
                });
              }
            }
          } else {
            const pos = nodePosition(item, lineIndex);
            diagnostics.push({
              ...pos,
              severity: 'error',
              message: `order[${i}] must be a string or array of strings`,
            });
          }
        }
      }
    }
  }

  if ('triggers' in parsed) {
    validateObjectField(contents, parsed, 'triggers', lineIndex, diagnostics);
  }

  return diagnostics;
}
