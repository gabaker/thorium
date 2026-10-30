import { FC, useState } from 'react';
import { Link } from 'react-router-dom';

// project imports
import { filterRunsByKind } from '@components/agentic/runs/display';
import { useRunBrowse } from '@components/agentic/runs/runBrowse';
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
import { useAuth } from '@utilities/auth';
import { isRunFinished, runKind } from '@utilities/runs';
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

/** List the tool, workflow, and pipeline runs in the user's groups. */
const RunBrowsing: FC = () => {
  const { userInfo } = useAuth();
  const userGroups = useGroupNames();
  const [group, setGroup] = useState('');
  const [tab, setTab] = useState<KindTab>('all');
  const groups = group === '' ? userGroups : [group];
  // a signed in user in no groups has no runs, so nothing is loaded for them
  const noGroups = userInfo !== null && userGroups.length === 0;
  const { runs, hasMore, error, loading, loadingMore, loadMore } = useRunBrowse(groups, userGroups.length > 0);
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
