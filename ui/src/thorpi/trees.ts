import { BranchNode, Graph, Seed } from '@models/trees';
import client, { parseRequestError } from './client';
import JSONBigInt from 'json-bigint';

// this will force big ints to be strings
const JSONBigString = JSONBigInt({ storeAsString: true });

/**
 * Normalize every node-id value in a parsed graph to a string.
 *
 * The API serializes u64 node ids as JSON numbers in `initial`/`growable`/`sent` and in each branch's
 * `node` field, while `data_map`/`branches` object keys are always strings. `json-bigint` (storeAsString)
 * only converts values longer than 15 digits to strings, so smaller hashes arrive as JS numbers. Downstream
 * code keys `Set`/`Map` lookups by string ids, so a numeric id silently misses (duplicate/short-hash nodes
 * then show no children and can't be grown). Coercing every id to a string here keeps the whole app
 * consistent regardless of hash magnitude.
 *
 * @param graph - The freshly parsed graph (mutated in place).
 * @returns The same graph with all node ids as strings.
 */
const normalizeGraphIds = (graph: Graph): Graph => {
  // FIX (number/string ids): count coercions so the fix's effect is visible in the console
  let coerced = 0;
  const toStr = (id: unknown): string => {
    if (typeof id !== 'string') coerced += 1;
    return String(id);
  };
  if (Array.isArray(graph.initial)) graph.initial = graph.initial.map(toStr);
  if (Array.isArray(graph.growable)) graph.growable = graph.growable.map(toStr);
  if (Array.isArray(graph.sent)) graph.sent = graph.sent.map(toStr);
  const normBranches = (branches?: { [nodeId: string]: BranchNode[] }): void => {
    if (!branches) return;
    for (const key of Object.keys(branches)) {
      for (const branch of branches[key]) {
        branch.node = toStr(branch.node);
      }
    }
  };
  normBranches(graph.branches);
  normBranches(graph.hint_branches);
  // DEBUG (remove after diagnosing number/string id bugs): non-zero means short hashes were arriving as
  // numbers (the cause of duplicates showing no children); zero after the fix confirms all ids are strings.
  if (coerced > 0) {
    console.warn('[thorpi-debug] normalizeGraphIds coerced numeric ids to strings', { count: coerced, tree: graph.id });
  }
  return graph;
};

/**
 * Build the initial association graph from a set of seed nodes (`POST /trees/`).
 *
 * The response is parsed with a BigInt-aware JSON parser (storing big integers as strings) so
 * large numeric ids survive the round-trip without precision loss.
 *
 * @param data - The {@link Seed} describing the starting node(s) for the graph.
 * @param filterChildless - When `true`, omit nodes that have no children from the result.
 * @param limit - Maximum number of nodes to expand per growable node.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The built {@link Graph}, or `null` if the request failed.
 */
export const getInitialTree = async (
  data: Seed,
  filterChildless: boolean,
  limit: number,
  errorHandler: (error: string) => void,
): Promise<Graph | null> => {
  const url = '/trees/';
  const params: { filter_childless?: boolean; limit: number } = { limit };
  if (filterChildless) {
    params['filter_childless'] = filterChildless;
  }
  return client
    .post<string>(url, { ...data }, { transformResponse: [(data: string) => data], params: params })
    .then((res) => {
      if (res && res.status && res.status == 200) {
        // FIX (number/string ids): normalize all node ids to strings at the parse boundary
        return normalizeGraphIds(JSONBigString.parse(res.data) as Graph);
      }
      return null;
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Build Tree');
      return null;
    });
};

/**
 * Expand an existing association graph by growing from the given nodes (`PATCH /trees/{id}`).
 *
 * Like {@link getInitialTree}, the response is parsed with a BigInt-aware JSON parser to
 * preserve large numeric ids.
 *
 * @param id - The id of the existing graph/tree to grow.
 * @param nodes - The ids of the growable nodes to expand.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @param limit - Maximum number of children to expand per node (defaults to 1).
 * @returns The updated {@link Graph}, or `null` if the request failed.
 */
export const growTree = async (id: string, nodes: string[], errorHandler: (error: string) => void, limit = 1): Promise<Graph | null> => {
  const url = `/trees/${id}`;
  const params: { limit: number } = { limit };
  // DEBUG (remove after diagnosing 400s): the exact PATCH payload sent over the wire and id value types
  console.warn('[thorpi-debug] growTree request', { tree: id, limit, nodes, nodeTypes: Array.from(new Set(nodes.map((n) => typeof n))) });
  return client
    .patch<string>(url, { growable: nodes }, { transformResponse: [(data: string) => data], params: params })
    .then((res) => {
      if (res?.status && res.status == 200 && res.data) {
        // FIX (number/string ids): normalize all node ids to strings at the parse boundary
        const parsed = normalizeGraphIds(JSONBigString.parse(res.data) as Graph);
        // DEBUG (remove): compare the requested tree id with the id the server actually returned
        console.warn('[thorpi-debug] growTree response', { requestedTree: id, responseTree: parsed.id });
        return parsed;
      }
      return null;
    })
    .catch((error: unknown) => {
      // DEBUG (remove): surface the failing request (this is where the "not a valid growable node" 400 lands)
      console.warn('[thorpi-debug] growTree ERROR', { tree: id, nodes });
      parseRequestError(error, errorHandler, 'Grow Tree');
      return null;
    });
};
