import { QueryClient, useQuery } from '@tanstack/react-query';

// project imports
import { listPipelines } from '@thorpi/pipelines';
import { pipelineOrderKeys } from '@utilities/queries/runs';
import { Pipeline } from '@models/pipelines';

/** How long a pipeline list stays fresh; a picker opened again within this time reuses the list it read. */
export const PIPELINE_LIST_STALE_MS = 60 * 1000;

/** Query keys for pipeline lists. */
export const pipelineKeys = {
  /** Every pipeline list query */
  lists: ['pipelines'] as const,
  /**
   * The pipelines of a set of groups.
   *
   * @param groups - The groups, in any order.
   * @returns The query key, the same for the same groups in any order.
   */
  details: (groups: string[]) => ['pipelines', 'details', [...groups].sort().join(',')] as const,
};

/**
 * Mark every cached pipeline list and run's pipeline order stale, for a pipeline created, updated, or deleted.
 *
 * @param client - The query client.
 * @returns A promise that settles once the reads in use have been made again.
 */
export async function invalidatePipelines(client: QueryClient): Promise<void> {
  await Promise.all([
    client.invalidateQueries({ queryKey: pipelineKeys.lists }),
    client.invalidateQueries({ queryKey: pipelineOrderKeys }),
  ]);
}

/** The pipelines of several groups, with the groups whose listing failed. */
export interface GroupPipelines {
  /** The pipelines read, every group's in one list */
  pipelines: Pipeline[];
  /** The message of each group whose listing failed */
  errors: string[];
}

/**
 * Read the pipelines of every given group at once, keeping each failure to show.
 *
 * @param groups - The groups to list.
 * @returns The pipelines read and the failures; never throws.
 */
export async function fetchGroupPipelines(groups: string[]): Promise<GroupPipelines> {
  const errors: string[] = [];
  const lists = await Promise.all(
    groups.map(async (group) => {
      const details = (await listPipelines(group, (message) => errors.push(message), true)) as Pipeline[] | null;
      return details ?? [];
    }),
  );
  return { pipelines: lists.flat(), errors };
}

/** The pipeline list a page uses. */
export interface PipelineListQueryState extends GroupPipelines {
  /** Whether the first read is in flight */
  loading: boolean;
}

/**
 * Load the pipelines of the given groups through the query cache.
 *
 * @param groups - The groups to list.
 * @param enabled - Whether to read the lists; a disabled list reads nothing and reports empty, not loading.
 * @returns The pipelines, the failures, and whether the first read is in flight.
 */
export function usePipelineListQuery(groups: string[], enabled = true): PipelineListQueryState {
  const query = useQuery({
    queryKey: pipelineKeys.details(groups),
    queryFn: () => fetchGroupPipelines(groups),
    staleTime: PIPELINE_LIST_STALE_MS,
    enabled,
  });
  return { pipelines: query.data?.pipelines ?? [], errors: query.data?.errors ?? [], loading: enabled && query.isPending };
}
