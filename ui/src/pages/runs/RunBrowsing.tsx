import { FC, useCallback, useEffect, useRef, useState } from 'react';
import { Link } from 'react-router-dom';

// project imports
import { filterRunsByKind, mergeRuns, refreshedRows, staleUnfinished } from '@components/agentic/runs/display';
import { RunTime } from '@components/agentic/runs/RunTime';
import { StatusPill } from '@components/agentic/runs/StatusIndicator';
import {
  Actions,
  Badge,
  BannerSlot,
  EntityLink,
  Muted,
  PageHead,
  PageTitle,
  Row,
  Section,
  Select,
  Table,
} from '@components/agentic/shared';
import { useGroupNames } from '@components/agentic/hooks';
import Page from '@components/pages/Page';
import AlertBanner from '@components/shared/alerts/AlertBanner';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import { Button, ButtonVariant } from '@components/shared/buttons';
import { Tabs } from '@components/shared/tabs';
import { listRunsByStatus, listRunsByTag, RunPage } from '@thorpi/runs';
import { useAuth } from '@utilities/auth';
import { rereadRuns } from '@utilities/queries/runs';
import { isRunFinished, runKind } from '@utilities/runs';
import { Reaction, ReactionStatus } from '@models/reactions';
import { RunKind } from '@models/runs';

/** A kind filter tab: a run kind, or `all` for every kind. */
type KindTab = RunKind | 'all';

/** The kind filter tabs in the order shown, keyed by the run kind each one filters to. */
export const KIND_TABS: { key: KindTab; label: string }[] = [
  { key: 'all', label: 'All' },
  { key: RunKind.Workflow, label: 'Workflow' },
  { key: RunKind.Tool, label: 'Tool' },
  { key: RunKind.Pipeline, label: 'Pipeline' },
];

/** The tag every workflow run carries. */
const WORKFLOW_RUN_TAG = 'WorkflowRun';

/** How soon an unfinished list reloads after a change, in milliseconds. */
const REFRESH_MS = 3000;

/** The longest wait between reloads of an unfinished list, in milliseconds. */
const MAX_REFRESH_MS = 30_000;

/** How much longer each reload of an unchanged list waits than the one before. */
const REFRESH_BACKOFF = 1.5;

/** The listings read for each group: the workflow run tag, and each status. */
const SOURCES: (typeof WORKFLOW_RUN_TAG | ReactionStatus)[] = [WORKFLOW_RUN_TAG, ...Object.values(ReactionStatus)];

/** Where the next page of one group's listing starts. */
interface PageCursor {
  /** The group the listing is of */
  group: string;
  /** The listing: the workflow run tag or a status */
  source: (typeof SOURCES)[number];
  /** The cursor of the next page */
  cursor: number;
}

/**
 * Read one page of one group's listing.
 *
 * @param group - The group to list.
 * @param source - The listing to read: the workflow run tag, or a status.
 * @param cursor - The cursor of the page, or `undefined` for the first page.
 * @param onError - Called with a formatted message if the request fails.
 * @returns The page, or `null` if the request failed.
 */
function readPage(
  group: string,
  source: PageCursor['source'],
  cursor: number | undefined,
  onError: (message: string) => void,
): Promise<RunPage | null> {
  // workflow runs are found by their tag, the other kinds by status and source
  return source === WORKFLOW_RUN_TAG ? listRunsByTag(group, source, onError, cursor) : listRunsByStatus(group, source, onError, cursor);
}

/** What one sweep read: the first page of every listing and, on a refresh, the stale shown rows read again by id. */
export interface SweepRead {
  /** The first page of each listing that answered */
  lists: Reaction[][];
  /** Where the next page of each listing with more runs starts */
  next: PageCursor[];
  /** The stale shown runs read again by id */
  reread: Reaction[];
  /** The ids of stale shown runs the API no longer finds */
  gone: string[];
  /** How many listings the sweep read */
  requests: number;
  /** How many listing reads failed; only these decide whether a refresh's pages are used */
  failures: number;
  /** How many reads of stale rows by id failed for a reason other than the run being gone */
  rereadFailures: number;
}

/**
 * Read the first page of every listing of the groups and, on a refresh, read again by id the shown unfinished rows no
 * first page holds anymore, so they show their new status or leave when gone.
 *
 * @param groups - The groups to list.
 * @param shown - The rows shown before a refresh, or `null` for a full load, which reads nothing again.
 * @param onError - Called with the message of each failed read.
 * @returns What the sweep read, with the count of listing requests and of the listing and re-read failures.
 */
export async function readSweep(groups: string[], shown: Reaction[] | null, onError: (message: string) => void): Promise<SweepRead> {
  let failures = 0;
  let rereadFailures = 0;
  let requests = 0;
  // count each failed listing read before reporting it
  const failed = (message: string) => {
    failures += 1;
    onError(message);
  };
  // re-read failures are counted apart, so rows that fail to read again never make a refresh drop pages its listings read
  const rereadFailed = (message: string) => {
    rereadFailures += 1;
    onError(message);
  };
  const lists: Reaction[][] = [];
  const next: PageCursor[] = [];
  for (const name of groups) {
    for (const source of SOURCES) {
      requests += 1;
      const page = await readPage(name, source, undefined, failed);
      if (!page) continue;
      lists.push(page.runs);
      if (page.cursor !== null) next.push({ group: name, source, cursor: page.cursor });
    }
  }
  // the stale rows are read again only when some listing answered, so a refresh with the API down reads nothing more
  let reread: Reaction[] = [];
  let gone: string[] = [];
  if (shown !== null && failures < requests) {
    ({ found: reread, gone } = await rereadRuns(staleUnfinished(shown, lists), rereadFailed));
  }
  return { lists, next, reread, gone, requests, failures, rereadFailures };
}

/**
 * Tell whether a refresh's pages replace the shown rows.
 *
 * @param sweep - The refresh's listing request and failure counts.
 * @returns Whether a listing answered, or there was nothing to list; a refresh where every listing failed keeps the rows.
 */
export function usesRefresh(sweep: Pick<SweepRead, 'requests' | 'failures'>): boolean {
  return sweep.requests === 0 || sweep.failures < sweep.requests;
}

/** Tell which runs of a list are in which status, so a reload can tell whether anything moved. */
function runsSignature(runs: Reaction[]): string {
  return runs
    .map((run) => `${run.id}:${run.status}`)
    .sort()
    .join(',');
}

/** List the tool, workflow, and pipeline runs in the user's groups. */
const RunBrowsing: FC = () => {
  const { userInfo } = useAuth();
  const userGroups = useGroupNames();
  const [group, setGroup] = useState('');
  const [tab, setTab] = useState<KindTab>('all');
  const [runs, setRuns] = useState<Reaction[]>([]);
  const [more, setMore] = useState<PageCursor[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState('');
  const groups = group === '' ? userGroups : [group];
  // a signed in user in no groups has no runs, so nothing is loaded for them
  const noGroups = userInfo !== null && userGroups.length === 0;
  // the id of the newest sweep; a group change or a refresh makes older sweeps stale
  const sweep = useRef(0);
  // the id of the newest full load; a group change makes the pages being read for the old rows stale
  const epoch = useRef(0);
  // what the first pages held at the last sweep, to tell whether a refresh found any change
  const signature = useRef('');
  // the rows shown now, so a refresh can find the unfinished ones its first pages no longer hold
  const shownRuns = useRef<Reaction[]>([]);
  useEffect(() => {
    shownRuns.current = runs;
  }, [runs]);

  // read the first page of every listing; a refresh merges into the rows already shown, a full load replaces them
  const load = useCallback(
    async (refresh: boolean): Promise<boolean> => {
      const current = ++sweep.current;
      if (!refresh) {
        epoch.current += 1;
        // an error of another group's listing does not belong to this one
        setError('');
        // pages being read for the rows this load replaces are abandoned
        setLoadingMore(false);
      }
      const onError = (message: string) => {
        if (current === sweep.current) setError(message);
      };
      const { lists, next, reread, gone, requests, failures, rereadFailures } = await readSweep(
        groups,
        refresh ? shownRuns.current : null,
        onError,
      );
      // a stale sweep must not replace the rows of the newer one
      if (current !== sweep.current) return false;
      if (!refresh) {
        // a full load replaces the rows and cursors even when every request failed, so no other group's rows stay under this one
        setRuns(mergeRuns(lists));
        setMore(next);
      } else if (usesRefresh({ requests, failures })) {
        // a refresh where every request failed keeps the rows, so refreshing goes on and can restore them
        setRuns((shown) => refreshedRows(shown, lists, reread, gone));
      }
      setLoading(false);
      // a sweep that read every listing and every stale row clears an error left by an earlier failed read
      if (failures === 0 && rereadFailures === 0) setError('');
      // a refresh that read the same runs in the same statuses found nothing new
      const seen = runsSignature([...lists.flat(), ...reread]);
      const changed = seen !== signature.current;
      signature.current = seen;
      return changed;
    },
    [groups.join(',')],
  );

  // read the next page of every listing that has one
  const loadMore = async () => {
    const started = epoch.current;
    setLoadingMore(true);
    // a retry starts without the error of the read it retries
    setError('');
    const lists: Reaction[][] = [];
    const remaining: PageCursor[] = [];
    for (const entry of more) {
      const page = await readPage(entry.group, entry.source, entry.cursor, setError);
      // a failed page keeps its cursor so the next click retries it
      if (!page) {
        remaining.push(entry);
        continue;
      }
      lists.push(page.runs);
      if (page.cursor !== null) remaining.push({ ...entry, cursor: page.cursor });
    }
    // a group change while the pages were read makes them belong to rows no longer shown
    if (started !== epoch.current) return;
    setRuns((shown) => mergeRuns([shown, ...lists]));
    setMore(remaining);
    setLoadingMore(false);
  };

  useEffect(() => {
    if (userGroups.length > 0) void load(false);
  }, [load, userGroups.length]);

  // keep reloading while any run is unfinished
  const unfinished = runs.some((run) => !isRunFinished(run.status));
  useEffect(() => {
    if (!unfinished) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    // arm the next refresh only after a sweep finishes, so slow sweeps never overlap
    const next = (wait: number) => {
      timer = setTimeout(() => {
        // a hidden tab waits instead of reading, and a list that did not change waits longer next time, so a run stuck
        // unfinished does not hold the API busy
        const reload = document.hidden ? Promise.resolve(false) : load(true);
        void reload.then((changed) => {
          if (!cancelled) next(changed ? REFRESH_MS : Math.min(wait * REFRESH_BACKOFF, MAX_REFRESH_MS));
        });
      }, wait);
    };
    next(REFRESH_MS);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [unfinished, load]);

  // the listings hold more runs than the pages read so far
  const hasMore = more.length > 0;
  const filtered = tab !== 'all';
  const shown = filterRunsByKind(runs, tab === 'all' ? null : tab);

  return (
    <Page title="Runs · Thorium">
      <PageHead>
        <PageTitle>Runs</PageTitle>
        <Link to="/launch">
          <Button variant={ButtonVariant.Ok} data-testid="launch-link">
            Launch a run
          </Button>
        </Link>
      </PageHead>
      <BannerSlot>{error !== '' && <AlertBanner>{error}</AlertBanner>}</BannerSlot>
      <Tabs
        active={tab}
        onChange={setTab}
        aria-label="Run kind"
        tabs={KIND_TABS.map(({ key, label }) => ({
          key,
          label,
          count: filterRunsByKind(runs, key === 'all' ? null : key).length,
          countMore: hasMore,
        }))}
      />
      <Row style={{ margin: '1rem 0' }}>
        <Select
          data-testid="runs-group"
          aria-label="Filter runs by group"
          style={{ maxWidth: '16rem' }}
          value={group}
          onChange={(event) => setGroup(event.target.value)}
        >
          <option value="">All my groups</option>
          {userGroups.map((name) => (
            <option key={name} value={name}>
              {name}
            </option>
          ))}
        </Select>
      </Row>
      <Section>
        <LoadingSpinner loading={loading && !noGroups} />
        {/* the notices sit in their own block so they don't run on from anything else in the card */}
        {(!loading || noGroups) && shown.length === 0 && (
          <Muted as="div" data-testid="no-runs">
            {/* on a kind tab the runs not read yet might match, so the notice speaks only of the runs read so far */}
            {filtered && runs.length > 0
              ? hasMore
                ? `None of the ${runs.length} runs read so far are ${tab} runs.`
                : `No ${tab} runs found.`
              : 'No runs found.'}
          </Muted>
        )}
        {shown.length > 0 && (
          <Table data-testid="runs-table">
            <thead>
              <tr>
                <th>Run</th>
                <th>Kind</th>
                <th>Status</th>
                <th>Group</th>
                <th>Creator</th>
                <th>SLA expires</th>
              </tr>
            </thead>
            <tbody>
              {shown.map((run) => (
                <tr key={run.id} data-testid="run-row">
                  <td>
                    <EntityLink to={`/runs/${run.group}/${run.id}`}>{run.pipeline}</EntityLink>
                  </td>
                  <td>
                    <Badge>{runKind(run)}</Badge>
                  </td>
                  <td>
                    <StatusPill status={run.status} />
                  </td>
                  <td>{run.group}</td>
                  <td>{run.creator}</td>
                  <td>
                    <RunTime value={run.sla} overdueWhenPast={!isRunFinished(run.status)} />
                  </td>
                </tr>
              ))}
            </tbody>
          </Table>
        )}
        {hasMore && (
          <Actions style={{ marginTop: '0.75rem' }}>
            <Muted data-testid="runs-more-note">
              {filtered
                ? `More runs exist than the ${runs.length} read so far; ${shown.length} of them ${shown.length === 1 ? 'is a' : 'are'} ${tab} run${shown.length === 1 ? '' : 's'}.`
                : `More runs exist than the ${runs.length} shown.`}
            </Muted>
            <Button variant={ButtonVariant.Secondary} data-testid="runs-load-more" disabled={loadingMore} onClick={() => void loadMore()}>
              {loadingMore ? 'Loading...' : 'Load more runs'}
            </Button>
          </Actions>
        )}
      </Section>
    </Page>
  );
};

export default RunBrowsing;
