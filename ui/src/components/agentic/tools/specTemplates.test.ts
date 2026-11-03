import { describe, expect, it } from 'vitest';

// project imports
import { SPEC_TEMPLATES } from './specTemplates';
import { parseToolSpec } from '@utilities/agentic/forms';
import { ToolKind } from '@models/tools';

describe('SPEC_TEMPLATES', () => {
  it.each(Object.values(ToolKind))('the %s template is a spec of its own kind', (kind) => {
    const parsed = parseToolSpec(SPEC_TEMPLATES[kind]);
    expect(parsed.error).toBeNull();
    expect(parsed.kind).toBe(kind);
  });
});
