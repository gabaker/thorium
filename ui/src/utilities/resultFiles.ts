// project imports
import { getCachedResultFile, setCachedResultFile } from './resultFileCache';
import { getResultsFile } from '@thorpi/results';

/**
 * Fetch a result file's bytes, preferring the shared LRU cache and populating it on a miss.
 *
 * Used by both the Files tab (preview/download) and the "download all" zip builder so a file that
 * was already previewed or downloaded is not re-fetched. Cache reads/writes are keyed by the
 * result's unique id, so reruns of the same tool never share entries.
 *
 * @param sha256 - The SHA256 of the file the result belongs to.
 * @param tool - The tool that produced the result.
 * @param resultId - The unique id of the specific result/run (used as the cache key + API path).
 * @param name - The name of the result file to fetch.
 * @param errorHandler - Called with a formatted message if the download fails.
 * @returns The file bytes, or `null` if the request failed.
 */
export async function fetchResultFileCached(
  sha256: string,
  tool: string,
  resultId: string,
  name: string,
  errorHandler: (error: string) => void,
): Promise<ArrayBuffer | null> {
  const cached = getCachedResultFile(resultId, name);
  if (cached) {
    return cached;
  }
  const res = await getResultsFile(sha256, tool, resultId, name, errorHandler);
  const bytes = res?.data ?? null;
  if (bytes) {
    setCachedResultFile(resultId, name, bytes);
  }
  return bytes;
}
