/** Where an entered hash stands: being looked up, found, not found or hidden, or not a hash at all. */
export enum HashCheckStatus {
  Pending = 'pending',
  Valid = 'valid',
  Missing = 'missing',
  Malformed = 'malformed',
}

/** The outcome of checking one entered hash. */
export interface HashCheck {
  /** Where the hash stands */
  status: HashCheckStatus;
  /** A readable name for a found hash (a sample's submitted file name), when one is known */
  name?: string;
  /** Why a hash can't be used, for a missing or malformed hash */
  reason?: string;
}

/** The reason shown for a hash that the lookup didn't find; the API answers the same for missing and hidden samples. */
export const HASH_NOT_ACCESSIBLE = 'not found or not accessible';

/**
 * Split typed or pasted text into hash entries.
 *
 * @param text - Hashes separated by whitespace or commas.
 * @returns The entries, trimmed and lowercased, without blanks or repeats, in the order given.
 */
export function splitHashEntries(text: string): string[] {
  const entries = text
    .split(/[\s,]+/)
    .map((entry) => entry.trim().toLowerCase())
    .filter((entry) => entry !== '');
  return [...new Set(entries)];
}

/**
 * Check that an entry is a SHA256, the only hash a run or a sample lookup takes.
 *
 * @param entry - The entry as typed.
 * @returns Why the entry is not a SHA256, or `null` when it is one.
 */
export function sha256Problem(entry: string): string | null {
  if (/^[0-9a-f]{64}$/i.test(entry)) return null;
  // an MD5 or SHA1 is a real hash, but samples are named by their SHA256
  if (/^([0-9a-f]{32}|[0-9a-f]{40})$/i.test(entry)) return 'invalid hash: use the SHA256, not the MD5 or SHA1';
  return 'invalid hash: a SHA256 is 64 hex characters';
}

/**
 * Add entries to a list of hashes, keeping the list free of repeats.
 *
 * @param current - The hashes already entered.
 * @param added - The new entries.
 * @returns The combined list, existing entries first.
 */
export function addHashes(current: string[], added: string[]): string[] {
  return [...current, ...added.filter((entry) => !current.includes(entry))];
}

/**
 * Name why a list of entered hashes can't be used yet.
 *
 * @param hashes - The entered hashes.
 * @param checks - Their checks so far; an entry without one is judged by its form alone.
 * @returns A sentence naming the first blocking cause (malformed, then missing, then pending), or `null` when every
 * hash was found.
 */
export function hashChecksBlocker(hashes: string[], checks: Record<string, HashCheck>): string | null {
  // an entry not checked yet is malformed when it isn't a SHA256, and pending otherwise
  const status = (hash: string) => checks[hash]?.status ?? (sha256Problem(hash) ? HashCheckStatus.Malformed : HashCheckStatus.Pending);
  const malformed = hashes.filter((hash) => status(hash) === HashCheckStatus.Malformed);
  if (malformed.length > 0) return `"${malformed[0]}" is not a SHA256: fix or remove it.`;
  const missing = hashes.filter((hash) => status(hash) === HashCheckStatus.Missing);
  if (missing.length === 1) return `Sample ${missing[0].slice(0, 12)}… was not found or is not accessible: remove it.`;
  if (missing.length > 1) return `${missing.length} samples were not found or are not accessible: remove them.`;
  if (hashes.some((hash) => status(hash) === HashCheckStatus.Pending)) return 'Checking the entered samples…';
  return null;
}
