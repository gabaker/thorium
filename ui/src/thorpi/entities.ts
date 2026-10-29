import { Filters } from '@models/search';
import { EntityTypes } from '@models/entities/entities';
import client, { parseRequestError } from './client';

interface ApiCursor<T> {
  cursor?: string;
  data: T[];
}

/**
 * Create a new entity (`POST /entities/`).
 *
 * @param data - Multipart form data describing the entity (type, fields, groups, optional image).
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns An object with the new entity's `id`, or `null` if the request failed.
 */
export const createEntity = async (data: FormData, errorHandler: (error: string) => void): Promise<{ id: string } | null> => {
  const url = '/entities/';
  return client
    .post<{ id: string }>(url, data)
    .then((res) => {
      if (res && res.status && res.status == 200 && res.data) {
        return res.data;
      }
      return null;
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Create Entity');
      return null;
    });
};

/**
 * Fetch a single entity by id (`GET /entities/{id}`).
 *
 * @param id - The id of the entity to fetch.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The entity, or `null` if not found or the request failed.
 */
export const getEntity = async (id: string, errorHandler: (error: string) => void): Promise<EntityTypes | null> => {
  const url = `/entities/${id}`;
  return client
    .get<EntityTypes>(url)
    .then((res) => {
      if (res && res.status && res.status == 200 && res.data) {
        return res.data;
      }
      return null;
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Get Entity');
      return null;
    });
};

/**
 * Update an existing entity (`PATCH /entities/{id}`).
 *
 * @param id - The id of the entity to update.
 * @param data - Multipart form data with the fields to change (and optionally a new image).
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns `true` if the update succeeded (HTTP 204), otherwise `false`.
 */
export const updateEntity = async (id: string, data: FormData, errorHandler: (error: string) => void): Promise<boolean> => {
  const url = `/entities/${id}`;
  return client
    .patch(url, data)
    .then((res) => {
      if (res && res.status && res.status == 204) {
        return true;
      }
      return false;
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Update Entity');
      return false;
    });
};

/**
 * Delete an entity by id (`DELETE /entities/{id}`).
 *
 * @param id - The id of the entity to delete.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns `true` if the deletion succeeded (HTTP 204), otherwise `false`.
 */
export const deleteEntity = async (id: string, errorHandler: (error: string) => void): Promise<boolean> => {
  const url = `/entities/${id}`;
  return client
    .delete(url)
    .then((res) => {
      if (res?.status && res.status == 204) {
        return true;
      }
      return false;
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Delete Entity');
      return false;
    });
};

/**
 * Delete tag values from an entity (`DELETE /entities/tags/{id}`).
 *
 * The route takes a JSON body (`{groups, tags}`) on the DELETE, so the body rides in
 * axios' `data` option. Deleting a rule's `YaraRuleStatus` values is the re-admit path
 * for a quarantined yara rule; the backend gates this on group-edit permissions.
 *
 * @param id - The id of the entity to delete tags from.
 * @param tags - Map of tag keys to the value lists to delete.
 * @param groups - The groups to delete these tags from.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns `true` if the tags were deleted (HTTP 204), otherwise `false`.
 */
export const deleteEntityTags = async (
  id: string,
  tags: Record<string, string[]>,
  groups: string[],
  errorHandler: (error: string) => void,
): Promise<boolean> => {
  const url = `/entities/tags/${id}`;
  return client
    .delete(url, { data: { groups, tags } })
    .then((res) => {
      if (res?.status && res.status == 204) {
        return true;
      }
      return false;
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Delete Entity Tags');
      return false;
    });
};

/**
 * Exempt a yara rule from noisy/slow quarantine (`POST /entities/rules/{id}/quarantine-exemption`).
 *
 * Admin only. Setting the exemption also clears any Noisy/Slow `YaraRuleStatus` values
 * server-side, so the rule rejoins scanning in the same action.
 *
 * @param id - The id of the yara rule to exempt.
 * @param errorHandler - Called with a formatted message if the request fails (non-admin
 *                       401s and non-yara-rule 400s surface the API's error text).
 * @returns `true` when the API acknowledged the exemption with a 204.
 */
export const setQuarantineExemption = async (id: string, errorHandler: (error: string) => void): Promise<boolean> => {
  const url = `/entities/rules/${id}/quarantine-exemption`;
  return client
    .post(url)
    .then((res) => {
      if (res?.status && res.status == 204) {
        return true;
      }
      return false;
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Set Quarantine Exemption');
      return false;
    });
};

/**
 * Clear a yara rule's quarantine exemption (`DELETE /entities/rules/{id}/quarantine-exemption`).
 *
 * Admin only. Clearing only removes the rule's protection from future quarantine; it
 * never adds or removes any `YaraRuleStatus` values.
 *
 * @param id - The id of the yara rule to clear the exemption from.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns `true` when the API acknowledged the clear with a 204.
 */
export const clearQuarantineExemption = async (id: string, errorHandler: (error: string) => void): Promise<boolean> => {
  const url = `/entities/rules/${id}/quarantine-exemption`;
  return client
    .delete(url)
    .then((res) => {
      if (res?.status && res.status == 204) {
        return true;
      }
      return false;
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Clear Quarantine Exemption');
      return false;
    });
};

export interface EntityImage {
  url: string;
  isSvg: boolean;
}

/**
 * Fetch an entity's image as a browser-displayable object URL (`GET /entities/{id}/image`).
 *
 * The blob is wrapped in an object URL via `URL.createObjectURL`; callers are responsible
 * for revoking it with `URL.revokeObjectURL` when done to avoid leaking memory. This
 * function intentionally swallows errors (returns `null`) since a missing image is expected.
 *
 * @param entityId - The id of the entity whose image to fetch.
 * @returns An {@link EntityImage} with the object URL and an `isSvg` flag, or `null` if there is no image.
 */
export const fetchEntityImage = async (entityId: string): Promise<EntityImage | null> => {
  return client
    .get<Blob>(`/entities/${entityId}/image`, { responseType: 'blob' })
    .then((res) => {
      if (res?.status === 200 && res.data) {
        const blob = res.data;
        return {
          url: URL.createObjectURL(blob),
          isSvg: blob.type === 'image/svg+xml',
        };
      }
      return null;
    })
    .catch(() => null);
};

/**
 * A thin entity list line as serialized by the summary list route (`GET /entities/`).
 *
 * Mirrors the API's `EntityListLine`: only `id`, `name`, `kind`, and `created` come over the
 * wire (`groups` is skipped server-side, and there are no tags or kind-specific metadata).
 * Full {@link EntityTypes} objects only come back from the details route (`GET /entities/details/`).
 */
export interface EntityListLine {
  /** The entity's unique id */
  id: string;
  /** The entity's name */
  name: string;
  /** The kind of entity this is (without the kind's metadata) */
  kind: EntityTypes['kind'];
  /** The time this entity was created */
  created: string;
}

/**
 * List entities matching the given filters (`GET /entities/` or `GET /entities/details/` —
 * both routes require the trailing slash; the bare `/entities` path is a 404).
 *
 * Results are paginated via an opaque cursor: pass the returned `entityCursor` back in on the
 * next call to fetch the following page (`null` cursor means no more pages).
 *
 * @param data - Search/filter parameters (groups, tags, time range, limit, etc.).
 * @param errorHandler - Called with a formatted message if the request fails.
 * @param details - When `true`, request full entity objects (`/details/`); when `false`, each
 *                  returned line is a thin {@link EntityListLine} (id/name/kind/created only)
 *                  despite the `EntityTypes[]` return type — do not read tags/metadata off it.
 * @param cursor - Pagination cursor from a previous call, or `null` for the first page.
 * @returns The page of entities and the next-page cursor (`entityCursor` is `null` when exhausted).
 *          On failure, returns an empty list and a `null` cursor.
 */
export const listEntities = async (
  data: Filters,
  errorHandler: (error: string) => void,
  details: boolean,
  cursor: string | null,
): Promise<{ entityList: EntityTypes[]; entityCursor: string | null }> => {
  // both list routes require the trailing slash (the bare path is a 404)
  let url = '/entities/';
  if (details) {
    url += 'details/';
  }
  if (cursor) {
    data.cursor = cursor;
  }
  return client
    .get<ApiCursor<EntityTypes>>(url, { params: data })
    .then((res) => {
      if (res?.status && res.status == 200 && res.data) {
        const cursor = res.data.cursor ?? null;
        return { entityList: res.data.data, entityCursor: cursor };
      }
      return { entityList: [], entityCursor: null };
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'List Entity');
      return { entityList: [], entityCursor: null };
    });
};
