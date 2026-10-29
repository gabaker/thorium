import React from 'react';
import { describe, expect, it, vi } from 'vitest';

// project imports
import { KIND_TABS } from './RunBrowsing';
import { RunKind } from '@models/runs';

// the page shell needs the auth provider, which is not under test here
vi.mock('@components/pages/Page', () => ({
  default: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
}));

describe('RunBrowsing kind tabs', () => {
  it('keys every tab but All by a run kind, one tab per kind', () => {
    const keys = KIND_TABS.map((tab) => tab.key);
    expect(keys[0]).toBe('all');
    expect(keys.slice(1).every((key) => (Object.values(RunKind) as string[]).includes(key))).toBe(true);
    expect([...keys.slice(1)].sort()).toEqual([...Object.values(RunKind)].sort());
  });
});
