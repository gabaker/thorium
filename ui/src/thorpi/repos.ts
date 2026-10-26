import client, { parseRequestError } from './client';

// project imports
import { CommitishDetailsPage, Repo } from '@models/repos';
import { Filters } from '@models/search';

interface ApiCursor<T> {
  cursor?: string;
  data: T[];
}

/**
 * List code repositories matching the given filters (`GET /repos` or `/repos/details/`).
 *
 * Results are paginated via an opaque cursor: pass the returned `entityCursor` back in on the
 * next call to fetch the following page (`null` cursor means no more pages).
 *
 * @param data - Search/filter parameters (groups, tags, time range, limit, etc.).
 * @param errorHandler - Called with a formatted message if the request fails.
 * @param details - When `true`, request full {@link Repo} objects (`/details/`) instead of summaries.
 * @param cursor - Pagination cursor from a previous call, or `null`/omitted for the first page.
 * @returns The page of repos and the next-page cursor (`entityCursor` is `null` when exhausted).
 *          On failure, returns an empty list and a `null` cursor.
 */
export async function listRepos(
  data: Filters,
  errorHandler: (error: string) => void,
  details?: boolean | null,
  cursor?: string | null,
): Promise<{ entityList: Repo[]; entityCursor: string | null }> {
  let url = '/repos';
  if (details) {
    url += '/details/';
  }
  if (cursor) {
    data['cursor'] = cursor;
  }
  return client
    .get<ApiCursor<Repo>>(url, { params: data })
    .then((res) => {
      if (res?.status == 200 && res.data) {
        const cursor = res.data.cursor ?? null;
        return { entityList: res.data.data, entityCursor: cursor };
      }
      return { entityList: [], entityCursor: null };
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'List Repos');
      return { entityList: [], entityCursor: null };
    });
}

/**
 * Build the path of a repo in a route (the repo's URL with each segment encoded).
 *
 * @param repo - The repo's URL, such as `github.com/org/name`.
 * @returns The encoded path, keeping the slashes between segments.
 */
function repoRoutePath(repo: string): string {
  return repo.split('/').map(encodeURIComponent).join('/');
}

/**
 * Fetch one repo (`GET /repos/data/{repo_path}`).
 *
 * @param repo - The repo's URL, such as `github.com/org/name`.
 * @param errorHandler - Called with the API's error message if the request fails (a repo the user
 *   can't see answers not found, the same as a repo that doesn't exist).
 * @returns The repo, or `null` if it was not found or the request failed.
 */
export async function getRepo(repo: string, errorHandler: (error: string) => void): Promise<Repo | null> {
  return client
    .get<Repo>(`/repos/data/${repoRoutePath(repo)}`)
    .then((res) => (res?.status == 200 && res.data ? res.data : null))
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Get Repo');
      return null;
    });
}

/**
 * List the latest commits, branches, and tags of a repo with their details
 * (`GET /repos/commitish-details/{repo_path}`).
 *
 * @param repo - The repo's URL, such as `github.com/org/name`.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @param limit - The most commitishes to return on the page.
 * @param cursor - The cursor of the page to read; the first page when omitted.
 * @returns The page of commitishes, or `null` if the request failed.
 */
export async function listCommitishDetails(
  repo: string,
  errorHandler: (error: string) => void,
  limit = 100,
  cursor?: string | null,
): Promise<CommitishDetailsPage | null> {
  return client
    .get<CommitishDetailsPage>(`/repos/commitish-details/${repoRoutePath(repo)}`, { params: cursor ? { limit, cursor } : { limit } })
    .then((res) => (res?.status == 200 && res.data ? res.data : null))
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'List Commitishes');
      return null;
    });
}
