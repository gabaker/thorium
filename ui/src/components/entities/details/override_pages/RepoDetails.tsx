import React, { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from 'react';
import { useParams } from 'react-router';
import { FaServer } from 'react-icons/fa';
import styled from 'styled-components';

// project imports
const AssociationGraph = React.lazy(() => import('@components/associations/graph/AssociationGraph'));
import { GraphDataProvider } from '@components/associations/data/GraphDataContext';
import Page from '@components/pages/Page';
import Results from '@components/pages/files/Results';
import { EntityNotFound } from '@components/agentic/EntityNotFound';
import {
  BuildDashboardButton,
  BuildDashboardResource,
  Button,
  ButtonSize,
  ButtonToolbar,
  ButtonVariant,
  LinkButton,
} from '@components/shared/buttons';
import { launchRunPath } from '@components/agentic/runs/launchForms';
import Subtitle from '@components/shared/titles/Subtitle';
import Title from '@components/shared/titles/Title';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import { Panel } from '@components/shared/Panel';
import Time from '@components/shared/Time';
import CondensedEntityTags from '@components/tags/condensed/CondensedEntityTags';
import { getRepo, listCommitishDetails } from '@thorpi/repos';
import { shortHash, splitCommitishes, type SplitCommitishes } from '@utilities/repos';
import { Entities } from '@models/entities';
import type { CommitishDetails, Repo } from '@models/repos';
import type { Output } from '@models/results';

interface RepoDetailsContextType {
  // full URL of the repo the page is displaying
  repo: string | undefined;
}

// context carrying the current repo's URL to the page's subcomponents
const RepoContext = createContext<RepoDetailsContextType | undefined>(undefined);

// access the repo context; throws if used outside a RepoContext provider
const useRepoContext = () => {
  const context = useContext(RepoContext);
  if (context === undefined) {
    throw new Error('useRepoContext must be used within a RepoContextProvider');
  }
  return context;
};

// the tag keys the results tools and workflows write to a repo; the Results panel below shows them, so the tag list leaves them out
const RESULTS_TAG_KEYS = ['Results'];

// the repo icon and name, centered side by side
const IconTitle = styled.div`
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 8px;
  color: var(--thorium-text);
`;

// the page header: the repo's icon and URL
const RepoHeader = () => {
  const { repo } = useRepoContext();
  return (
    <Panel>
      <Panel.Body>
        <IconTitle>
          <FaServer size="72" className="icon" />
          <Title className="title">{repo}</Title>
        </IconTitle>
      </Panel.Body>
    </Panel>
  );
};

// a compact table of a repo's branches, tags, or commits
const RefTable = styled.table`
  width: 100%;
  color: var(--thorium-text);

  /* the branch, tag and commit tables share one column grid (see RefColumns) so they line up */
  &.fixed-grid {
    table-layout: fixed;
  }

  th,
  td {
    padding: 4px 12px;
    text-align: left;
    word-break: break-all;
  }
`;

// the facts table: labels in the small uppercase subtitle style, in a narrow column so each value sits next to its label
const FactsTable = styled(RefTable)`
  th {
    width: 1%;
    white-space: nowrap;
    word-break: normal;
    text-transform: uppercase;
    font-size: 0.87rem;
    font-weight: 400;
  }
`;

// the column widths shared by the branch, git tag and commit tables
const RefColumns = () => (
  <colgroup>
    <col style={{ width: '28%' }} />
    <col style={{ width: '22%' }} />
    <col style={{ width: '25%' }} />
    <col style={{ width: '25%' }} />
  </colgroup>
);

// facts about the repo: provider, owner, submissions, and when it was first seen
const RepoFacts = ({ repoInfo }: { repoInfo: Repo }) => (
  <Panel className="mt-4">
    <Panel.Body>
      <Subtitle className="text-center">Details</Subtitle>
      <FactsTable data-testid="repo-facts">
        <tbody>
          <tr>
            <th>Provider</th>
            <td>{repoInfo.provider}</td>
          </tr>
          <tr>
            <th>Owner</th>
            <td>{repoInfo.user}</td>
          </tr>
          <tr>
            <th>Submissions</th>
            <td>{repoInfo.submissions.length}</td>
          </tr>
          {repoInfo.earliest && (
            <tr>
              <th>Earliest commit</th>
              <td>
                <Time>{repoInfo.earliest}</Time>
              </td>
            </tr>
          )}
        </tbody>
      </FactsTable>
      <Subtitle className="text-center mt-3">Tags</Subtitle>
      <CondensedEntityTags tags={repoInfo.tags ?? {}} resource={Entities.Repo} excludeKeys={RESULTS_TAG_KEYS} fullWidthEmpty />
    </Panel.Body>
  </Panel>
);

// props for the branches, tags and commits panel
interface RepoRefsProps {
  // the commitishes read so far, split by kind
  refs: SplitCommitishes;
  // whether a page of commitishes is being read
  loading: boolean;
  // the message for the last failed read of commitishes; empty when there is none
  error: string;
  // whether the API has more commitishes than have been read
  hasMore: boolean;
  // read the next page of commitishes
  onLoadMore: () => void;
}

// the repo's branches, git tags, and latest commits, with the state of reading them
const RepoRefs = ({ refs, loading, error, hasMore, onLoadMore }: RepoRefsProps) => (
  <Panel className="mt-4">
    <Panel.Body>
      <Subtitle className="text-center">Branches, Tags and Commits</Subtitle>
      {loading && <LoadingSpinner loading={true} />}
      {error && <AlertBanner>{error}</AlertBanner>}
      {!loading && !error && refs.branches.length + refs.tags.length + refs.commits.length === 0 && (
        <AlertBanner severity={Severity.Info}>No branches, tags, or commits found.</AlertBanner>
      )}
      {refs.branches.length > 0 && (
        <RefTable data-testid="repo-branches" className="fixed-grid">
          <RefColumns />
          <thead>
            <tr>
              <th>Branch</th>
              <th>Commit</th>
              <th colSpan={2}>Updated</th>
            </tr>
          </thead>
          <tbody>
            {refs.branches.map((branch) => (
              <tr key={branch.name}>
                <td>{branch.name}</td>
                <td>{shortHash(branch.commit)}</td>
                <td colSpan={2}>
                  <Time>{branch.timestamp}</Time>
                </td>
              </tr>
            ))}
          </tbody>
        </RefTable>
      )}
      {refs.tags.length > 0 && (
        <RefTable data-testid="repo-git-tags" className="mt-3 fixed-grid">
          <RefColumns />
          <thead>
            <tr>
              <th>Git tag</th>
              <th>Commit</th>
              <th colSpan={2}>Updated</th>
            </tr>
          </thead>
          <tbody>
            {refs.tags.map((tag) => (
              <tr key={tag.name}>
                <td>{tag.name}</td>
                <td>{shortHash(tag.commit)}</td>
                <td colSpan={2}>
                  <Time>{tag.timestamp}</Time>
                </td>
              </tr>
            ))}
          </tbody>
        </RefTable>
      )}
      {refs.commits.length > 0 && (
        <RefTable data-testid="repo-commits" className="mt-3 fixed-grid">
          <RefColumns />
          <thead>
            <tr>
              <th>Commit</th>
              <th>Topic</th>
              <th>Author</th>
              <th>Added</th>
            </tr>
          </thead>
          <tbody>
            {refs.commits.map((commit) => (
              <tr key={commit.hash}>
                <td>{shortHash(commit.hash)}</td>
                <td>{commit.topic ?? ''}</td>
                <td>{commit.author}</td>
                <td>
                  <Time>{commit.timestamp}</Time>
                </td>
              </tr>
            ))}
          </tbody>
        </RefTable>
      )}
      {hasMore && (
        <ButtonToolbar className="mt-3 justify-content-center">
          <Button variant={ButtonVariant.Secondary} data-testid="repo-refs-more" disabled={loading} onClick={onLoadMore}>
            Load more
          </Button>
        </ButtonToolbar>
      )}
    </Panel.Body>
  </Panel>
);

// the results tools and workflows stored on the repo, shown the way a file's results are
const RepoResults = ({ repo }: { repo: string }) => {
  const [results, setResults] = useState<Record<string, Output[]>>({});
  const [numResults, setNumResults] = useState(0);
  return (
    <Panel className="mt-4" data-testid="repo-results">
      <Panel.Body>
        <Subtitle className="text-center">Results</Subtitle>
        <Results repo sha256={repo} results={results} setResults={setResults} numResults={numResults} setNumResults={setNumResults} />
      </Panel.Body>
    </Panel>
  );
};

const RepoDetails = () => {
  const { '*': repo } = useParams<{ '*': string }>();
  const seed = useMemo(() => ({ repos: [repo ? repo : ''] }), [repo]);
  const [loading, setLoading] = useState(true);
  const [repoError, setRepoError] = useState('');
  const [repoInfo, setRepoInfo] = useState<Repo | null>(null);
  const [commitishes, setCommitishes] = useState<CommitishDetails[]>([]);
  const [refsCursor, setRefsCursor] = useState<string | null>(null);
  const [refsLoading, setRefsLoading] = useState(false);
  const [refsError, setRefsError] = useState('');
  // identifies the repo being shown, so a read that finishes after the repo changed is dropped
  const generation = useRef(0);

  // read one page of the repo's commitishes, starting a new list when no cursor is given and extending the list otherwise
  const loadRefs = useCallback(
    async (cursor: string | null, gen: number) => {
      setRefsLoading(true);
      setRefsError('');
      let error = '';
      const page = await listCommitishDetails(repo ?? '', (message) => (error = message), 100, cursor);
      if (generation.current !== gen) return;
      if (page) {
        setCommitishes((prev) => (cursor ? [...prev, ...page.data] : page.data));
        setRefsCursor(page.cursor ?? null);
      } else {
        // the refs are secondary: a failure to read them leaves the rest of the page in place
        setRefsError(error || 'Failed to list branches, tags, and commits');
      }
      setRefsLoading(false);
    },
    [repo],
  );

  // fetch the repo and its branches, tags, and commits
  useEffect(() => {
    generation.current += 1;
    const gen = generation.current;
    const fetchRepo = async () => {
      setLoading(true);
      setRepoError('');
      setRepoInfo(null);
      setCommitishes([]);
      setRefsCursor(null);
      setRefsError('');
      let error = '';
      const info = await getRepo(repo ?? '', (message) => (error = message));
      if (generation.current !== gen) return;
      if (!info) {
        // a repo that does not exist and one the user can't see answer alike, so say only that
        setRepoError(error);
        setLoading(false);
        return;
      }
      setRepoInfo(info);
      setLoading(false);
      await loadRefs(null, gen);
    };
    void fetchRepo();
    return () => {
      generation.current += 1;
    };
  }, [repo, loadRefs]);

  // the branches, tags, and commits read so far
  const refs = useMemo(() => splitCommitishes(commitishes), [commitishes]);

  // a repo that can't be read (it doesn't exist or the user can't see it) shows only a titled message with a way back
  if (!repoInfo) {
    return <EntityNotFound what="Repo" loading={loading} listPath="/repos" listName="repos" error={repoError} />;
  }

  return (
    <RepoContext.Provider value={{ repo }}>
      <Page className="full-min-width" title={`Repo · ${repo}`}>
        <RepoHeader />
        <ButtonToolbar className="mt-3">
          <BuildDashboardButton resource={BuildDashboardResource.Repo} id={repo ?? ''} label="repo" />
          <LinkButton
            variant={ButtonVariant.Ok}
            size={ButtonSize.Small}
            to={launchRunPath({ repo: repo ?? '' })}
            data-testid="repo-run-analysis"
          >
            Run Analysis
          </LinkButton>
        </ButtonToolbar>
        <RepoFacts repoInfo={repoInfo} />
        <RepoRefs
          refs={refs}
          loading={refsLoading}
          error={refsError}
          hasMore={Boolean(refsCursor)}
          onLoadMore={() => void loadRefs(refsCursor, generation.current)}
        />
        <RepoResults repo={repo ?? ''} />
        <GraphDataProvider initial={seed}>
          <Panel className="mt-4">
            <Panel.Body>
              <Subtitle className="text-center">Associations</Subtitle>
              <AssociationGraph inView bordered={false} />
            </Panel.Body>
          </Panel>
        </GraphDataProvider>
      </Page>
    </RepoContext.Provider>
  );
};

export default RepoDetails;
