import { describe, expect, it } from 'vitest';

// project imports
import { addHashes, HashCheckStatus, hashChecksBlocker, sha256Problem, splitHashEntries } from './hashes';

/** Two well-formed SHA256s. */
const A = 'a'.repeat(64);
const B = 'b'.repeat(64);

describe('splitHashEntries', () => {
  it('splits on whitespace and commas, lowercases, and drops blanks and repeats', () => {
    expect(splitHashEntries(` ${A.toUpperCase()},\n${B}  ${A} ,, `)).toEqual([A, B]);
  });
});

describe('sha256Problem', () => {
  it('accepts a SHA256 and refuses anything else, naming an MD5 or SHA1', () => {
    expect(sha256Problem(A)).toBeNull();
    expect(sha256Problem('nothex')).toMatch(/64 hex characters/);
    expect(sha256Problem('c'.repeat(32))).toMatch(/not the MD5 or SHA1/);
    expect(sha256Problem('c'.repeat(40))).toMatch(/not the MD5 or SHA1/);
  });
});

describe('addHashes', () => {
  it('appends new entries and skips ones already entered', () => {
    expect(addHashes([A], [B, A])).toEqual([A, B]);
  });
});

describe('hashChecksBlocker', () => {
  it('passes when every hash was found', () => {
    expect(hashChecksBlocker([A], { [A]: { status: HashCheckStatus.Valid } })).toBeNull();
  });
  it('names a malformed entry first, even before it is checked', () => {
    expect(hashChecksBlocker([A, 'nothex'], {})).toMatch(/"nothex" is not a SHA256/);
  });
  it('names missing samples, then pending ones', () => {
    expect(hashChecksBlocker([A], { [A]: { status: HashCheckStatus.Missing } })).toMatch(/not found or is not accessible/);
    expect(hashChecksBlocker([A, B], { [A]: { status: HashCheckStatus.Missing }, [B]: { status: HashCheckStatus.Missing } })).toMatch(
      /2 samples/,
    );
    expect(hashChecksBlocker([A], {})).toMatch(/Checking/);
  });
});
