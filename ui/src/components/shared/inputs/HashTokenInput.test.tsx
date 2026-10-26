import { renderToStaticMarkup } from 'react-dom/server';
import { MemoryRouter } from 'react-router-dom';
import { describe, expect, it } from 'vitest';

// project imports
import { HashTokenInput, shortHash } from './HashTokenInput';
import { HashCheck, HashCheckStatus } from '@utilities/hashes';

/** Four entries, one per state. */
const VALID = 'a'.repeat(64);
const MISSING = 'b'.repeat(64);
const PENDING = 'c'.repeat(64);

/** Render the field with the given entries and checks. */
const render = (hashes: string[], checks: Record<string, HashCheck>) =>
  renderToStaticMarkup(
    <MemoryRouter>
      <HashTokenInput hashes={hashes} checks={checks} onChange={() => undefined} linkTo={(hash) => `/file/${hash}`} testId="entry" />
    </MemoryRouter>,
  );

describe('HashTokenInput', () => {
  const markup = render([VALID, MISSING, 'nothex', PENDING], {
    [VALID]: { status: HashCheckStatus.Valid, name: 'evil.exe' },
    [MISSING]: { status: HashCheckStatus.Missing, reason: 'not found or not accessible' },
    nothex: { status: HashCheckStatus.Malformed, reason: 'invalid hash: a SHA256 is 64 hex characters' },
  });
  it('links a found hash to its page with the resolved name', () => {
    expect(markup).toContain(`href="/file/${VALID}"`);
    expect(markup).toContain(`evil.exe (${shortHash(VALID)})`);
    expect(markup).toContain(`data-testid="hash-token-${VALID}" data-status="valid"`);
  });
  it('marks a missing or malformed hash with an error icon labelled with the reason, and no link', () => {
    expect(markup).toContain('data-status="missing"');
    expect(markup).toContain('aria-label="not found or not accessible"');
    expect(markup).toContain('data-status="malformed"');
    expect(markup).toContain('aria-label="invalid hash: a SHA256 is 64 hex characters"');
    expect(markup).not.toContain(`href="/file/${MISSING}"`);
  });
  it('shows a spinner for a hash not answered yet', () => {
    expect(markup).toContain(`data-testid="hash-token-${PENDING}" data-status="pending"`);
    expect(markup).toContain('aria-label="checking"');
  });
  it('gives every token a remove control', () => {
    expect(markup.match(/data-testid="hash-token-remove-/g)).toHaveLength(4);
  });
  it('shows the placeholder only while empty', () => {
    expect(render([], {})).toContain('placeholder="Paste or type SHA256s"');
    expect(markup).toContain('placeholder=""');
  });
});
