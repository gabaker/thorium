//! Query hooks for one tool: the tool with its full version list, read once and shared until a change makes it stale.
import { useQuery } from '@tanstack/react-query';

// project imports
import { listToolInstances } from '@thorpi/agentic';
import { AGENTIC_LIST_STALE_MS, agenticKeys, fetchAgenticEntity } from '@utilities/queries/agentic';
import { AgenticKinds, AnyAgenticEntity } from '@models/agentic';
import { Tool } from '@models/tools';

/** Query keys for tools. */
export const toolKeys = {
  /**
   * A tool with its full version list; it sits under the tool's entity key, so invalidating entities covers it.
   *
   * @param id - The tool's id.
   * @returns The query key.
   */
  withVersions: (id: string) => [...agenticKeys.entity(id), 'versions'] as const,
};

/** A tool read with its versions, or the kind of an id that is not a tool. */
export type ToolRead =
  | {
      /** The tool, with every version including deprecated ones */
      tool: Tool;
      /** Why the full version list couldn't be read (the tool's own trimmed list is kept), or an empty string */
      versionsError: string;
    }
  | {
      /** The id and kind of an entity that is not a tool, such as a tool version */
      other: { id: string; kind: string };
    };

/**
 * Read a tool and its full version list.
 *
 * @param id - The tool's id.
 * @returns The tool with its versions, or the kind of an entity that is not a tool.
 * @throws {AgenticReadError} When the entity is missing, hidden, or the read failed.
 */
export async function fetchToolRead(id: string): Promise<ToolRead> {
  const found = await fetchAgenticEntity<AnyAgenticEntity>(id);
  // an id of another kind (such as a tool instance) has no tool metadata to show
  if (found.kind !== AgenticKinds.Tool) return { other: { id, kind: found.kind } };
  const tool = found as Tool;
  // the entity's own instance list may be trimmed (and is left out when empty), so read the full list including deprecated versions
  let versionsError = '';
  const instances = await listToolInstances(id, (message) => (versionsError = message));
  const versions = instances.length > 0 ? instances : (tool.metadata.Tool.instances ?? []);
  return { tool: { ...tool, metadata: { ...tool.metadata, Tool: { ...tool.metadata.Tool, instances: versions } } }, versionsError };
}

/**
 * Read a tool and its full version list through the query cache, reading it again on every mount so the page opens on
 * what other users changed while showing the cached tool meanwhile.
 *
 * @param id - The tool's id.
 * @returns The query: its data, the failure of the last read, and whether a read is in flight.
 */
export function useToolRead(id: string) {
  return useQuery({
    queryKey: toolKeys.withVersions(id),
    queryFn: () => fetchToolRead(id),
    staleTime: AGENTIC_LIST_STALE_MS,
    refetchOnMount: 'always',
    // a missing or hidden tool stays so, and the page offers its own retry for a failed read
    retry: false,
  });
}
