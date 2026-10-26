import { useQueries } from '@tanstack/react-query';

// project imports
import { useDebouncedValue } from '@utilities/useDebouncedValue';
import { HASH_NOT_ACCESSIBLE, HashCheck, HashCheckStatus, sha256Problem } from '@utilities/hashes';
import { getFileDetails } from '@thorpi/files';
import { Sample } from '@models/files';

/** How long entered hashes must stay unchanged before they are looked up, so a burst of pastes is one batch. */
export const SAMPLE_CHECK_DEBOUNCE_MS = 300;

/** How long a sample lookup stays fresh; a sample rarely changes groups while a form is open. */
const SAMPLE_STALE_MS = 5 * 60 * 1000;

/** Query keys for samples, so every page that reads a sample shares one cache entry per hash. */
export const sampleKeys = {
  /** Every sample query */
  all: ['sample'] as const,
  /**
   * One sample's details.
   *
   * @param sha256 - The sample's SHA256.
   * @returns The query key.
   */
  detail: (sha256: string) => ['sample', sha256] as const,
};

/**
 * The query options that read one sample's details as the signed-in user.
 *
 * The API answers a sample in none of the user's groups the same as one that doesn't exist, so a `null` result means
 * "not found or not accessible". The read never throws, so it is never retried.
 *
 * @param sha256 - The sample's SHA256.
 * @returns Options for `useQuery` or `useQueries`.
 */
export function sampleQueryOptions(sha256: string) {
  return {
    queryKey: sampleKeys.detail(sha256),
    queryFn: () => getFileDetails(sha256, () => undefined),
    staleTime: SAMPLE_STALE_MS,
  };
}

/** What a sample query has produced so far, as far as checking an entered hash goes. */
export interface SampleQueryState {
  /** Whether the query has answered */
  done: boolean;
  /** The sample, or `null` when the user can't see it */
  sample: Sample | null | undefined;
}

/**
 * Turn entered hashes and their lookups into checks.
 *
 * @param hashes - Every hash currently entered.
 * @param settled - The hashes as they were when typing last paused; a newer entry isn't looked up yet.
 * @param results - The lookup state of each looked-up hash.
 * @returns The check of each entered hash: malformed at once, pending until settled and answered, then found or not.
 */
export function sampleChecks(hashes: string[], settled: string[], results: Record<string, SampleQueryState>): Record<string, HashCheck> {
  const checks: Record<string, HashCheck> = {};
  for (const hash of hashes) {
    const problem = sha256Problem(hash);
    const result = results[hash];
    if (problem) checks[hash] = { status: HashCheckStatus.Malformed, reason: problem };
    else if (!settled.includes(hash) || !result || !result.done) checks[hash] = { status: HashCheckStatus.Pending };
    else if (result.sample) {
      checks[hash] = { status: HashCheckStatus.Valid, name: result.sample.submissions?.find((submission) => submission.name)?.name };
    } else checks[hash] = { status: HashCheckStatus.Missing, reason: HASH_NOT_ACCESSIBLE };
  }
  return checks;
}

/**
 * The hashes to look up: the well-formed ones that are both entered now and settled.
 *
 * @param hashes - Every hash currently entered.
 * @param settled - The hashes as they were when typing last paused.
 * @returns The hashes to query.
 */
export function sampleLookupTargets(hashes: string[], settled: string[]): string[] {
  return hashes.filter((hash) => settled.includes(hash) && sha256Problem(hash) === null);
}

/**
 * Check entered sample hashes: malformed ones fail at once, and well-formed ones are looked up (cached per hash and
 * shared with every other reader of the sample) once the entries stop changing.
 *
 * @param hashes - Every hash currently entered.
 * @returns The check of each entered hash, keyed by hash.
 */
export function useSampleChecks(hashes: string[]): Record<string, HashCheck> {
  const settled = useDebouncedValue(hashes, SAMPLE_CHECK_DEBOUNCE_MS);
  const targets = sampleLookupTargets(hashes, settled);
  const queries = useQueries({ queries: targets.map((hash) => sampleQueryOptions(hash)) });
  const results: Record<string, SampleQueryState> = {};
  targets.forEach((hash, index) => {
    results[hash] = { done: queries[index].isSuccess, sample: queries[index].data };
  });
  return sampleChecks(hashes, settled, results);
}
