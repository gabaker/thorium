import { describe, expect, it } from 'vitest';

// project imports
import { buildToolSetUpdateForm, selectorText } from './forms';

describe('buildToolSetUpdateForm', () => {
  it('sends only what changed', () => {
    const form = buildToolSetUpdateForm({ addTools: ['a'], removeTools: ['b', 'c'], readOnly: false });
    expect(form.getAll('metadata[add_toolset_tools][]')).toEqual(['a']);
    expect(form.getAll('metadata[remove_toolset_tools][]')).toEqual(['b', 'c']);
    expect(form.get('metadata[toolset_read_only]')).toBe('false');
    expect(form.has('metadata[toolset_auto_instance]')).toBe(false);
  });
  it('needs a full selector', () => {
    expect(buildToolSetUpdateForm({ selector: { key: 'k', value: '' } }).has('metadata[toolset_selector_key]')).toBe(false);
    const form = buildToolSetUpdateForm({ selector: { key: 'k', value: 'v' }, clearSelector: false });
    expect(form.get('metadata[toolset_selector_value]')).toBe('v');
  });
  it('clears the selector', () => {
    expect(buildToolSetUpdateForm({ clearSelector: true }).get('metadata[clear_toolset_selector]')).toBe('true');
  });
});

describe('selectorText', () => {
  it('prints key=value or none', () => {
    expect(selectorText({ key: 'a', value: 'b' })).toBe('a=b');
    expect(selectorText(null)).toBe('none');
  });
});
