import { EditorView, Decoration, type DecorationSet, WidgetType } from '@codemirror/view';
import { EditorState, StateField, StateEffect } from '@codemirror/state';
import { FormatType, FieldValueType, type FieldSchema } from '@utilities/rules/types';

export interface PreviewProposal {
  field: string;
  value: string;
  format: FormatType;
  cursorLine?: number;
  isList?: boolean;
  schema?: FieldSchema;
}

export const addPreview = StateEffect.define<PreviewProposal>();
export const clearPreview = StateEffect.define<void>();
export const acceptPreview = StateEffect.define<void>();
const updateInsertText = StateEffect.define<string>();

function lineOffset(lines: string[], lineIdx: number): number {
  let offset = 0;
  for (let k = 0; k < lineIdx; k++) {
    offset += lines[k].length + 1;
  }
  return offset;
}

function buildYaraInsertText(
  field: string,
  value: string,
  docText: string,
  lines: string[],
  cursorLine?: number,
): { text: string; pos: number; inline?: boolean } {
  if (field.startsWith('section.')) {
    const sectionName = field.slice('section.'.length);
    const sectionOrder = ['meta', 'strings', 'condition'];
    const sectionIdx = sectionOrder.indexOf(sectionName);

    const scaffolds: Record<string, string> = {
      meta: '    meta:\n        description = ""\n        author = ""\n',
      strings: '    strings:\n        $s1 = ""\n',
      condition: '    condition:\n        true\n',
    };

    let insertLineIdx = -1;
    for (let s = sectionIdx + 1; s < sectionOrder.length; s++) {
      const re = new RegExp(`^\\s*${sectionOrder[s]}\\s*:`);
      for (let i = 0; i < lines.length; i++) {
        if (re.test(lines[i])) {
          insertLineIdx = i;
          break;
        }
      }
      if (insertLineIdx >= 0) break;
    }

    if (insertLineIdx < 0) {
      for (let i = lines.length - 1; i >= 0; i--) {
        if (lines[i].trim() === '}') {
          insertLineIdx = i;
          break;
        }
      }
    }

    if (insertLineIdx >= 0) {
      const hasBlankBefore = insertLineIdx > 0 && lines[insertLineIdx - 1].trim() === '';
      const scaffold = scaffolds[sectionName] ?? `    ${sectionName}:\n`;
      const insertText = (hasBlankBefore ? '' : '\n') + scaffold + '\n';
      const pos = Math.min(lineOffset(lines, insertLineIdx), docText.length);
      return { text: insertText, pos };
    }
    const insertText = '\n' + (scaffolds[sectionName] ?? `    ${sectionName}:\n`);
    return { text: insertText, pos: docText.length };
  }

  if (field === 'meta' || field.startsWith('meta.')) {
    const metaKey = field === 'meta' ? value : field.split('.').slice(1).join('.');
    const metaVal = field === 'meta' ? `"<value>"` : `"${value}"`;
    let metaIdx = -1;
    let insertAfterIdx = -1;
    for (let i = 0; i < lines.length; i++) {
      if (/^\s*meta\s*:/.test(lines[i])) {
        metaIdx = i;
        insertAfterIdx = i;
        for (let j = i + 1; j < lines.length; j++) {
          if (/^\s+\S/.test(lines[j]) && !/^\s*(strings|condition)\s*:/.test(lines[j])) {
            insertAfterIdx = j;
          } else {
            break;
          }
        }
        break;
      }
    }
    if (metaIdx >= 0) {
      const indent = lines[insertAfterIdx].match(/^(\s*)/)?.[1] ?? '        ';
      const lineEnd = Math.min(lineOffset(lines, insertAfterIdx) + lines[insertAfterIdx].length, docText.length);
      return { text: `\n${indent}${metaKey} = ${metaVal}`, pos: lineEnd };
    }
    return { text: `\n        ${metaKey} = ${metaVal}`, pos: docText.length };
  }

  if (field === 'import') {
    return { text: `import "${value}"\n`, pos: 0 };
  }

  if (field === 'strings.modifiers' && cursorLine != null) {
    const lineIdx = Math.min(cursorLine - 1, lines.length - 1);
    const lineEnd = Math.min(lineOffset(lines, lineIdx + 1) - 1, docText.length);
    return { text: ` ${value}`, pos: lineEnd, inline: true };
  }

  return { text: `${value}\n`, pos: docText.length };
}

function defaultValueForType(schema: FieldSchema): string {
  switch (schema.type) {
    case FieldValueType.Number:
      return schema.placeholder ?? '';
    case FieldValueType.Boolean:
      return 'false';
    case FieldValueType.Enum:
      return schema.enumValues?.[0] ?? '';
    case FieldValueType.String:
      return '';
    default:
      return '';
  }
}

function formatValueForYaml(value: string, schema?: FieldSchema): string {
  if (!schema || !value) return `'${value}'`;
  switch (schema.type) {
    case FieldValueType.Number:
      return value;
    case FieldValueType.Boolean:
      return value;
    case FieldValueType.Enum:
      return value;
    default:
      return `'${value}'`;
  }
}

function buildObjectYamlText(field: string, schema: FieldSchema, values: Record<string, string>, indent: string = ''): string {
  if (!schema.fields) return `${indent}${field}: ''\n`;
  let text = `${indent}${field}:\n`;
  for (const [subKey, subSchema] of Object.entries(schema.fields)) {
    const val = values[subKey] ?? '';
    if (!val && !subSchema.required) continue;
    if (subSchema.type === FieldValueType.Object) continue;
    text += `${indent}    ${subKey}: ${formatValueForYaml(val, subSchema)}\n`;
  }
  return text;
}

function buildYamlInsertText(
  field: string,
  value: string,
  docText: string,
  lines: string[],
  isList?: boolean,
  schema?: FieldSchema,
): { text: string; pos: number } {
  const parts = field.split('.');
  if (parts.length === 1) {
    const trailing = docText.endsWith('\n') ? '' : '\n';

    if (schema?.type === FieldValueType.Object && schema.fields) {
      const defaults: Record<string, string> = {};
      for (const [k, s] of Object.entries(schema.fields)) {
        defaults[k] = s.required ? defaultValueForType(s) : '';
      }
      return {
        text: `${trailing}${buildObjectYamlText(field, schema, defaults)}`,
        pos: docText.length,
      };
    }

    if (schema?.type === FieldValueType.StringArray || isList) {
      return {
        text: `${trailing}${field}:\n    - '${value}'\n`,
        pos: docText.length,
      };
    }

    return {
      text: `${trailing}${field}: ${formatValueForYaml(value, schema)}\n`,
      pos: docText.length,
    };
  }

  const parentKey = parts[0];
  const childKey = parts.slice(1).join('.');

  let parentLineIdx = -1;
  let insertAfterIdx = -1;
  for (let i = 0; i < lines.length; i++) {
    const trimmed = lines[i].trimStart();
    if (trimmed.startsWith(`${parentKey}:`) || trimmed.startsWith(`${parentKey} :`)) {
      parentLineIdx = i;
      insertAfterIdx = i;
      for (let j = i + 1; j < lines.length; j++) {
        if (lines[j].match(/^\s/) && !lines[j].match(/^\S/)) {
          insertAfterIdx = j;
        } else if (lines[j].trim() === '') {
          insertAfterIdx = j;
        } else {
          break;
        }
      }
      break;
    }
  }

  if (parentLineIdx >= 0) {
    const pos = Math.min(lineOffset(lines, insertAfterIdx + 1), docText.length);
    const needsNewline = insertAfterIdx < lines.length - 1 || !docText.endsWith('\n');
    return {
      text: `    ${childKey}: ${formatValueForYaml(value, schema)}${needsNewline ? '\n' : ''}`,
      pos,
    };
  }

  const trailing = docText.endsWith('\n') ? '' : '\n';
  return {
    text: `${trailing}${parentKey}:\n    ${childKey}: ${formatValueForYaml(value, schema)}\n`,
    pos: docText.length,
  };
}

function buildInsertText(
  field: string,
  value: string,
  docText: string,
  format: FormatType,
  cursorLine?: number,
  isList?: boolean,
  schema?: FieldSchema,
): { text: string; pos: number; inline?: boolean } {
  const lines = docText.split('\n');

  switch (format) {
    case FormatType.JSON:
      return { text: `"${field}": "${value}"`, pos: docText.length };
    case FormatType.YARA:
      return buildYaraInsertText(field, value, docText, lines, cursorLine);
    default:
      return buildYamlInsertText(field, value, docText, lines, isList, schema);
  }
}

const setViewRef = StateEffect.define<EditorView>();

const EDITABLE_RE = /(=\s*"|:\s*'|-\s*')([^"']*)("|')/;

function findEditableRange(text: string): { before: string; value: string; after: string; quote: string } | null {
  const trimmed = text.trim();
  const m = trimmed.match(EDITABLE_RE);
  if (!m) return null;
  const matchStart = m.index!;
  const before = trimmed.slice(0, matchStart + m[1].length);
  const value = m[2];
  const after = trimmed.slice(matchStart + m[1].length + m[2].length);
  return { before, value, after, quote: m[3] };
}

function makeBtn(label: string, title: string, bgVar: string, onClick: () => void): HTMLButtonElement {
  const btn = document.createElement('button');
  btn.textContent = label;
  btn.title = title;
  btn.style.cssText = [
    `background-color: var(${bgVar})`,
    'color: var(--thorium-button-text)',
    'border: none',
    'border-radius: 4px',
    'padding: 2px 10px',
    'cursor: pointer',
    'font-size: 11px',
    'font-weight: 600',
    'transition: filter 0.15s',
  ].join(';');
  btn.addEventListener('mouseenter', () => {
    btn.style.filter = 'brightness(1.15)';
  });
  btn.addEventListener('mouseleave', () => {
    btn.style.filter = '';
  });
  btn.addEventListener('click', (e) => {
    e.preventDefault();
    e.stopPropagation();
    onClick();
  });
  return btn;
}

function validateSchemaValue(value: string, schema: FieldSchema): boolean {
  if (schema.required && !value) return false;
  if (!value) return true;
  switch (schema.type) {
    case FieldValueType.Number:
      return /^-?\d+(\.\d+)?$/.test(value.trim());
    case FieldValueType.Boolean:
      return value === 'true' || value === 'false';
    case FieldValueType.Enum:
      return schema.enumValues ? schema.enumValues.includes(value) : true;
    default:
      return true;
  }
}

function validateObjectSchema(values: Record<string, string>, schema: FieldSchema): { valid: boolean; errors: string[] } {
  if (!schema.fields) return { valid: true, errors: [] };
  const errors: string[] = [];
  for (const [key, subSchema] of Object.entries(schema.fields)) {
    const val = values[key] ?? '';
    if (subSchema.required && !val) {
      errors.push(`${key} is required`);
    } else if (val && !validateSchemaValue(val, subSchema)) {
      errors.push(`${key}: invalid ${subSchema.type} value`);
    }
  }
  return { valid: errors.length === 0, errors };
}

const inputBaseStyle = [
  'background: var(--thorium-highlight-panel-bg)',
  'color: var(--thorium-text)',
  'border: 1px solid var(--thorium-panel-border)',
  'border-radius: 3px',
  'padding: 0 4px',
  'font-family: monospace',
  'font-size: 13px',
  'line-height: 18px',
  'height: 20px',
  'box-sizing: border-box',
  'outline: none',
].join(';');

function makeFormInput(schema: FieldSchema, initialValue: string): HTMLInputElement {
  const input = document.createElement('input');
  input.type = 'text';
  if (schema.type === FieldValueType.Number) input.inputMode = 'numeric';
  input.value = initialValue;
  input.placeholder = schema.placeholder ?? '';
  input.style.cssText = `${inputBaseStyle};min-width:80px;max-width:200px;`;
  return input;
}

function makeFormSelect(options: readonly string[], initialValue: string): HTMLSelectElement {
  const select = document.createElement('select');
  select.style.cssText = `${inputBaseStyle};cursor:pointer;min-width:80px;`;
  const emptyOpt = document.createElement('option');
  emptyOpt.value = '';
  emptyOpt.textContent = '-- select --';
  select.appendChild(emptyOpt);
  for (const opt of options) {
    const option = document.createElement('option');
    option.value = opt;
    option.textContent = opt;
    if (opt === initialValue) option.selected = true;
    select.appendChild(option);
  }
  if (initialValue) select.value = initialValue;
  return select;
}

function makeBoolSelect(initialValue: string): HTMLSelectElement {
  return makeFormSelect(['true', 'false'] as const, initialValue);
}

class PreviewWidget extends WidgetType {
  constructor(
    readonly insertText: string,
    readonly viewRef: { current: EditorView | null },
    readonly inline: boolean = false,
    readonly schema?: FieldSchema,
  ) {
    super();
  }

  eq(other: PreviewWidget): boolean {
    return this.insertText === other.insertText && this.inline === other.inline && this.schema === other.schema;
  }

  toDOM(): HTMLElement {
    if (this.inline) return this.toInlineDOM();
    if (this.schema?.type === FieldValueType.Object && this.schema.fields) return this.toObjectDOM();

    const firstLine = this.insertText.split('\n').find((l) => l.trim().length > 0) ?? this.insertText;
    const leadingSpaces = firstLine.match(/^(\s*)/)?.[1].length ?? 0;
    const marginLeft = `${leadingSpaces}ch`;

    const wrapper = document.createElement('div');
    wrapper.className = 'cm-suggestion-preview';
    wrapper.style.cssText = [
      'display: flex',
      'flex-wrap: wrap',
      'align-items: flex-end',
      'gap: 8px',
      'padding: 4px 10px',
      `margin: 0 4px 0 ${marginLeft}`,
      'background-color: var(--thorium-panel-bg)',
      'border: 1px solid var(--thorium-panel-border)',
      'border-left: 3px solid var(--thorium-info-secondary-bg)',
      'border-radius: 6px',
      'font-family: monospace',
      'font-size: 13px',
      'box-shadow: 0 1px 3px rgba(0, 0, 0, 0.15)',
    ].join(';');

    const editable = findEditableRange(this.insertText);
    const contentContainer = document.createElement('span');
    contentContainer.style.cssText = ['flex: 1 1 auto', 'min-width: 0'].join(';');

    const acceptBtn = makeBtn('Accept', 'Insert this field (Enter)', '--thorium-ok-bg', () => {
      this.viewRef.current?.dispatch({ effects: acceptPreview.of() });
    });
    const dismissBtn = makeBtn('Dismiss', 'Cancel (Escape)', '--thorium-info-bg', () => {
      this.viewRef.current?.dispatch({ effects: clearPreview.of() });
    });

    if (editable) {
      const labelStyle = ['color: var(--thorium-info-secondary-bg)', 'font-style: italic', 'white-space: pre', 'line-height: 20px'].join(
        ';',
      );

      const trimmedText = this.insertText.trim();
      const editMatch = trimmedText.match(EDITABLE_RE);
      const editLineIdx = editMatch ? trimmedText.slice(0, editMatch.index).split('\n').length - 1 : 0;
      const allLines = trimmedText.split('\n');

      const headerLines = allLines.slice(0, editLineIdx);
      if (headerLines.length > 0) {
        const headerSpan = document.createElement('div');
        headerSpan.style.cssText = [
          'color: var(--thorium-info-secondary-bg)',
          'font-style: italic',
          'white-space: pre',
          'line-height: 20px',
        ].join(';');
        headerSpan.textContent = headerLines.join('\n');
        contentContainer.appendChild(headerSpan);
      }

      const editableRow = document.createElement('div');
      editableRow.style.cssText = ['display: flex', 'align-items: baseline', 'gap: 0'].join(';');

      const prefixSpan = document.createElement('span');
      prefixSpan.style.cssText = labelStyle;
      const editableLine = allLines[editLineIdx] ?? '';
      const lineMatch = editableLine.match(EDITABLE_RE);
      prefixSpan.textContent = lineMatch ? editableLine.slice(0, lineMatch.index! + lineMatch[1].length) : editable.before;

      let inputEl: HTMLInputElement | HTMLSelectElement;

      if (this.schema?.type === FieldValueType.Enum && this.schema.enumValues) {
        inputEl = makeFormSelect(this.schema.enumValues, editable.value === '<value>' ? '' : editable.value);
        inputEl.style.cssText += ';vertical-align:baseline;';
      } else if (this.schema?.type === FieldValueType.Boolean) {
        inputEl = makeBoolSelect(editable.value === '<value>' ? '' : editable.value);
        inputEl.style.cssText += ';vertical-align:baseline;';
      } else {
        const input = document.createElement('input');
        input.type = 'text';
        input.value = editable.value === '<value>' ? '' : editable.value;
        input.placeholder = this.schema?.placeholder ?? 'value';
        input.style.cssText = [
          'background: var(--thorium-highlight-panel-bg)',
          'color: var(--thorium-text)',
          'border: 1px solid var(--thorium-panel-border)',
          'border-radius: 3px',
          'padding: 0 4px',
          'font-family: monospace',
          'font-size: 13px',
          'line-height: 18px',
          'height: 20px',
          'box-sizing: border-box',
          'min-width: 80px',
          'max-width: 300px',
          'outline: none',
          'vertical-align: baseline',
        ].join(';');
        if (this.schema?.type === FieldValueType.Number) input.inputMode = 'numeric';
        inputEl = input;
      }

      const suffixSpan = document.createElement('span');
      suffixSpan.style.cssText = labelStyle;
      suffixSpan.textContent = editable.after;

      const rebuildText = () => {
        const indent = this.insertText.match(/^(\s*)/)?.[1] ?? '';
        const rest = this.insertText.trimStart();
        const m = rest.match(EDITABLE_RE);
        if (m) {
          const val = inputEl.value;
          const formatted = this.schema ? formatValueForYaml(val, this.schema) : `${m[3]}${val}${m[3]}`;
          const mIdx = m.index ?? 0;
          const prefix = rest.slice(0, mIdx) + rest.slice(mIdx, mIdx + m[1].length).replace(/['"]$/, '');
          const suffix = rest.slice(mIdx + m[0].length).replace(/^['"]/, '');
          const newRest =
            m[3] === '"' || m[3] === "'"
              ? rest.slice(0, m.index! + m[1].length) + val + rest.slice(m.index! + m[1].length + m[2].length)
              : prefix + formatted + suffix;
          return indent + newRest;
        }
        return this.insertText;
      };

      const updateValidation = () => {
        if (!this.schema) return;
        const isValid = validateSchemaValue(inputEl.value, this.schema);
        inputEl.style.borderColor = isValid ? 'var(--thorium-panel-border)' : 'var(--thorium-danger-bg)';
        acceptBtn.style.opacity = isValid ? '1' : '0.4';
        acceptBtn.style.pointerEvents = isValid ? 'auto' : 'none';
      };

      const onInputChange = () => {
        this.viewRef.current?.dispatch({ effects: updateInsertText.of(rebuildText()) });
        updateValidation();
      };

      inputEl.addEventListener('input', onInputChange);
      inputEl.addEventListener('change', onInputChange);

      inputEl.addEventListener('keydown', ((e: KeyboardEvent) => {
        if (e.key === 'Enter') {
          e.preventDefault();
          e.stopPropagation();
          if (this.schema && !validateSchemaValue(inputEl.value, this.schema)) return;
          this.viewRef.current?.dispatch({ effects: updateInsertText.of(rebuildText()) });
          setTimeout(() => this.viewRef.current?.dispatch({ effects: acceptPreview.of() }), 0);
        }
        if (e.key === 'Escape') {
          e.preventDefault();
          e.stopPropagation();
          this.viewRef.current?.dispatch({ effects: clearPreview.of() });
        }
      }) as EventListener);

      editableRow.appendChild(prefixSpan);
      editableRow.appendChild(inputEl);
      editableRow.appendChild(suffixSpan);
      contentContainer.appendChild(editableRow);

      const footerLines = allLines.slice(editLineIdx + 1).filter((l) => l.trim().length > 0);
      if (footerLines.length > 0) {
        const footerSpan = document.createElement('div');
        footerSpan.style.cssText = [
          'color: var(--thorium-info-secondary-bg)',
          'font-style: italic',
          'white-space: pre',
          'line-height: 20px',
          'opacity: 0.7',
        ].join(';');
        footerSpan.textContent = footerLines.join('\n');
        contentContainer.appendChild(footerSpan);
      }

      setTimeout(() => {
        inputEl.focus();
        updateValidation();
      }, 0);
    } else {
      const textSpan = document.createElement('span');
      textSpan.style.cssText = [
        'color: var(--thorium-info-secondary-bg)',
        'font-style: italic',
        'white-space: pre-wrap',
        'word-break: break-word',
      ].join(';');
      textSpan.textContent = this.insertText.trim();
      contentContainer.appendChild(textSpan);
    }

    const btnContainer = document.createElement('span');
    btnContainer.style.cssText = ['display: flex', 'gap: 6px', 'flex-shrink: 0'].join(';');

    btnContainer.appendChild(acceptBtn);
    btnContainer.appendChild(dismissBtn);
    wrapper.appendChild(contentContainer);
    wrapper.appendChild(btnContainer);
    return wrapper;
  }

  private toObjectDOM(): HTMLElement {
    const schema = this.schema!;
    const fields = schema.fields!;
    const fieldName = this.insertText.trim().split(':')[0].trim();

    const wrapper = document.createElement('div');
    wrapper.className = 'cm-suggestion-preview';
    wrapper.style.cssText = [
      'display: flex',
      'flex-direction: column',
      'gap: 6px',
      'padding: 8px 12px',
      'margin: 0 4px',
      'background-color: var(--thorium-panel-bg)',
      'border: 1px solid var(--thorium-panel-border)',
      'border-left: 3px solid var(--thorium-info-secondary-bg)',
      'border-radius: 6px',
      'font-family: monospace',
      'font-size: 13px',
      'box-shadow: 0 1px 3px rgba(0, 0, 0, 0.15)',
    ].join(';');

    const headerSpan = document.createElement('div');
    headerSpan.style.cssText = 'color:var(--thorium-info-secondary-bg);font-style:italic;font-weight:600;line-height:20px;';
    headerSpan.textContent = `${fieldName}:`;
    wrapper.appendChild(headerSpan);

    const formContainer = document.createElement('div');
    formContainer.style.cssText = 'display:flex;flex-direction:column;gap:4px;padding-left:16px;';

    const inputs = new Map<string, HTMLInputElement | HTMLSelectElement>();

    const errorArea = document.createElement('div');
    errorArea.style.cssText = 'font-size:11px;color:var(--thorium-danger-bg);min-height:14px;padding-left:16px;';

    const acceptBtn = makeBtn('Accept', 'Insert this field (Enter)', '--thorium-ok-bg', () => {
      const values = collectValues();
      const { valid } = validateObjectSchema(values, schema);
      if (valid) {
        this.viewRef.current?.dispatch({ effects: acceptPreview.of() });
      }
    });
    const dismissBtn = makeBtn('Dismiss', 'Cancel (Escape)', '--thorium-info-bg', () => {
      this.viewRef.current?.dispatch({ effects: clearPreview.of() });
    });

    const collectValues = (): Record<string, string> => {
      const values: Record<string, string> = {};
      for (const [key, el] of inputs) {
        values[key] = el.value;
      }
      return values;
    };

    const rebuildAndValidate = () => {
      const values = collectValues();
      const indent = this.insertText.match(/^(\s*)/)?.[1] ?? '';
      const text = indent + buildObjectYamlText(fieldName, schema, values);
      this.viewRef.current?.dispatch({ effects: updateInsertText.of(text) });

      const { valid, errors } = validateObjectSchema(values, schema);
      errorArea.textContent = errors.length > 0 ? errors[0] : '';
      acceptBtn.style.opacity = valid ? '1' : '0.4';
      acceptBtn.style.pointerEvents = valid ? 'auto' : 'none';

      for (const [key, el] of inputs) {
        const subSchema = fields[key];
        if (!subSchema) continue;
        const isFieldValid = validateSchemaValue(el.value, subSchema);
        el.style.borderColor = isFieldValid ? 'var(--thorium-panel-border)' : 'var(--thorium-danger-bg)';
      }
    };

    let firstInput: HTMLInputElement | HTMLSelectElement | null = null;

    for (const [subKey, subSchema] of Object.entries(fields)) {
      if (subSchema.type === FieldValueType.Object) continue;

      const row = document.createElement('div');
      row.style.cssText = 'display:flex;align-items:baseline;gap:6px;';

      const label = document.createElement('span');
      label.style.cssText = 'color:var(--thorium-info-secondary-bg);font-style:italic;white-space:nowrap;min-width:90px;';
      label.textContent = `${subKey}:`;
      if (subSchema.required) {
        const req = document.createElement('span');
        req.style.cssText = 'color:var(--thorium-danger-bg);margin-left:2px;';
        req.textContent = '*';
        label.appendChild(req);
      }

      let inputEl: HTMLInputElement | HTMLSelectElement;
      const initialValue = defaultValueForType(subSchema);

      if (subSchema.type === FieldValueType.Enum && subSchema.enumValues) {
        inputEl = makeFormSelect(subSchema.enumValues, initialValue);
      } else if (subSchema.type === FieldValueType.Boolean) {
        inputEl = makeBoolSelect(initialValue);
      } else {
        inputEl = makeFormInput(subSchema, initialValue === subSchema.placeholder ? '' : initialValue);
      }

      inputEl.addEventListener('input', rebuildAndValidate);
      inputEl.addEventListener('change', rebuildAndValidate);
      inputEl.addEventListener('keydown', ((e: KeyboardEvent) => {
        if (e.key === 'Enter') {
          e.preventDefault();
          e.stopPropagation();
          const values = collectValues();
          const { valid } = validateObjectSchema(values, schema);
          if (valid) {
            setTimeout(() => this.viewRef.current?.dispatch({ effects: acceptPreview.of() }), 0);
          }
        }
        if (e.key === 'Escape') {
          e.preventDefault();
          e.stopPropagation();
          this.viewRef.current?.dispatch({ effects: clearPreview.of() });
        }
      }) as EventListener);

      inputs.set(subKey, inputEl);
      if (!firstInput) firstInput = inputEl;

      row.appendChild(label);
      row.appendChild(inputEl);
      formContainer.appendChild(row);
    }

    wrapper.appendChild(formContainer);
    wrapper.appendChild(errorArea);

    const btnContainer = document.createElement('div');
    btnContainer.style.cssText = 'display:flex;gap:6px;justify-content:flex-end;';
    btnContainer.appendChild(acceptBtn);
    btnContainer.appendChild(dismissBtn);
    wrapper.appendChild(btnContainer);

    setTimeout(() => {
      if (firstInput) firstInput.focus();
      rebuildAndValidate();
    }, 0);

    return wrapper;
  }

  private toInlineDOM(): HTMLElement {
    const wrapper = document.createElement('span');
    wrapper.className = 'cm-suggestion-preview';
    wrapper.style.cssText = [
      'display: inline-flex',
      'align-items: center',
      'gap: 6px',
      'padding: 1px 8px',
      'margin-left: 8px',
      'background-color: var(--thorium-panel-bg)',
      'border: 1px solid var(--thorium-panel-border)',
      'border-left: 3px solid var(--thorium-info-secondary-bg)',
      'border-radius: 4px',
      'font-family: monospace',
      'font-size: 13px',
      'vertical-align: baseline',
    ].join(';');

    const textSpan = document.createElement('span');
    textSpan.style.cssText = ['color: var(--thorium-info-secondary-bg)', 'font-style: italic', 'white-space: pre'].join(';');
    textSpan.textContent = this.insertText.trim();
    wrapper.appendChild(textSpan);

    const btnContainer = document.createElement('span');
    btnContainer.style.cssText = ['display: inline-flex', 'gap: 4px', 'flex-shrink: 0'].join(';');

    const acceptBtn = makeBtn('Accept', 'Insert (Enter)', '--thorium-ok-bg', () => {
      this.viewRef.current?.dispatch({ effects: acceptPreview.of() });
    });
    const dismissBtn = makeBtn('Dismiss', 'Cancel (Escape)', '--thorium-info-bg', () => {
      this.viewRef.current?.dispatch({ effects: clearPreview.of() });
    });

    btnContainer.appendChild(acceptBtn);
    btnContainer.appendChild(dismissBtn);
    wrapper.appendChild(btnContainer);
    return wrapper;
  }

  ignoreEvent(): boolean {
    return true;
  }
}

interface ProposalState {
  proposal: PreviewProposal | null;
  insertText: string;
  insertPos: number;
  inline: boolean;
}

export const previewState = StateField.define<ProposalState>({
  create() {
    return { proposal: null, insertText: '', insertPos: 0, inline: false };
  },

  update(state, tr) {
    const empty: ProposalState = { proposal: null, insertText: '', insertPos: 0, inline: false };
    for (const effect of tr.effects) {
      if (effect.is(addPreview)) {
        const docText = tr.state.doc.toString();
        const result = buildInsertText(
          effect.value.field,
          effect.value.value,
          docText,
          effect.value.format,
          effect.value.cursorLine,
          effect.value.isList,
          effect.value.schema,
        );
        return { proposal: effect.value, insertText: result.text, insertPos: result.pos, inline: result.inline ?? false };
      }
      if (effect.is(clearPreview)) return empty;
      if (effect.is(acceptPreview)) return empty;
      if (effect.is(updateInsertText)) {
        return { ...state, insertText: effect.value };
      }
    }
    if (tr.docChanged && state.proposal) return empty;
    return state;
  },
});

const previewDecoField = StateField.define<{ decos: DecorationSet; viewRef: { current: EditorView | null } }>({
  create() {
    return { decos: Decoration.none, viewRef: { current: null } };
  },

  update(state, tr) {
    for (const effect of tr.effects) {
      if (effect.is(setViewRef)) {
        return { ...state, viewRef: { current: effect.value } };
      }
    }

    const prev = tr.startState.field(previewState);
    const curr = tr.state.field(previewState);
    if (prev === curr) return state;

    if (!curr.proposal) {
      return { ...state, decos: Decoration.none };
    }

    const iPos = Math.min(curr.insertPos, tr.state.doc.length);
    const widget = new PreviewWidget(curr.insertText, state.viewRef, curr.inline, curr.proposal?.schema);

    if (curr.inline) {
      const widgetDeco = Decoration.widget({ widget, side: 1, block: false }).range(iPos);
      return { ...state, decos: Decoration.set([widgetDeco]) };
    }

    const doc = tr.state.doc;
    const line = doc.lineAt(iPos);
    const side = iPos === doc.length || iPos !== line.from ? 1 : -1;
    const widgetDeco = Decoration.widget({ widget, side, block: true }).range(iPos);
    return {
      ...state,
      decos: Decoration.set([widgetDeco]),
    };
  },

  provide(field) {
    return EditorView.decorations.from(field, (val) => val.decos);
  },
});

export function handleAcceptEffect(view: EditorView): boolean {
  const state = view.state.field(previewState);
  if (!state.proposal) return false;

  view.dispatch({ effects: acceptPreview.of() });
  return true;
}

export function createPreviewExtensions() {
  const acceptFilter = EditorState.transactionFilter.of((tr) => {
    for (const effect of tr.effects) {
      if (effect.is(acceptPreview)) {
        const state = tr.startState.field(previewState);
        if (state.proposal) {
          return [
            tr,
            {
              changes: { from: state.insertPos, insert: state.insertText },
            },
          ];
        }
      }
    }
    return tr;
  });

  const keyHandler = EditorView.domEventHandlers({
    keydown(event, view) {
      const state = view.state.field(previewState);
      if (!state.proposal) return false;

      const target = event.target as HTMLElement;
      if (target.closest('.cm-suggestion-preview')) return false;

      if (event.key === 'Enter') {
        event.preventDefault();
        view.dispatch({ effects: acceptPreview.of() });
        return true;
      }
      if (event.key === 'Escape') {
        event.preventDefault();
        view.dispatch({ effects: clearPreview.of() });
        return true;
      }
      return false;
    },
  });

  const viewRefPlugin = EditorView.updateListener.of((update) => {
    const field = update.state.field(previewDecoField);
    if (!field.viewRef.current) {
      update.view.dispatch({ effects: setViewRef.of(update.view) });
    }
  });

  return [previewState, previewDecoField, acceptFilter, keyHandler, viewRefPlugin];
}
