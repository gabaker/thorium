import { describe, expect, it } from 'vitest';

// project imports
import { sampleNamesByHash } from './hooks';

describe('sampleNamesByHash', () => {
  it('names each read sample by its submissions and leaves out unread, unreadable, and unnamed ones', () => {
    const named = { submissions: [{ name: 'a.exe' }, { name: 'b.exe' }, { name: 'a.exe' }] };
    const unnamed = { submissions: [{ name: undefined }] };
    const names = sampleNamesByHash(['1', '2', '3', '4'], [named, null, undefined, unnamed] as never);
    expect(names).toEqual({ '1': 'a.exe, b.exe' });
  });
});
