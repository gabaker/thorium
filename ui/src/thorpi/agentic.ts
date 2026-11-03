import axios from 'axios';
import client, { parseRequestError } from './client';

// project imports
import { AgenticKinds, AgenticPage, AnyAgenticEntity, EntityCreateResponse, EntityNotification } from '@models/agentic';
import { ToolInstanceSummary } from '@models/tools';
import { InstancedAgent, InstancedTool } from '@models/workflows';

interface ApiCursor<T> {
  cursor?: string;
  data: T[];
}

/** An agent of a workflow instance with the spec of each tool it was given (`GET .../toolset`). */
export interface InstancedAgentToolset {
  /** The agent as it was instanced */
  agent: InstancedAgent;
  /** Each tool of the agent with its stored spec */
  tools: { tool: InstancedTool; spec: Record<string, unknown> }[];
}

/**
 * List one page of full entities of a single kind (`GET /entities/details/`).
 *
 * @param kind - The kind of entity to list.
 * @param groups - The groups to list from (the caller's groups when empty).
 * @param errorHandler - Called with a formatted message if the request fails.
 * @param cursor - The cursor from a previous page, or `null` for the first page.
 * @param limit - The most entities to return on the page.
 * @returns The page of entities; empty with a `null` cursor if the request failed.
 */
export async function listAgenticPage<T extends AnyAgenticEntity>(
  kind: AgenticKinds,
  groups: string[],
  errorHandler: (error: string) => void,
  cursor: string | null = null,
  limit = 100,
): Promise<AgenticPage<T>> {
  const params: Record<string, unknown> = { kinds: [kind], limit };
  if (groups.length > 0) {
    params['groups'] = groups;
  }
  if (cursor) {
    params['cursor'] = cursor;
  }
  return client
    .get<ApiCursor<T>>('/entities/details/', { params })
    .then((res) => {
      if (res?.status == 200 && res.data) {
        return { entities: res.data.data, cursor: res.data.cursor ?? null };
      }
      return { entities: [], cursor: null };
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, `List ${kind}`);
      return { entities: [], cursor: null };
    });
}

/**
 * List every entity of a single kind by following the list cursor to the end.
 *
 * @param kind - The kind of entity to list.
 * @param groups - The groups to list from (the caller's groups when empty).
 * @param errorHandler - Called with a formatted message if a request fails or the listing is
 *   cut off at `maxPages`.
 * @param maxPages - The most pages to read, so a huge listing can't loop forever.
 * @returns Every entity read; partial if a page failed or the listing was cut off.
 */
export async function listAllAgentic<T extends AnyAgenticEntity>(
  kind: AgenticKinds,
  groups: string[],
  errorHandler: (error: string) => void,
  maxPages = 20,
): Promise<T[]> {
  const all: T[] = [];
  let cursor: string | null = null;
  for (let page = 0; page < maxPages; page++) {
    const next: AgenticPage<T> = await listAgenticPage<T>(kind, groups, errorHandler, cursor);
    all.push(...next.entities);
    cursor = next.cursor;
    if (!cursor) break;
  }
  // a cursor left over means there are more entities than the cap allows; never pass that off as the whole list
  if (cursor) errorHandler(`List ${kind}: showing only the first ${all.length} entities`);
  return all;
}

/**
 * Fetch one entity by id (`GET /entities/{id}`).
 *
 * @param id - The entity's id.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @param statusHandler - Called with the HTTP status of a failed request (0 when the server gave no response), so a caller can tell a
 *   missing or hidden entity (404, 403) from a read that failed and may work on a retry.
 * @returns The entity, or `null` if it was not found or the request failed.
 */
export async function getAgenticEntity<T extends AnyAgenticEntity>(
  id: string,
  errorHandler: (error: string) => void,
  statusHandler?: (status: number) => void,
): Promise<T | null> {
  return client
    .get<T>(`/entities/${encodeURIComponent(id)}`)
    .then((res) => (res?.status == 200 && res.data ? res.data : null))
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Get Entity');
      statusHandler?.(axios.isAxiosError(error) ? (error.response?.status ?? 0) : 0);
      return null;
    });
}

/**
 * Create an entity from a multipart form (`POST /entities/`).
 *
 * @param form - The form describing the entity.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The creation response (id, name, first tool instance, portability), or `null` on failure.
 */
export async function createAgenticEntity(form: FormData, errorHandler: (error: string) => void): Promise<EntityCreateResponse | null> {
  return client
    .post<EntityCreateResponse>('/entities/', form)
    .then((res) => (res?.status == 200 && res.data ? res.data : null))
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Create Entity');
      return null;
    });
}

/**
 * List a tool's instances, newest first, including deprecated ones (`GET /entities/tools/{id}/instances`).
 *
 * @param tool - The tool's id.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The instance summaries; empty if the request failed.
 */
export async function listToolInstances(tool: string, errorHandler: (error: string) => void): Promise<ToolInstanceSummary[]> {
  return client
    .get<ToolInstanceSummary[]>(`/entities/tools/${encodeURIComponent(tool)}/instances`)
    .then((res) => (res?.status == 200 && Array.isArray(res.data) ? res.data : []))
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'List Tool Instances');
      return [];
    });
}

/**
 * Read an entity's ban notifications (`GET /entities/{id}/notifications`); only model entities have any.
 *
 * @param id - The entity's id.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The notifications; empty if there are none or the request failed.
 */
export async function listEntityNotifications(id: string, errorHandler: (error: string) => void): Promise<EntityNotification[]> {
  return client
    .get<EntityNotification[]>(`/entities/${encodeURIComponent(id)}/notifications`)
    .then((res) => (res?.status == 200 && Array.isArray(res.data) ? res.data : []))
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'List Entity Notifications');
      return [];
    });
}

/**
 * Ask the event handler to build a new instance of a workflow or toolset
 * (`POST /entities/{workflows|toolsets}/{id}/instance`).
 *
 * @param root - Whether the root is a workflow or a toolset.
 * @param id - The root's id.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns `true` if the build was queued (HTTP 202), otherwise `false`.
 */
export async function requestInstance(root: 'workflows' | 'toolsets', id: string, errorHandler: (error: string) => void): Promise<boolean> {
  return client
    .post(`/entities/${root}/${encodeURIComponent(id)}/instance`)
    .then((res) => res?.status == 202 || res?.status == 200 || res?.status == 204)
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Request Instance');
      return false;
    });
}

/**
 * Read one agent of a workflow instance with the stored spec of each tool it was given
 * (`GET /entities/workflows/instances/{id}/toolset`).
 *
 * @param instance - The workflow instance's id.
 * @param agent - The agent's id within the instance.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The agent's toolset, or `null` if the request failed.
 */
export async function getWorkflowToolset(
  instance: string,
  agent: string,
  errorHandler: (error: string) => void,
): Promise<InstancedAgentToolset | null> {
  return client
    .get<InstancedAgentToolset>(`/entities/workflows/instances/${encodeURIComponent(instance)}/toolset`, { params: { agent } })
    .then((res) => (res?.status == 200 && res.data ? res.data : null))
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Get Workflow Toolset');
      return null;
    });
}
