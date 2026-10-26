import axios from 'axios';
import client, { parseRequestError } from './client';
import type { AxiosResponse } from './client';

// project imports
import { EntityRequest } from '@models/entities/requests';
import { OutputMap } from '@models/results';

/**
 * Build the path of a result-file route for a file or repo (each segment encoded).
 *
 * Workflow result keys hold a `#`, which would otherwise start a URL fragment and cut the path.
 * A repo's URL keeps its slashes, since the API reads the repo path from a wildcard route.
 *
 * @param repo - Whether the owner is a repo (`/repos/result-files`) instead of a file (`/files/result-files`).
 * @param owner - The file's SHA256 or the repo's URL.
 * @param segments - The segments that follow the owner (tool, result id, and optionally an entity kind).
 * @returns The route path, without a query string.
 */
function resultFilesPath(repo: boolean, owner: string, segments: string[]): string {
  // a repo URL is encoded one segment at a time so its slashes stay path separators
  const ownerPath = repo ? owner.split('/').map(encodeURIComponent).join('/') : encodeURIComponent(owner);
  return `/${repo ? 'repos' : 'files'}/result-files/${[ownerPath, ...segments.map(encodeURIComponent)].join('/')}`;
}

/**
 * Fetch all tool results for a repo (`GET /repos/results/{repo_path}`).
 *
 * @param repo - The repo's URL, such as `github.com/org/name`.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The {@link OutputMap} of tool results keyed by tool, or `null` if the request failed.
 */
export async function getRepoResults(repo: string, errorHandler: (error: string) => void): Promise<OutputMap | null> {
  const url = '/repos/results/' + repo.split('/').map(encodeURIComponent).join('/');
  return client
    .get<OutputMap>(url)
    .then((res) => (res?.status == 200 && res.data ? res.data : null))
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Get Repo Results');
      return null;
    });
}

/**
 * Fetch all tool results for a file (`GET /files/results/{sha256}`).
 *
 * @param sha256 - The SHA256 of the file whose results to fetch.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @param data - Optional axios request config (e.g. `params` to filter by tool/group).
 * @returns The {@link OutputMap} of tool results keyed by tool, or `null` if the request failed.
 */
export async function getResults(
  sha256: string,
  errorHandler: (error: string) => void,
  data: Record<string, unknown> = {},
): Promise<OutputMap | null> {
  const url = '/files/results/' + sha256;
  return client
    .get<OutputMap>(url, data)
    .then((res) => {
      if (res?.status == 200 && res.data) {
        return res.data;
      }
      return null;
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Get Results');
      return null;
    });
}

/**
 * Decode the body of a failed `arraybuffer` request so the API's error message is not lost.
 *
 * A request that asks for an `ArrayBuffer` gets the error body as bytes too, which the shared
 * error parser cannot read. The bytes are decoded as JSON when they hold JSON, or as text
 * otherwise.
 *
 * @param error - The caught request error.
 * @returns The same error with a byte body of its response replaced by the decoded body.
 */
function decodeBufferError(error: unknown): unknown {
  // only a server response whose body arrived as bytes needs decoding
  if (!axios.isAxiosError(error) || !error.response) return error;
  const data: unknown = error.response.data;
  if (!(data instanceof ArrayBuffer) && !ArrayBuffer.isView(data)) return error;
  // read the bytes as text, then as JSON when the text parses
  const text = new TextDecoder().decode(data);
  try {
    error.response.data = JSON.parse(text) as unknown;
  } catch {
    error.response.data = text;
  }
  return error;
}

/**
 * Download a single result file produced by a tool run (`GET /files/result-files/{sha256}/{tool}/{id}`).
 *
 * The raw axios response is returned (rather than just the body) so callers can read response
 * headers when saving the file.
 *
 * @param sha256 - The SHA256 of the file the result belongs to (or the repo's URL when `repo` is set).
 * @param tool - The tool or workflow result key the result is stored under.
 * @param id - The id of the specific result/run.
 * @param name - The name of the result file to download (sent as the `result_file` query param).
 * @param errorHandler - Called with a formatted message if the request fails.
 * @param repo - Whether `sha256` is a repo's URL, so the file is read from the repo's results.
 * @returns The axios response with the result file bytes as an `ArrayBuffer`, or `null` if the request failed.
 */
export async function getResultsFile(
  sha256: string,
  tool: string,
  id: string,
  name: string,
  errorHandler: (error: string) => void,
  repo = false,
): Promise<AxiosResponse<ArrayBuffer> | null> {
  // build the route for the file's or repo's result file
  const url = resultFilesPath(repo, sha256, [tool, id]);
  const data = {
    result_file: name,
  };
  return client
    .get<ArrayBuffer>(url, { params: data, responseType: 'arraybuffer' })
    .then((res) => {
      if (res?.status == 200) {
        return res;
      }
      return null;
    })
    .catch((error: unknown) => {
      parseRequestError(decodeBufferError(error), errorHandler, 'Get Results File');
      return null;
    });
}

/**
 * Download the entities of a single kind produced by a tool run
 * (`GET /files/result-files/{sha256}/{tool}/{result_id}/{entity_kind}`).
 *
 * The body is a JSON array of {@link EntityRequest}. Unlike {@link getResultsFile}, the target is
 * identified by an `entity_kind` path segment rather than a `result_file` query param.
 *
 * @param sha256 - The SHA256 of the file the result belongs to.
 * @param tool - The tool or workflow result key the result is stored under.
 * @param resultId - The id of the specific result/run.
 * @param entityKind - The entity kind to fetch (e.g. `Device`, `Other`).
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The parsed array of entity requests, or `null` if the request failed.
 */
export async function getResultEntities(
  sha256: string,
  tool: string,
  resultId: string,
  entityKind: string,
  errorHandler: (error: string) => void,
): Promise<EntityRequest[] | null> {
  // build the route for the file's result entities of this kind
  const url = resultFilesPath(false, sha256, [tool, resultId, entityKind]);
  return client
    .get<EntityRequest[] | string>(url)
    .then((res) => {
      if (res?.status == 200 && res.data != null) {
        // tolerate a string body (e.g. when the server doesn't set a JSON content-type)
        return typeof res.data === 'string' ? (JSON.parse(res.data) as EntityRequest[]) : res.data;
      }
      return null;
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Get Result Entities');
      return null;
    });
}
