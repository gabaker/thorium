import { ToolSetSelector } from '@models/toolsets';

/** The edits a toolset update can carry. */
export interface ToolSetChange {
  /** Tool ids to include */
  addTools?: string[];
  /** Tool ids to stop including */
  removeTools?: string[];
  /** A selector to set */
  selector?: ToolSetSelector;
  /** Whether to clear the selector */
  clearSelector?: boolean;
  /** The read only flag to set */
  readOnly?: boolean;
  /** The auto instance flag to set */
  autoInstance?: boolean;
}

/**
 * Build the multipart form that edits a toolset's members and settings.
 *
 * @param change - The edits to send; unset edits are left out.
 * @returns The form for `PATCH /entities/{toolset}`.
 */
export function buildToolSetUpdateForm(change: ToolSetChange): FormData {
  const form = new FormData();
  for (const id of change.addTools ?? []) form.append('metadata[add_toolset_tools][]', id);
  for (const id of change.removeTools ?? []) form.append('metadata[remove_toolset_tools][]', id);
  // a selector needs both parts to mean anything
  if (change.selector && change.selector.key !== '' && change.selector.value !== '') {
    form.append('metadata[toolset_selector_key]', change.selector.key);
    form.append('metadata[toolset_selector_value]', change.selector.value);
  }
  if (change.clearSelector) form.append('metadata[clear_toolset_selector]', 'true');
  if (change.readOnly !== undefined) form.append('metadata[toolset_read_only]', String(change.readOnly));
  if (change.autoInstance !== undefined) form.append('metadata[toolset_auto_instance]', String(change.autoInstance));
  return form;
}

/**
 * Describe a selector as `key=value`.
 *
 * @param selector - The selector, if any.
 * @returns The text, or `none`.
 */
export function selectorText(selector: ToolSetSelector | null | undefined): string {
  return selector ? `${selector.key}=${selector.value}` : 'none';
}
