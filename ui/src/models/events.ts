/**
 * The kinds of events the event-handler processes; values are the exact wire strings.
 *
 * These key the per-kind maps on the scan stats routes, so the UI keeps the same
 * fixed order everywhere a kind picks a color or a row.
 */
export enum EventType {
  ReactionTrigger = 'ReactionTrigger',
  SigmaScannableResults = 'SigmaScannableResults',
  YaraScannableSample = 'YaraScannableSample',
  YaraHuntChunk = 'YaraHuntChunk',
  WorkflowGraph = 'WorkflowGraph',
  ModelRefresh = 'ModelRefresh',
}

/**
 * Every event kind in its fixed categorical order.
 *
 * Series colors and table rows assign by this order and never cycle, so a kind
 * keeps its hue even when another kind's series is empty or filtered away.
 */
export const EVENT_KINDS: EventType[] = [
  EventType.ReactionTrigger,
  EventType.SigmaScannableResults,
  EventType.YaraScannableSample,
  EventType.YaraHuntChunk,
  EventType.WorkflowGraph,
  EventType.ModelRefresh,
];

/// The queue depths for a single kind of event
export type KindDepth = {
  /// The number of events waiting in the pending queue
  queued: number;
  /// The number of events currently in flight across all owners
  in_flight: number;
};

/**
 * One worker's cumulative counters for one event kind.
 *
 * Counters are cumulative since process start: a worker restart resets them to
 * zero, so consumers derive rates between readings and clamp negative deltas.
 * Nested stats flatten to dotted names (`total.scanned`, `by_entity.*`).
 */
export type WorkerStatsReport = {
  /// Flat counter map (stats-struct field name -> cumulative count)
  counters: Record<string, number>;
  /// When the worker took this reading (RFC3339)
  reported_at: string;
};

/// Point-in-time stats for one kind of event
export type KindStats = {
  /// The number of events waiting in the pending queue
  queued: number;
  /// The number of events currently in flight across all owners
  in_flight: number;
  /// Cumulative counters summed across this kinds live workers
  counters: Record<string, number>;
};

/// The live scan stats aggregate (`GET /events/stats/`)
export type EventStats = {
  /// When this aggregate was assembled (RFC3339)
  taken_at: string;
  /// The number of event handler workers with a live heartbeat
  workers: number;
  /// The per kind depths and summed counters (keyed by {@link EventType})
  kinds: Record<string, KindStats>;
  /// The per worker reports behind the per kind sums (worker -> kind -> report)
  per_worker: Record<string, Record<string, WorkerStatsReport>>;
};

/// One persisted row of the scan stats time series (`GET /events/stats/history/`)
export type ScanStatsSnapshot = {
  /// The quantized instant this row covers (RFC3339)
  snapshot_at: string;
  /// How many live workers were registered when this snapshot was taken
  workers: number;
  /// The per kind depths and summed counters (keyed by {@link EventType})
  kinds: Record<string, KindStats>;
};
