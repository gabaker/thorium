import React, { ClipboardEvent, FC, KeyboardEvent, useState } from 'react';
import { FaCircleExclamation, FaXmark } from 'react-icons/fa6';
import { Link } from 'react-router-dom';
import styled from 'styled-components';

// project imports
import { Badge, BadgeTone } from '@components/shared/badges/Badge';
import { StyledSpinner } from '@components/shared/fallback/Spinner.styled';
import { OverlayTipTop } from '@components/shared/overlay/tips';
import { addHashes, HashCheck, HashCheckStatus, splitHashEntries } from '@utilities/hashes';

/** The box that holds the tokens and the entry field, styled like a text input. */
const TokenBox = styled.div`
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.25rem 0.5rem;
  width: 100%;
  padding: 0.5rem 0.75rem;
  border: 1px solid var(--thorium-panel-border);
  border-radius: 6px;
  background-color: var(--thorium-secondary-panel-bg);
  color: var(--thorium-text);

  &:focus-within {
    border-color: var(--thorium-highlight-panel-border);
  }
`;

/** The entry field inside the box; it takes the rest of the line and wraps under the tokens when they fill it. */
const Entry = styled.input`
  flex: 1 1 12rem;
  min-width: 8rem;
  border: none;
  outline: none;
  background: transparent;
  color: inherit;
  font-family: monospace;
  font-size: 0.85rem;

  &::placeholder {
    color: var(--thorium-secondary-text);
  }
`;

/** A hash token: a badge whose icon, text, and remove control sit on one line. */
const Token = styled(Badge)`
  gap: 0.35rem;
  max-width: 100%;
  font-family: monospace;
  font-weight: 500;

  a {
    color: var(--thorium-link-text);
  }
`;

/** The remove control of a token. */
const RemoveToken = styled.button`
  display: inline-flex;
  padding: 0;
  border: none;
  background: transparent;
  color: inherit;
  cursor: pointer;

  &:focus-visible {
    outline: 2px solid var(--thorium-link-text);
    border-radius: 2px;
  }
`;

/**
 * Shorten a hash for a token, keeping its start and end.
 *
 * @param hash - The hash or entry.
 * @returns The first 8 and last 4 characters around an ellipsis, or the entry itself when it is short.
 */
export function shortHash(hash: string): string {
  return hash.length > 16 ? `${hash.slice(0, 8)}…${hash.slice(-4)}` : hash;
}

/** The badge tone of each check status. */
const TONE_BY_STATUS: Record<HashCheckStatus, BadgeTone> = {
  [HashCheckStatus.Pending]: BadgeTone.Neutral,
  [HashCheckStatus.Valid]: BadgeTone.Ok,
  [HashCheckStatus.Missing]: BadgeTone.Error,
  [HashCheckStatus.Malformed]: BadgeTone.Error,
};

/** Props for {@link HashTokenInput}. */
export interface HashTokenInputProps {
  /** The entered hashes, in order */
  hashes: string[];
  /** The check of each entered hash; an entry without one shows as pending */
  checks: Record<string, HashCheck>;
  /** Called with the new list when entries are added or removed */
  onChange: (hashes: string[]) => void;
  /** The page a found hash links to; found hashes are plain text when it isn't given */
  linkTo?: (hash: string) => string;
  /** The entry field's placeholder */
  placeholder?: string;
  /** The entry field's test id; each token's is `hash-token-<hash>` */
  testId?: string;
  /** The entry field's accessible name */
  'aria-label'?: string;
}

/**
 * A field for entering hashes as tokens. An entry becomes a token when it is a complete SHA256, or on Enter, a comma,
 * a space, a paste, or leaving the field; Backspace in an empty field removes the last token. Each token shows its
 * check: found (a link with the resolved name), checking, or an error with the reason in a tooltip.
 */
export const HashTokenInput: FC<HashTokenInputProps> = ({
  hashes,
  checks,
  onChange,
  linkTo,
  placeholder = 'Paste or type SHA256s',
  testId,
  'aria-label': ariaLabel,
}) => {
  const [draft, setDraft] = useState('');

  /**
   * Turn text into tokens and clear the entry field.
   *
   * @param text - The text to split into entries.
   */
  const commit = (text: string) => {
    const entries = splitHashEntries(text);
    setDraft('');
    if (entries.length > 0) onChange(addHashes(hashes, entries));
  };

  /**
   * Keep typed text as the draft, committing it once it holds a separator or a whole SHA256.
   *
   * @param text - The entry field's new value.
   */
  const type = (text: string) => {
    if (/[\s,]/.test(text) || /^[0-9a-f]{64}$/i.test(text.trim())) commit(text);
    else setDraft(text);
  };

  /**
   * Commit on Enter or a comma, and remove the last token on Backspace in an empty field.
   *
   * @param event - The key press.
   */
  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === 'Enter' || event.key === ',') {
      event.preventDefault();
      commit(draft);
    } else if (event.key === 'Backspace' && draft === '' && hashes.length > 0) {
      onChange(hashes.slice(0, -1));
    }
  };

  /**
   * Commit pasted text at once, so a pasted list becomes one token per hash.
   *
   * @param event - The paste.
   */
  const onPaste = (event: ClipboardEvent<HTMLInputElement>) => {
    event.preventDefault();
    commit(`${draft} ${event.clipboardData.getData('text')}`);
  };

  return (
    <TokenBox>
      {hashes.map((hash) => {
        const check = checks[hash] ?? { status: HashCheckStatus.Pending };
        const failed = check.status === HashCheckStatus.Missing || check.status === HashCheckStatus.Malformed;
        let label: React.ReactNode = shortHash(hash);
        if (check.status === HashCheckStatus.Valid) {
          const text = check.name ? `${check.name} (${shortHash(hash)})` : shortHash(hash);
          label = linkTo ? <Link to={linkTo(hash)}>{text}</Link> : text;
        }
        return (
          <Token key={hash} tone={TONE_BY_STATUS[check.status]} data-testid={`hash-token-${hash}`} data-status={check.status} title={hash}>
            {check.status === HashCheckStatus.Pending && <StyledSpinner aria-label="checking" role="status" />}
            {failed && (
              <OverlayTipTop tip={check.reason ?? ''}>
                <FaCircleExclamation role="img" aria-label={check.reason} />
              </OverlayTipTop>
            )}
            {label}
            <RemoveToken
              type="button"
              aria-label={`Remove ${hash}`}
              data-testid={`hash-token-remove-${hash}`}
              onClick={() => onChange(hashes.filter((entry) => entry !== hash))}
            >
              <FaXmark size={12} />
            </RemoveToken>
          </Token>
        );
      })}
      <Entry
        data-testid={testId}
        aria-label={ariaLabel}
        value={draft}
        placeholder={hashes.length === 0 ? placeholder : ''}
        spellCheck={false}
        onChange={(event) => type(event.target.value)}
        onKeyDown={onKeyDown}
        onPaste={onPaste}
        onBlur={() => commit(draft)}
      />
    </TokenBox>
  );
};

export default HashTokenInput;
