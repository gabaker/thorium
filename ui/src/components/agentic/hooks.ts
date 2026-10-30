import { useQueries } from '@tanstack/react-query';
import { useEffect, useRef } from 'react';

// project imports
import { NamedInstance, Notice, REQUEST_WATCH, requestedNotice, watchRequestedInstance } from '@components/agentic/instances';
import { Severity } from '@components/shared/alerts/AlertBanner';
import { sampleFileNames } from '@components/agentic/runs/display';
import { requestInstance } from '@thorpi/agentic';
import {
  canCreatePipelineInGroupName,
  createBlockReason,
  deleteBlockReason,
  editBlockReason,
  pipelineCreateBlockReason,
} from '@utilities/agentic/access';
import { useAuth } from '@utilities/auth';
import { AgenticListQueryState, invalidateAgentic, useAgenticListQuery } from '@utilities/queries/agentic';
import { useChangeAndInvalidate } from '@utilities/queries/changes';
import { useGroupDetailsQuery } from '@utilities/queries/groups';
import { sampleQueryOptions } from '@utilities/queries/samples';
import { canEditEntityInGroups } from '@utilities/permissions';
import { AgenticKinds, AnyAgenticEntity } from '@models/agentic';
import { Sample } from '@models/files';
import { Group } from '@models/groups';

/**
 * Load every entity of one kind the user can see and expose a reload, reading the list again on every mount so a list
 * page shows what other pages created or deleted.
 *
 * @template T - The entity type of the kind.
 * @param kind - The kind of entity to load.
 * @returns The items, loading flag, last error, and a reload callback.
 */
export function useAgenticList<T extends AnyAgenticEntity>(kind: AgenticKinds): AgenticListQueryState<T> {
  return useAgenticListQuery<T>(kind, true, true);
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
 * Read the details of groups the user can see, to decide which actions to offer; each group is read once and shared
 * through the query cache.
 *
 * A group that can't be read is left out, so the actions it would gate stay available and the
 * API's refusal is shown instead.
 *
 * @param names - The group names to read.
 * @returns The details of the groups read so far.
 */
export function useGroupDetails(names: string[]): Group[] {
  return useGroupDetailsQuery(names);
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
  // the instance lists other pages cached are marked stale once the watch ends, when the new instance is likely built
  const change = useChangeAndInvalidate(invalidateAgentic);
  return () =>
    change(async () => {
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
    });
}

/**
 * Name the samples of a list of SHA256s from their details.
 *
 * @param shas - The SHA256s of the samples.
 * @param samples - Each sample's details in the same order: `null` when it can't be read, `undefined` while it is read.
 * @returns The submitted file names keyed by SHA256; a sample without details or a name gets no entry.
 */
export function sampleNamesByHash(shas: string[], samples: (Pick<Sample, 'submissions'> | null | undefined)[]): Record<string, string> {
  const names: Record<string, string> = {};
  shas.forEach((sha, index) => {
    const sample = samples[index];
    const name = sample ? sampleFileNames(sample) : '';
    if (name !== '') names[sha] = name;
  });
  return names;
}

/**
 * Read the file names of samples so a list of bare SHA256s can say which file is which; each sample is read through the
 * query cache it shares with the launch form's sample checks.
 *
 * A sample that can't be read, or that was submitted without a name, gets no entry; the
 * SHA256 alone is shown for it.
 *
 * @param shas - The SHA256s of the samples.
 * @returns The submitted file names, keyed by SHA256.
 */
export function useSampleNames(shas: string[]): Record<string, string> {
  return useQueries({
    queries: shas.map((sha) => sampleQueryOptions(sha)),
    combine: (results) =>
      sampleNamesByHash(
        shas,
        results.map((result) => result.data),
      ),
  });
}
