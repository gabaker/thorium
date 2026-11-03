import { useEffect, useMemo, useState } from 'react';
import { Badge, Col, ProgressBar, Row, Table } from 'react-bootstrap';
import { Link } from 'react-router';
import styled from 'styled-components';

// project imports
import Page from '@components/pages/Page';
import Subtitle from '@components/shared/titles/Subtitle';
import Title from '@components/shared/titles/Title';
import AlertBanner from '@components/shared/alerts/AlertBanner';
import { getDetailsBasePathByEntity } from '@components/entities/details/EntityDetailsRoutes';
import { huntBarClass, huntStatusVariant } from '@components/entities/details/configs/huntDisplay';
import { laneFraction, overallProgress } from '@components/entities/details/configs/huntProgress';
import VisxLineChart, { VisxLinePoint, VisxLineSeries } from '@dashboards/Dashboard/charts/VisxLineChart';
import { ChartBlock, ChartHeader, ChartTitle, KeyPicker } from '@dashboards/Dashboard/charts/charts.styled';
import { getEventStats, getEventStatsHistory } from '@thorpi/events';
import { getHuntSystemStatus } from '@thorpi/hunts';
import { EVENT_KINDS, EventStats, EventType, ScanStatsSnapshot } from '@models/events';
import { Entities } from '@models/entities/entities';
import { HuntStatusLine, HuntSystemStatus, isActiveHuntStatus } from '@models/entities/hunts';
import { spacers } from '@styles';

/**
 * The primary throughput counter per event kind, pinned by name.
 *
 * Rates derive from these cumulative counters between consecutive readings;
 * sigma's is a dotted path because its stats struct is nested.
 */
export const PRIMARY_RATE_COUNTERS: Record<EventType, string> = {
  [EventType.ReactionTrigger]: 'seen',
  [EventType.SigmaScannableResults]: 'total.scanned',
  [EventType.YaraScannableSample]: 'scanned',
  [EventType.YaraHuntChunk]: 'scanned',
  [EventType.WorkflowGraph]: 'events_processed',
  [EventType.ModelRefresh]: 'events_processed',
};

/// Short display labels per event kind, in the fixed categorical order.
export const KIND_LABELS: Record<EventType, string> = {
  [EventType.ReactionTrigger]: 'Triggers',
  [EventType.SigmaScannableResults]: 'Sigma scans',
  [EventType.YaraScannableSample]: 'Yara scans',
  [EventType.YaraHuntChunk]: 'Hunt chunks',
  [EventType.WorkflowGraph]: 'Workflow builds',
  [EventType.ModelRefresh]: 'Model refreshes',
};

/**
 * Series colors per event kind: fixed assignment, never cycled, so a kind
 * keeps its hue even when another series is empty. The hexes are the
 * validated fallbacks for the theme variables defined in colors.scss.
 */
export const KIND_COLORS: Record<EventType, string> = {
  [EventType.ReactionTrigger]: 'var(--thorium-series-1, #2a78d6)',
  [EventType.SigmaScannableResults]: 'var(--thorium-series-2, #eb6834)',
  [EventType.YaraScannableSample]: 'var(--thorium-series-3, #1baf7a)',
  [EventType.YaraHuntChunk]: 'var(--thorium-series-4, #eda100)',
  [EventType.WorkflowGraph]: 'var(--thorium-series-5, #7a5bd6)',
  [EventType.ModelRefresh]: 'var(--thorium-series-6, #cf4f96)',
};

/// How often the census and live stats re-poll, in milliseconds.
const POLL_INTERVAL_MS = 10_000;

/// The fallback staleness limit when the snapshot cadence cannot be derived.
const STALE_FALLBACK_MS = 90_000;

/// The selectable history ranges for the time-series charts.
export const HISTORY_RANGES = [
  { id: '1h', label: 'Last hour', secs: 3_600 },
  { id: '6h', label: 'Last 6 hours', secs: 21_600 },
  { id: '24h', label: 'Last 24 hours', secs: 86_400 },
  { id: '7d', label: 'Last 7 days', secs: 604_800 },
] as const;

/**
 * How often the history charts refresh for a given range, in milliseconds.
 *
 * The server downsamples every range to roughly 1,000 points, so a new point
 * exists at most every range/1000 seconds — refreshing faster than that adds
 * bytes but no information. Floored at 60s so short ranges stay lively.
 *
 * @param rangeSecs - The selected history range in seconds.
 * @returns The refresh interval in milliseconds.
 */
export function historyRefreshMs(rangeSecs: number): number {
  return Math.max(60, rangeSecs / 1_000) * 1_000;
}

/**
 * The median spacing between consecutive sorted timestamps.
 *
 * @param timestamps - Epoch-millisecond timestamps sorted ascending.
 * @returns The median spacing in milliseconds, or null with fewer than 2 points.
 */
export function medianSpacingMs(timestamps: number[]): number | null {
  if (timestamps.length < 2) {
    return null;
  }
  const diffs: number[] = [];
  for (let index = 1; index < timestamps.length; index++) {
    diffs.push(timestamps[index] - timestamps[index - 1]);
  }
  diffs.sort((a, b) => a - b);
  return diffs[Math.floor(diffs.length / 2)];
}

/**
 * The spacing beyond which two consecutive snapshots are a gap, not a sample.
 *
 * Derived from the data (2.5x the median inter-snapshot spacing) rather than
 * any configured cadence, so an install running a non-default
 * `stats_snapshot_secs` never renders every interval as a gap.
 *
 * @param timestamps - Epoch-millisecond timestamps sorted ascending.
 * @returns The gap threshold in milliseconds; Infinity when it cannot be derived.
 */
export function gapThresholdMs(timestamps: number[]): number {
  const median = medianSpacingMs(timestamps);
  // floor the threshold so clustered timestamps never gap everything
  return median === null ? Number.POSITIVE_INFINITY : Math.max(2.5 * median, 1_000);
}

/**
 * Parse snapshots into sorted (timestamp, snapshot) pairs, dropping bad stamps.
 *
 * @param snapshots - The history rows to parse.
 * @returns The parseable rows oldest first.
 */
function parseSnapshots(snapshots: ScanStatsSnapshot[]): { ts: number; snapshot: ScanStatsSnapshot }[] {
  return snapshots
    .map((snapshot) => ({ ts: Date.parse(snapshot.snapshot_at), snapshot }))
    .filter((pair) => Number.isFinite(pair.ts))
    .sort((a, b) => a.ts - b.ts);
}

/**
 * Derive one kind's scans-per-second series from consecutive snapshots.
 *
 * Rates divide each counter delta by the pair's real spacing; a negative delta
 * (a worker restart reset the cumulative counter) clamps to zero so restarts
 * render as dips, never spikes; and a pair spanning longer than the derived
 * gap threshold becomes a null point so the chart breaks the line instead of
 * interpolating across a window where nothing was measured.
 *
 * @param snapshots - The history rows oldest first.
 * @param kind - The event kind to derive a rate series for.
 * @returns The rate points oldest first, with nulls marking gaps.
 */
export function deriveRateSeries(snapshots: ScanStatsSnapshot[], kind: EventType): VisxLinePoint[] {
  const parsed = parseSnapshots(snapshots);
  const threshold = gapThresholdMs(parsed.map((pair) => pair.ts));
  const counterName = PRIMARY_RATE_COUNTERS[kind];
  const points: VisxLinePoint[] = [];
  for (let index = 1; index < parsed.length; index++) {
    const previous = parsed[index - 1];
    const current = parsed[index];
    const dtMs = current.ts - previous.ts;
    // duplicate timestamps carry no rate information
    if (dtMs <= 0) {
      continue;
    }
    // a pair spanning a dead window is a gap, never a rate
    if (dtMs > threshold) {
      points.push({ ts: current.ts, value: null });
      continue;
    }
    const before = previous.snapshot.kinds[kind]?.counters[counterName] ?? 0;
    const after = current.snapshot.kinds[kind]?.counters[counterName] ?? 0;
    // clamp counter resets to zero: a restart is a dip, never a spike
    const delta = Math.max(0, after - before);
    points.push({ ts: current.ts, value: delta / (dtMs / 1_000) });
  }
  return points;
}

/**
 * Derive one kind's queued-depth gauge series from snapshots.
 *
 * Gauges carry one point per snapshot; a spacing beyond the derived gap
 * threshold inserts a null breaker so the line never bridges a dead window.
 *
 * @param snapshots - The history rows oldest first.
 * @param kind - The event kind to derive a depth series for.
 * @returns The depth points oldest first, with nulls marking gaps.
 */
export function deriveQueueSeries(snapshots: ScanStatsSnapshot[], kind: EventType): VisxLinePoint[] {
  const parsed = parseSnapshots(snapshots);
  const threshold = gapThresholdMs(parsed.map((pair) => pair.ts));
  const points: VisxLinePoint[] = [];
  for (let index = 0; index < parsed.length; index++) {
    // break the line inside any dead window before this point
    if (index > 0 && parsed[index].ts - parsed[index - 1].ts > threshold) {
      points.push({ ts: parsed[index - 1].ts + 1, value: null });
    }
    points.push({ ts: parsed[index].ts, value: parsed[index].snapshot.kinds[kind]?.queued ?? 0 });
  }
  return points;
}

/**
 * The current total scans-per-second from two consecutive live readings.
 *
 * Sums the primary counter delta across every kind, clamping each kind's
 * negative delta (a worker restart) to zero.
 *
 * @param previous - The previous live aggregate, if one exists yet.
 * @param current - The newest live aggregate.
 * @returns Scans per second, or null until two readings exist.
 */
export function liveRate(previous: EventStats | null, current: EventStats | null): number | null {
  if (!previous || !current) {
    return null;
  }
  const dtMs = Date.parse(current.taken_at) - Date.parse(previous.taken_at);
  if (!Number.isFinite(dtMs) || dtMs <= 0) {
    return null;
  }
  let total = 0;
  for (const kind of EVENT_KINDS) {
    const counterName = PRIMARY_RATE_COUNTERS[kind];
    const before = previous.kinds[kind]?.counters[counterName] ?? 0;
    const after = current.kinds[kind]?.counters[counterName] ?? 0;
    total += Math.max(0, after - before);
  }
  return total / (dtMs / 1_000);
}

/**
 * Whether the persisted snapshots lag the live aggregate.
 *
 * An empty history is the "collecting" state, not staleness. Otherwise the
 * newest snapshot may age by the charts own refresh interval plus the derived
 * gap threshold (or a fixed fallback when the cadence cannot be derived)
 * before the lag note shows — a chart that simply has not refreshed yet must
 * never cry stale.
 *
 * @param snapshots - The history rows.
 * @param takenAt - The live aggregate's assembly time (RFC3339).
 * @param refreshSlackMs - The charts own refresh interval to tolerate.
 * @returns True when snapshots lag beyond refresh plus cadence slack.
 */
export function snapshotsAreStale(snapshots: ScanStatsSnapshot[], takenAt: string, refreshSlackMs: number): boolean {
  const parsed = parseSnapshots(snapshots);
  if (parsed.length === 0) {
    return false;
  }
  const threshold = gapThresholdMs(parsed.map((pair) => pair.ts));
  const limit = refreshSlackMs + (Number.isFinite(threshold) ? threshold : STALE_FALLBACK_MS);
  return Date.parse(takenAt) - parsed[parsed.length - 1].ts > limit;
}

/// The responsive row of stat tiles across the top of the page.
const TileRow = styled.div`
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
  gap: ${spacers.three};
  margin-bottom: ${spacers.four};
`;

/// One stat tile: a hero value above its muted label.
const Tile = styled.div`
  background: var(--thorium-panel-bg);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 8px;
  padding: ${spacers.three};
`;

/// The tile's hero value, in text ink (never a series color).
const TileValue = styled.div`
  font-size: 1.4rem;
  font-weight: 600;
  color: var(--thorium-text);
`;

/// The tile's muted label line.
const TileLabel = styled.div`
  font-size: 0.72rem;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--thorium-secondary-text);
`;

/// A census row for a user at or past their active quota, in warning tokens
/// (bootstrap's table-warning keeps light text on its light bg in dark mode).
const QuotaWarningRow = styled.tr`
  --bs-table-bg: var(--thorium-warning-bg);
  && td {
    color: var(--thorium-warning-text);
  }
`;

/// A muted inline note (staleness, truncation caveats).
const MutedNote = styled.div`
  font-size: 0.78rem;
  color: var(--thorium-secondary-text);
  margin-bottom: ${spacers.two};
`;

/**
 * One census hunts table: name links to details, status badge, counters, and
 * (for active hunts) the same lane-weighted progress the details page shows.
 *
 * @param lines - The bounded hunt lines to render.
 * @param showProgress - Whether to render the lane progress column.
 * @param showFailure - Whether to render the failure text column.
 * @returns The hunts table, or a muted empty note.
 */
const HuntLinesTable: React.FC<{ lines: HuntStatusLine[]; showProgress?: boolean; showFailure?: boolean }> = ({
  lines,
  showProgress = false,
  showFailure = false,
}) => {
  if (lines.length === 0) {
    return <MutedNote>None</MutedNote>;
  }
  return (
    <Table striped bordered size="sm">
      <thead>
        <tr>
          <th>Name</th>
          <th>Submitter</th>
          <th>Groups</th>
          <th>Status</th>
          <th>Scanned</th>
          <th>Matched</th>
          {showProgress && <th>Progress</th>}
          {showFailure && <th>Failure</th>}
        </tr>
      </thead>
      <tbody>
        {lines.map((line) => (
          <tr key={line.id}>
            <td>
              <Link to={`${getDetailsBasePathByEntity(Entities.Hunt)}/${line.id}`}>{line.name}</Link>
            </td>
            <td>{line.submitter}</td>
            <td>{line.groups.join(', ')}</td>
            <td>
              <Badge bg={huntStatusVariant(line.status)}>{line.status}</Badge>
            </td>
            <td>{line.counters.scanned.toLocaleString()}</td>
            <td>{line.counters.matched.toLocaleString()}</td>
            {showProgress && (
              <td style={{ minWidth: '140px' }}>
                <ProgressBar
                  className={huntBarClass(line.status)}
                  now={Math.round(overallProgress(line.lanes) * 100)}
                  label={`${Math.round(overallProgress(line.lanes) * 100)}%`}
                  animated={isActiveHuntStatus(line.status)}
                  title={line.lanes.map((lane, index) => `lane ${index}: ${Math.round(laneFraction(lane) * 100)}%`).join(' · ')}
                />
              </td>
            )}
            {showFailure && <td>{line.failure ?? ''}</td>}
          </tr>
        ))}
      </tbody>
    </Table>
  );
};

/**
 * The admin hunt status page: what is hunting now and next, who is using
 * hunts against their quotas, and how fast the system scans - live and over a
 * selected history range. API-side `is_admin!` gates are the real boundary;
 * the route wrapper's admin redirect is UX.
 *
 * @returns The /hunt-status page.
 */
const HuntStatus = () => {
  const [error, setError] = useState('');
  // history failures get their own banner: the live poll clears its banner on
  // recovery and must never wipe a history error that is still outstanding
  const [historyError, setHistoryError] = useState('');
  const [census, setCensus] = useState<HuntSystemStatus | null>(null);
  // keep the last two live readings so the rate tile derives a real delta
  const [statsPair, setStatsPair] = useState<{ previous: EventStats | null; current: EventStats | null }>({
    previous: null,
    current: null,
  });
  const [history, setHistory] = useState<ScanStatsSnapshot[] | null>(null);
  const [rangeId, setRangeId] = useState<string>('6h');
  // the range entries are stable consts so this only changes with rangeId
  const activeRange = HISTORY_RANGES.find((candidate) => candidate.id === rangeId) ?? HISTORY_RANGES[1];
  const refreshMs = historyRefreshMs(activeRange.secs);

  // poll the census and live stats together on the house 10s cadence; ticks
  // skip while a fetch is still in flight (a slow API never stacks requests)
  // and while the tab is hidden, with an immediate refresh on return
  useEffect(() => {
    let inFlight = false;
    const fetchLive = async (): Promise<void> => {
      // never stack a new tick on one that is still in flight
      if (inFlight) {
        return;
      }
      inFlight = true;
      try {
        const [nextCensus, nextStats] = await Promise.all([getHuntSystemStatus(setError), getEventStats(setError)]);
        if (nextCensus) {
          setCensus(nextCensus);
        }
        if (nextStats) {
          setStatsPair((pair) => ({ previous: pair.current, current: nextStats }));
        }
        // clear the banner once both reads recover so errors never stick
        if (nextCensus && nextStats) {
          setError('');
        }
      } finally {
        inFlight = false;
      }
    };
    void fetchLive();
    const intervalId = setInterval(() => {
      // hidden tabs skip ticks instead of polling an unwatched page
      if (document.hidden) {
        return;
      }
      void fetchLive();
    }, POLL_INTERVAL_MS);
    // refresh immediately when the tab becomes visible again, dropping the
    // pre-hide reading so the rate tile never averages the whole hidden span
    const onVisible = (): void => {
      if (!document.hidden) {
        setStatsPair((pair) => ({ previous: null, current: pair.current }));
        void fetchLive();
      }
    };
    document.addEventListener('visibilitychange', onVisible);
    return () => {
      clearInterval(intervalId);
      document.removeEventListener('visibilitychange', onVisible);
    };
  }, []);

  // fetch history when the range changes, then keep it refreshing at the
  // charts information rate: the server strides every range to ~1,000 points
  // so a new point only exists every range/1000 seconds
  useEffect(() => {
    let inFlight = false;
    let lastLoaded = 0;
    // ignore results that land after this effect was torn down so a slow
    // old-range response can never overwrite the newly selected range
    let cancelled = false;
    // drop the previous range's rows and error so the charts never show old
    // data (or a stale failure) under the newly selected range label
    setHistory(null);
    setHistoryError('');
    const load = async (): Promise<void> => {
      // never stack a refresh on one that is still in flight
      if (inFlight) {
        return;
      }
      inFlight = true;
      try {
        const end = new Date();
        const start = new Date(end.getTime() - activeRange.secs * 1_000);
        // omit step_secs so the server picks its ~1,000 point default stride
        const rows = await getEventStatsHistory(start.toISOString(), end.toISOString(), undefined, (msg) => {
          // route history failures to their own banner, never the live one
          if (!cancelled) {
            setHistoryError(msg);
          }
        });
        if (rows && !cancelled) {
          setHistory(rows);
          setHistoryError('');
          lastLoaded = Date.now();
        }
      } finally {
        inFlight = false;
      }
    };
    void load();
    const intervalId = setInterval(() => {
      // hidden tabs skip refreshes instead of re-downloading unwatched charts
      if (document.hidden) {
        return;
      }
      void load();
    }, refreshMs);
    // catch stale charts up as soon as the tab becomes visible again
    const onVisible = (): void => {
      if (!document.hidden && Date.now() - lastLoaded >= refreshMs) {
        void load();
      }
    };
    document.addEventListener('visibilitychange', onVisible);
    return () => {
      cancelled = true;
      clearInterval(intervalId);
      document.removeEventListener('visibilitychange', onVisible);
    };
    // activeRange and refreshMs both derive from rangeId alone
  }, [rangeId]);

  const stats = statsPair.current;
  const rate = liveRate(statsPair.previous, stats);

  // derive both chart series sets once per history fetch
  const rateSeries: VisxLineSeries[] = useMemo(
    () =>
      EVENT_KINDS.map((kind) => ({
        id: kind,
        label: KIND_LABELS[kind],
        color: KIND_COLORS[kind],
        points: deriveRateSeries(history ?? [], kind),
      })),
    [history],
  );
  const queueSeries: VisxLineSeries[] = useMemo(
    () =>
      EVENT_KINDS.map((kind) => ({
        id: kind,
        label: KIND_LABELS[kind],
        color: KIND_COLORS[kind],
        points: deriveQueueSeries(history ?? [], kind),
      })),
    [history],
  );

  // sum the paced hunt chunk backlog across its (today: one) kinds
  const backlogQueued = Object.values(census?.chunk_backlog ?? {}).reduce((total, depth) => total + depth.queued, 0);
  const backlogInFlight = Object.values(census?.chunk_backlog ?? {}).reduce((total, depth) => total + depth.in_flight, 0);
  // memoize the stale check so its history parse runs once per live tick
  // instead of on every render
  const takenAt = stats?.taken_at;
  const stale = useMemo(() => takenAt != null && snapshotsAreStale(history ?? [], takenAt, refreshMs), [history, takenAt, refreshMs]);

  return (
    <Page title="Hunt Status · Thorium">
      {error != '' && (
        <Row>
          <AlertBanner>{error}</AlertBanner>
        </Row>
      )}
      {historyError != '' && (
        <Row>
          <AlertBanner>{historyError}</AlertBanner>
        </Row>
      )}
      {stats != null && stats.workers === 0 && (
        <Row>
          <AlertBanner>No live event-handler workers: nothing is scanning and no snapshots are being written.</AlertBanner>
        </Row>
      )}
      <Row>
        <Col className="d-flex justify-content-center">
          <Title>Hunt Status</Title>
        </Col>
      </Row>
      <TileRow>
        <Tile>
          <TileValue>{census ? `${census.quotas.active_global} / ${census.quotas.max_active_global}` : '—'}</TileValue>
          <TileLabel>Active hunts vs global quota</TileLabel>
        </Tile>
        <Tile>
          <TileValue>{census ? backlogQueued.toLocaleString() : '—'}</TileValue>
          <TileLabel>Hunt chunks queued</TileLabel>
        </Tile>
        <Tile>
          <TileValue>{census ? backlogInFlight.toLocaleString() : '—'}</TileValue>
          <TileLabel>Hunt chunks in flight</TileLabel>
        </Tile>
        <Tile>
          <TileValue>{stats ? stats.workers.toLocaleString() : '—'}</TileValue>
          <TileLabel>Live workers</TileLabel>
        </Tile>
        <Tile>
          <TileValue>{rate !== null ? `${rate.toFixed(1)}/s` : '—'}</TileValue>
          <TileLabel>Scan rate (all kinds)</TileLabel>
        </Tile>
      </TileRow>
      <ChartBlock>
        <ChartHeader>
          <ChartTitle>Scans per second</ChartTitle>
          <KeyPicker value={rangeId} onChange={(event) => setRangeId(event.target.value)} aria-label="History range">
            {HISTORY_RANGES.map((range) => (
              <option key={range.id} value={range.id}>
                {range.label}
              </option>
            ))}
          </KeyPicker>
        </ChartHeader>
        {stale && <MutedNote>Snapshots are lagging the live view; the newest history row is older than the snapshot cadence.</MutedNote>}
        <VisxLineChart
          series={rateSeries}
          ariaLabel="Scans per second by event kind"
          valueFormatter={(value) => `${value.toFixed(2)}/s`}
          emptyText="Collecting snapshots…"
        />
      </ChartBlock>
      <ChartBlock>
        <ChartHeader>
          <ChartTitle>Queue depth</ChartTitle>
        </ChartHeader>
        <VisxLineChart series={queueSeries} ariaLabel="Queued events by event kind" emptyText="Collecting snapshots…" />
      </ChartBlock>
      {census != null && (
        <>
          <Subtitle>Ongoing hunts</Subtitle>
          <HuntLinesTable lines={census.ongoing} showProgress />
          <Subtitle>Upcoming hunts</Subtitle>
          <MutedNote>
            Created hunts waiting on their first checkpoint, plus {backlogQueued.toLocaleString()} queued paced chunks — queued paced work,
            not scheduling.
          </MutedNote>
          <HuntLinesTable lines={census.upcoming} showProgress />
          <Subtitle>Recently finished</Subtitle>
          <MutedNote>
            Terminal hunts created inside the window, newest created first — a long hunt created before the window will not appear here when
            it finishes.
          </MutedNote>
          <HuntLinesTable lines={census.recent} showFailure />
          <Subtitle>Users</Subtitle>
          {census.users.length === 0 ? (
            <MutedNote>None</MutedNote>
          ) : (
            <Table striped bordered size="sm">
              <thead>
                <tr>
                  <th>User</th>
                  <th>Active vs quota</th>
                  <th>Created in window</th>
                </tr>
              </thead>
              <tbody>
                {census.users.map((row) => {
                  // flag users at or past their active quota in warning tokens
                  const RowTag = row.active >= row.quota ? QuotaWarningRow : 'tr';
                  return (
                    <RowTag key={row.username}>
                      <td>{row.username}</td>
                      <td>
                        {row.active} / {row.quota}
                      </td>
                      <td>{row.recent_created}</td>
                    </RowTag>
                  );
                })}
              </tbody>
            </Table>
          )}
        </>
      )}
    </Page>
  );
};

export default HuntStatus;
