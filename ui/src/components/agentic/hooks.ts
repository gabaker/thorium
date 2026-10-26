import { useCallback, useEffect, useRef, useState } from 'react';

// project imports
import { NamedInstance, Notice, REQUEST_WATCH, requestedNotice, watchRequestedInstance } from '@components/agentic/instances';
import { Severity } from '@components/shared/alerts/AlertBanner';
import { listAllAgentic, requestInstance } from '@thorpi/agentic';
import { sampleFileNames } from '@components/agentic/runs/display';
import { getFileDetails } from '@thorpi/files';
import { getGroup } from '@thorpi/groups';
import {
  canCreatePipelineInGroupName,
  createBlockReason,
  deleteBlockReason,
  editBlockReason,
  hasGroupDetails,
  pipelineCreateBlockReason,
} from '@utilities/agentic/access';
import { useAuth } from '@utilities/auth';
import { canEditEntityInGroups } from '@utilities/permissions';
import { AgenticKinds, AnyAgenticEntity } from '@models/agentic';
import { Group } from '@models/groups';

/** The state of a list of entities loaded for a page. */
export interface AgenticListState<T> {
  /** The entities read so far */
  items: T[];
  /** Whether a load is in flight */
  loading: boolean;
  /** The last error message, or an empty string */
  error: string;
  /** Read the list again */
  reload: () => void;
}

/**
 * Load every entity of one kind the user can see and expose a reload.
 *
 * @param kind - The kind of entity to load.
 * @returns The items, loading flag, last error, and a reload callback.
 */
export function useAgenticList<T extends AnyAgenticEntity>(kind: AgenticKinds): AgenticListState<T> {
  const [items, setItems] = useState<T[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [tick, setTick] = useState(0);
  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError('');
    void listAllAgentic<T>(kind, [], (message) => {
      // a load that was superseded or unmounted must not report its errors over the current one
      if (!cancelled) setError(message);
    })
      .then((all) => {
        if (!cancelled) {
          setItems(all);
          setLoading(false);
        }
      })
      .catch((reason: unknown) => {
        // a reply the listing could not read ends the load with an error rather than leaving it loading forever
        if (!cancelled) {
          setError(`List ${kind}: unexpected reply (${reason instanceof Error ? reason.message : String(reason)})`);
          setLoading(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [kind, tick]);
  const reload = useCallback(() => setTick((value) => value + 1), []);
  return { items, loading, error, reload };
}

/**
 * Get the names of the groups the signed in user belongs to.
 *
 * @returns The group names, sorted.
 */
export function useGroupNames(): string[] {
  const { userInfo } = useAuth();
  return [...(userInfo?.groups ?? [])].sort();
}

/**
 * Read the details of groups the user can see, to decide which actions to offer.
 *
 * A group that can't be read is left out, so the actions it would gate stay available and the
 * API's refusal is shown instead.
 *
 * @param names - The group names to read.
 * @returns The details of the groups read so far.
 */
export function useGroupDetails(names: string[]): Group[] {
  const [details, setDetails] = useState<Group[]>([]);
  // a stable key so a new array of the same names doesn't read the groups again
  const key = [...names].sort().join('\n');
  useEffect(() => {
    let cancelled = false;
    const wanted = key === '' ? [] : key.split('\n');
    void Promise.all(wanted.map((name) => getGroup(name, () => undefined))).then((read) => {
      if (!cancelled) setDetails(read.filter((group) => hasGroupDetails(group)));
    });
    return () => {
      cancelled = true;
    };
  }, [key]);
  return details;
}

/** What the signed in user may do with an entity, with the reason for each refusal. */
export interface EntityAccess {
  /** Why the user can't edit the entity, or an empty string when they can */
  editReason: string;
  /** Why the user can't delete the entity, or an empty string when they can */
  deleteReason: string;
}

/**
 * Work out what the signed in user may do with a shared entity from the roles they hold in its groups.
 *
 * @param groups - The names of the entity's groups.
 * @param submitter - The user that submitted the entity.
 * @returns The reasons editing and deleting are blocked; empty strings for what is allowed.
 */
export function useEntityAccess(groups: string[], submitter: string): EntityAccess {
  const { userInfo } = useAuth();
  const details = useGroupDetails(groups);
  return {
    editReason: editBlockReason(details, userInfo),
    deleteReason: deleteBlockReason(details, userInfo, submitter),
  };
}

/**
 * Work out why the signed in user can't create an entity.
 *
 * @returns The reason creating is blocked, or an empty string when it is allowed.
 */
export function useCreateBlockReason(): string {
  const { userInfo } = useAuth();
  const details = useGroupDetails(userInfo?.groups ?? []);
  return createBlockReason(details, userInfo);
}

/**
 * Get the names of the signed in user's groups where the user may create and edit entities.
 *
 * @returns The group names, sorted; a group whose details are unknown stays in the list.
 */
export function useEditableGroupNames(): string[] {
  const { userInfo } = useAuth();
  const names = useGroupNames();
  const details = useGroupDetails(names);
  return userInfo === null ? names : names.filter((name) => canEditEntityInGroups(details, [name], userInfo));
}

/**
 * Work out why the signed in user can't create a pipeline.
 *
 * @returns The reason creating a pipeline is blocked, or an empty string when it is allowed.
 */
export function usePipelineCreateBlockReason(): string {
  const { userInfo } = useAuth();
  const details = useGroupDetails(userInfo?.groups ?? []);
  return pipelineCreateBlockReason(details, userInfo);
}

/**
 * Get the names of the signed in user's groups where the user may create a pipeline.
 *
 * @returns The group names, sorted; a group whose details are unknown stays in the list.
 */
export function usePipelineGroupNames(): string[] {
  const { userInfo } = useAuth();
  const names = useGroupNames();
  const details = useGroupDetails(names);
  return names.filter((name) => canCreatePipelineInGroupName(details, name, userInfo));
}

/** What {@link useRequestInstance} needs from the page that asks for an instance build. */
export interface RequestInstanceOptions<T extends NamedInstance> {
  /** Whether the root is a workflow or a toolset */
  root: 'workflows' | 'toolsets';
  /** The root's id */
  id: string;
  /** The root's instances as the page shows them, newest first */
  instances: T[];
  /** Read the root's instances again, newest first */
  read: () => Promise<T[]>;
  /** Show the instances the page just read */
  setInstances: (instances: T[]) => void;
  /** Show an error message */
  setError: (message: string) => void;
  /** Show or replace the notice; takes the new notice or a function of the current notice */
  setNotice: (notice: Notice | null | ((current: Notice | null) => Notice | null)) => void;
}

/**
 * Read the errors that made an instance Invalid.
 *
 * @param root - Whether the instance belongs to a workflow or a toolset.
 * @param item - The instance entity.
 * @returns The errors when the instance is Invalid, or undefined when it is Ready or has no readable status.
 */
function invalidErrors(root: 'workflows' | 'toolsets', item: unknown): string[] | undefined {
  // the status sits in the metadata variant for the instance kind
  const key = root === 'workflows' ? 'WorkflowInstance' : 'ToolSetInstance';
  const status = (item as { metadata?: Record<string, { status?: unknown } | undefined> }).metadata?.[key]?.status;
  // Ready is a bare string; Invalid carries its errors
  if (typeof status !== 'object' || status === null || !('Invalid' in status)) return undefined;
  const errors = (status as { Invalid?: { errors?: string[] } }).Invalid?.errors;
  return errors ?? [];
}

/**
 * Build the action that asks for an instance build and then watches for the instance it produces.
 *
 * The notice first says the build was queued, since the backend skips a build when nothing changed,
 * and is then replaced by what the watch found. The watch stops when the page is left.
 *
 * @param options - The root to build for and the page state to update.
 * @returns A function that queues the build and resolves once the watch ends.
 */
export function useRequestInstance<T extends NamedInstance>(options: RequestInstanceOptions<T>): () => Promise<void> {
  const { root, id, instances, read, setInstances, setError, setNotice } = options;
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  return async () => {
    setError('');
    const before = instances;
    if (!(await requestInstance(root, id, setError))) return;
    const queued: Notice = { text: requestedNotice(before[0]), severity: Severity.Success };
    setNotice(queued);
    const outcome = await watchRequestedInstance(
      async () => {
        const current = await read();
        if (mounted.current) setInstances(current);
        return current.map((item) => ({ id: item.id, name: item.name, invalid: invalidErrors(root, item) }));
      },
      before,
      { ...REQUEST_WATCH, cancelled: () => !mounted.current },
    );
    // a notice from a later action is left alone, and a left page shows nothing
    if (mounted.current) setNotice((current) => (current === queued ? outcome : current));
  };
}

/**
 * Read the file names of samples so a list of bare SHA256s can say which file is which.
 *
 * A sample that can't be read, or that was submitted without a name, gets no entry; the
 * SHA256 alone is shown for it.
 *
 * @param shas - The SHA256s of the samples.
 * @returns The submitted file names, keyed by SHA256.
 */
export function useSampleNames(shas: string[]): Record<string, string> {
  const [names, setNames] = useState<Record<string, string>>({});
  const key = shas.join(',');
  useEffect(() => {
    let cancelled = false;
    // a different set of samples starts from no names
    setNames({});
    void Promise.all(
      shas.map(async (sha) => {
        const sample = await getFileDetails(sha, () => undefined);
        return [sha, sample ? sampleFileNames(sample) : ''] as const;
      }),
    ).then((found) => {
      if (!cancelled) setNames(Object.fromEntries(found.filter(([, name]) => name !== '')));
    });
    return () => {
      cancelled = true;
    };
  }, [key]);
  return names;
}
