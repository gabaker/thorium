import { FC, useEffect, useState } from 'react';
import styled from 'styled-components';

// project imports
import { Actions, EntityLink, Mono, Muted, Pre, Section } from '@components/agentic/shared';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Button, ButtonSize, ButtonVariant } from '@components/shared/buttons';
import { getResultsFile } from '@thorpi/results';
import { getRepoRunResults, getRunStatusUpdates, getSampleRunResults } from '@thorpi/runs';
import { downloadBlob } from '@utilities/download';
import { Reaction, ReactionStatus } from '@models/reactions';
import { RunOutput, RunResults } from '@models/runs';
import { isRunFinished, noResultsMessage, resultsDuringRun, runResultKey, runResultsConfirmed, runWindow } from '@utilities/runs';
import { formatImageVersion } from '@utilities/version';
import { formatTimestamp } from './display';
import { NoticeBanner } from './LaunchNotices';

/** The results of the run on one input, keyed by the input's SHA256 or repo URL. */
export interface InputResults {
  /** The SHA256 or repo URL */
  input: string;
  /** Whether the input is a repo */
  repo: boolean;
  /** The results read, or `null` when none were found or the read failed */
  results: RunResults | null;
  /** Why reading the input's results failed, or empty when the read succeeded */
  error: string;
  /** How many results under the run's key were left out because other runs stored them */
  others: number;
  /** Whether a key holds more than one result from the run's lifetime, so another run that overlapped it may have stored some */
  overlapped: boolean;
}

/** Props for {@link ResultOutput}. */
interface ResultOutputProps {
  /** The sample the result is on, when it is on a sample */
  sha256: string | null;
  /** The tool or result key the result is stored under */
  tool: string;
  /** The result */
  output: RunOutput;
}

/** One input's results; blocks are spaced from the next input's but the last leaves no margin to add to the card's padding. */
const InputBlock = styled.div`
  &:not(:last-child) {
    margin-bottom: 1rem;
  }
`;

/** One result of an input, spaced from the next result or note below it. */
const ResultBlock = styled.div`
  margin-bottom: 0.75rem;
  /* the last result of the card leaves no margin to add to the card's padding */
  &:last-child {
    margin-bottom: 0;
  }
`;

/** The banner of a failed download, kept clear of the buttons above it. */
const DownloadError = styled(NoticeBanner)`
  margin: 0.5rem 0 0;
`;

/** One result with its JSON and downloadable files. */
const ResultOutput: FC<ResultOutputProps> = ({ sha256, tool, output }) => {
  const [error, setError] = useState('');
  // the API leaves the file list out when a result has no files
  const files = output.files ?? [];
  const download = async (name: string) => {
    if (!sha256) return;
    // clear the error of an earlier attempt so a retry that succeeds leaves no stale banner
    setError('');
    const res = await getResultsFile(sha256, tool, output.id, name, setError);
    if (res) downloadBlob(res.data, name.split('/').pop() ?? name);
  };
  return (
    <ResultBlock>
      {/* a result key is shared by every run of the tool, so each result says when and by which version it was stored */}
      <Muted data-testid="run-result-meta">
        {output.uploaded ? `Stored ${formatTimestamp(output.uploaded)}` : 'Stored at an unknown time'}
        {formatImageVersion(output.tool_version ?? undefined) !== '' &&
          ` by version ${formatImageVersion(output.tool_version ?? undefined)}`}
      </Muted>
      <Pre data-testid="run-result-json">{JSON.stringify(output.result, null, 2)}</Pre>
      {files.length > 0 && (
        <Actions data-testid="run-result-files" style={{ marginTop: '0.5rem' }}>
          {files.map((name) => (
            <Button
              key={name}
              size={ButtonSize.Small}
              variant={ButtonVariant.Secondary}
              disabled={!sha256}
              onClick={() => void download(name)}
            >
              Download {name}
            </Button>
          ))}
        </Actions>
      )}
      {error !== '' && (
        <DownloadError severity={Severity.Error}>
          <span data-testid="run-result-download-error">{error}</span>
        </DownloadError>
      )}
    </ResultBlock>
  );
};

/** Props for {@link RunInputResults}. */
interface RunInputResultsProps {
  /** The results read on the input */
  entry: InputResults;
  /** The run's status */
  status: ReactionStatus;
  /** Whether the run can be trusted to have stored the results found in its lifetime */
  confirmed: boolean;
  /** The file name the input sample was submitted under, when it has one */
  name?: string;
}

/** The results a run stored on one input, with notes on what was left out or may belong to another run. */
export const RunInputResults: FC<RunInputResultsProps> = ({ entry, status, confirmed, name }) => {
  const outputs = Object.entries(entry.results?.results ?? {}).filter(([, list]) => list.length > 0);
  const failed = entry.error !== '';
  return (
    // each input's results are their own group, spaced from the next input's
    <InputBlock data-testid="run-input-results">
      <p>
        {entry.repo ? (
          <Mono>{entry.input}</Mono>
        ) : (
          <EntityLink to={`/file/${entry.input}`}>
            <Mono>{entry.input}</Mono>
          </EntityLink>
        )}
        {name && <Muted data-testid="run-result-input-name"> ({name})</Muted>}
      </p>
      {/* a failed read says nothing about whether a result exists, so it shows only the error */}
      {failed && (
        <AlertBanner severity={Severity.Error}>
          <span data-testid="run-results-error">{entry.error}</span>
        </AlertBanner>
      )}
      {!failed && outputs.length === 0 && (
        // each note is its own block so the notes do not run together on one line
        <Muted as="div" data-testid="run-no-results">
          {noResultsMessage(status)}
        </Muted>
      )}
      {!confirmed && outputs.length > 0 && (
        <Muted as="div" data-testid="run-unconfirmed-results">
          {status === ReactionStatus.Failed ? 'This run failed' : 'No job of this run reported completing'}, so it may not have stored the{' '}
          {outputs.length === 1 && outputs[0][1].length === 1 ? 'result' : 'results'} below: another run on this input that was alive at the
          same time may have. Compare the stored times and versions.
        </Muted>
      )}
      {outputs.flatMap(([tool, list]) =>
        list.map((output) => <ResultOutput key={output.id} sha256={entry.repo ? null : entry.input} tool={tool} output={output} />),
      )}
      {entry.overlapped && (
        <Muted data-testid="run-overlapping-results">
          More than one result was stored under this key while this run was alive, so another run on this input may have stored some of
          them. Compare the stored times and versions.
        </Muted>
      )}
      {entry.others > 0 && (
        <Muted as="div" data-testid="run-other-results">
          {entry.others} {entry.others === 1 ? 'result' : 'results'} stored under this key by other runs {entry.others === 1 ? 'is' : 'are'}{' '}
          not shown.
        </Muted>
      )}
    </InputBlock>
  );
};

/** What reading a run's results found. */
export interface FoundResults {
  /** The results on each input, empty when the run's status updates could not be read */
  inputs: InputResults[];
  /** Whether the run can be trusted to have stored the results found in its lifetime */
  confirmed: boolean;
  /** Why the run's status updates could not be read, or empty when they were read */
  error: string;
}

/**
 * Read the results a finished run stored on each of its inputs.
 *
 * The results under a key come from every run of the tool, so the run's lifetime from its status
 * updates picks out its own. When the updates cannot be read the lifetime is unknown, so no
 * results are read and the error is returned instead of unfiltered results.
 *
 * @param run - The finished run.
 * @param key - The result key the run stores its results under.
 * @returns The results per input, whether they are confirmed, and any read error.
 */
export async function loadRunResults(run: Reaction, key: string): Promise<FoundResults> {
  // read the run's status updates, keeping the error so a failed read is not taken for a run with no updates
  let updatesError = '';
  const updates = await getRunStatusUpdates(run.group, run.id, (message) => {
    updatesError = message;
  });
  // without the updates the run's lifetime is unknown, so report the failure rather than guess
  if (updatesError !== '') return { inputs: [], confirmed: false, error: updatesError };
  const lifetime = runWindow(updates);
  // each input keeps its own read error so a failed read is never shown as a missing result
  const readInput = async (input: string, repo: boolean) => {
    let error = '';
    const onError = (message: string) => {
      error = message;
    };
    const results = repo ? await getRepoRunResults(input, key, onError) : await getSampleRunResults(input, key, onError);
    return { input, repo, results, error };
  };
  const inputs = [...run.samples.map((sha) => readInput(sha, false)), ...run.repos.map((repo) => readInput(repo.url, true))];
  const entries = (await Promise.all(inputs)).map(({ results, ...entry }): InputResults => {
    const all = Object.entries(results?.results ?? {});
    const own = all.map(([tool, list]) => [tool, resultsDuringRun(list, lifetime)] as const);
    const total = all.reduce((sum, [, list]) => sum + list.length, 0);
    const kept = own.reduce((sum, [, list]) => sum + list.length, 0);
    // a run stores at most one result per key on an input, so more than one in its lifetime means runs overlapped
    const overlapped = own.some(([, list]) => list.length > 1);
    return { ...entry, results: results && { ...results, results: Object.fromEntries(own) }, others: total - kept, overlapped };
  });
  return { inputs: entries, confirmed: runResultsConfirmed(run.status, updates), error: '' };
}

/** Props for {@link RunResultsPanel}. */
interface RunResultsPanelProps {
  /** The run */
  run: Reaction;
  /** The file names the input samples were submitted under, keyed by SHA256 */
  names?: Record<string, string>;
}

/** The results a finished run stored on each of its input files and repos. */
export const RunResultsPanel: FC<RunResultsPanelProps> = ({ run, names = {} }) => {
  const [found, setFound] = useState<FoundResults | null>(null);
  const key = runResultKey(run);
  const finished = isRunFinished(run.status);
  useEffect(() => {
    if (!finished || !key) return;
    let cancelled = false;
    void loadRunResults(run, key).then((all) => {
      if (!cancelled) setFound(all);
    });
    return () => {
      cancelled = true;
    };
  }, [finished, key, run.id]);
  return (
    <Section data-testid="run-results">
      <h2>Results</h2>
      {key === null && <Muted>A pipeline reaction stores its results stage by stage; open it in the single reaction status page.</Muted>}
      {key !== null && !finished && <Muted>Results appear when the run finishes.</Muted>}
      {key !== null && finished && found === null && <Muted>Loading results...</Muted>}
      {/* a failed read of the run's updates leaves its lifetime unknown, so only the error is shown */}
      {found && found.error !== '' && (
        <AlertBanner severity={Severity.Error}>
          <span data-testid="run-results-updates-error">{found.error}</span>
        </AlertBanner>
      )}
      {found?.inputs.map((entry) => (
        <RunInputResults
          key={entry.input}
          entry={entry}
          status={run.status}
          confirmed={found.confirmed}
          name={entry.repo ? undefined : names[entry.input]}
        />
      ))}
    </Section>
  );
};
