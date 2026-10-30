import { useEffect, useRef, useState } from 'react';
import { useParams } from 'react-router-dom';
import { FaAngleDown, FaAngleDoubleDown, FaAngleDoubleUp, FaAngleUp } from 'react-icons/fa';
import styled from 'styled-components';

// project imports
import Page from '@components/pages/Page';
import { Button, ButtonSize, ButtonVariant, IconButton } from '@components/shared/buttons';
import { Actions, EntityLink, Muted, PageHead, PageTitle, Row } from '@components/agentic/shared';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { OverlayTipBottom } from '@components/shared/overlay/tips';
import { useAuth } from '@utilities/auth';
import { getReaction, getReactionStageLogs } from '@thorpi/reactions';
import { getPipeline } from '@thorpi/pipelines';
import { isRunFinished, isUnknownStage, noStageLogsMessage, plannedStageNames, runKind } from '@utilities/runs';
import { RunKind } from '@models/runs';
import { ReactionStatus } from '@models/reactions';
import { AnsiText } from './AnsiText';

/** The empty-logs message and the links to the stages that have logs, kept apart from each other and the page below. */
const EmptyLogs = styled.div`
  display: flex;
  flex-direction: column;
  gap: 1rem;
  margin-bottom: 1rem;
`;

/** The log viewer's panel: the paging bar above the scrolling log lines. */
const LogBox = styled.section`
  display: flex;
  flex-direction: column;
  max-height: 90vh;
  background-color: var(--thorium-panel-bg);
  color: var(--thorium-text);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 6px;
`;

/** The paging buttons, centred above the log lines. */
const LogNav = styled.div`
  display: flex;
  justify-content: center;
  gap: 0.25rem;
  padding: 0.5rem;
  border-bottom: 1px solid var(--thorium-panel-border);
`;

/** The log lines, scrolling inside the panel. */
const LogLines = styled.div`
  overflow-y: scroll;
  padding: 1rem;
`;

/** Shows the logs of one stage of a reaction, paging through them and following new lines. */
const StageLogsView = () => {
  const { reactionID, group, stage } = useParams<{ reactionID: string; group: string; stage: string }>();
  const [cursor, setCursor] = useState(0);
  const [startLogLine, setStartLogLine] = useState(0);
  const [endLogLine, setEndLogLine] = useState(0);
  const [tailLogs, setTailLogs] = useState(true);
  const [pagingDown, setPagingDown] = useState(false);
  const [reactionStageLogs, setReactionStageLogs] = useState<string[]>([]);
  const [logUpdateTimeout, setLogUpdateTimeout] = useState(10);
  // the message for the last failed read of the logs; empty when the last read worked
  const [logError, setLogError] = useState('');
  // whether a read of the logs has finished, so an empty page can tell "not read yet" from "no logs"
  const [logsRead, setLogsRead] = useState(false);
  // the reaction's status and stage names, read when a page has no logs so the message can say why; null until read or when unreadable
  const [reactionStatus, setReactionStatus] = useState<ReactionStatus | null>(null);
  const [reactionStages, setReactionStages] = useState<string[] | null>(null);
  const { checkCookie } = useAuth();
  const maxRenderedLines = 1000;
  const limit = 1000;

  // read the reaction this stage belongs to, for its status and the names of its stages
  const readReaction = async () => {
    const reaction = await getReaction(group!, reactionID!, () => {
      void checkCookie();
    });
    if (!reaction) return;
    setReactionStatus(reaction.status ?? null);
    if (runKind(reaction) !== RunKind.Pipeline) {
      // a tool or workflow run has no pipeline, so its stages come from the plan it recorded
      setReactionStages(plannedStageNames(reaction));
    } else if (reaction.pipeline) {
      const pipeline = await getPipeline(group!, reaction.pipeline, () => {
        void checkCookie();
      });
      if (pipeline?.order) setReactionStages(pipeline.order.flat());
    }
  };

  // fetch a page of logs starting at the cursor
  const getLogPage = async () => {
    let failure = '';
    const stageLogs = await getReactionStageLogs(
      group!,
      reactionID!,
      stage!,
      (message) => {
        // keep the message to show on the page and check whether the session is still valid
        failure = message;
        void checkCookie();
      },
      cursor as unknown as null,
      limit,
    );
    setLogsRead(true);

    if (!stageLogs) {
      setLogError(failure || 'The logs for this stage could not be read');
      setLogUpdateTimeout(10000);
      return;
    }
    setLogError('');

    if (stageLogs.length == 0) {
      setTailLogs(true);
      setLogUpdateTimeout(10000);
      // an empty first page is explained by what the reaction is and has, which changes while it runs
      if (reactionStageLogs.length === 0 && !reactionEnded) void readReaction();
      return;
    } else {
      setLogUpdateTimeout(100);
    }

    let logs: string[] = [];
    if (startLogLine > cursor && startLogLine - cursor > endLogLine - startLogLine) {
      setStartLogLine(cursor);
      setEndLogLine(cursor + stageLogs.length);
      logs = stageLogs;
    } else if (startLogLine > cursor) {
      const endLogBufferIndex = reactionStageLogs.length - (startLogLine - cursor);
      logs = stageLogs.concat(reactionStageLogs.slice(0, endLogBufferIndex));
      setStartLogLine(cursor);
      setEndLogLine(cursor + logs.length);
    } else {
      let logStart = startLogLine;
      if (endLogLine + stageLogs.length - startLogLine > maxRenderedLines) {
        logStart = endLogLine + stageLogs.length - maxRenderedLines;
      }
      let startLogBufferIndex = 0;
      if (logStart > 0) {
        startLogBufferIndex = stageLogs.length;
      }
      logs = reactionStageLogs.slice(startLogBufferIndex).concat(stageLogs);
      setStartLogLine(logStart);
      setEndLogLine(endLogLine + stageLogs.length);
    }
    setReactionStageLogs(logs);
  };

  // whether the reaction is known to have ended, so a page with no logs will never get any
  const reactionEnded = reactionStatus !== null && isRunFinished(reactionStatus);

  const pageToEndAndFollow = () => {
    setTailLogs(true);
    setPagingDown(true);
    setCursor(endLogLine);
  };

  const pageDown = () => {
    setTailLogs(false);
    setPagingDown(true);
    setCursor(endLogLine);
  };

  const pageToTop = () => {
    setTailLogs(false);
    setPagingDown(false);
    setCursor(0);
  };

  const pageUp = () => {
    setTailLogs(false);
    setPagingDown(false);
    setCursor(startLogLine - limit < 0 ? 0 : startLogLine - limit);
  };

  // get first page on mount
  useEffect(() => {
    void getLogPage();
  }, []);

  // auto-refresh logs when tailing
  useEffect(() => {
    const intervalId = setInterval(() => {
      // a failed read stops the tail until the user retries, so a denied or missing stage isn't polled forever
      // a finished reaction with no logs writes none later, so that tail stops too
      if (tailLogs && !logError && !(reactionEnded && reactionStageLogs.length === 0)) {
        if (endLogLine != cursor) {
          setCursor(endLogLine);
        } else {
          void getLogPage();
        }
      }
    }, logUpdateTimeout);
    return () => clearInterval(intervalId);
  }, [tailLogs, endLogLine, logUpdateTimeout, logError, reactionEnded, reactionStageLogs]);

  // fetch page when cursor changes
  useEffect(() => {
    if (cursor < startLogLine || cursor >= endLogLine) {
      void getLogPage();
    }
  }, [cursor]);

  // scroll to correct end when logs update
  const startLogsRef = useRef<HTMLDivElement>(null);
  const endLogsRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (pagingDown) {
      endLogsRef.current?.scrollIntoView({ block: 'end', behavior: 'auto' });
    } else {
      startLogsRef.current?.scrollIntoView({ block: 'start', behavior: 'auto' });
    }
  }, [reactionStageLogs, pagingDown]);

  // a stage the reaction does not have can never log, so there is no viewer to page through
  const unknownStage =
    !logError && logsRead && reactionStageLogs.length === 0 && stage !== undefined && isUnknownStage(stage, reactionStages);

  return (
    <Page title={`Logs · ${stage} · Thorium`}>
      <PageHead data-testid="stage-logs-head">
        <PageTitle>Stage logs: {stage}</PageTitle>
        <Actions>
          <EntityLink to={`/reaction/${group}/${reactionID}`}>Back to reaction</EntityLink>
          <EntityLink to={`/runs/${group}/${reactionID}`}>Open in run view</EntityLink>
        </Actions>
      </PageHead>
      {logError && (
        <div data-testid="stage-logs-error">
          <AlertBanner>{logError}</AlertBanner>
          <Actions style={{ marginTop: '0.75rem' }}>
            <Button
              variant={ButtonVariant.Secondary}
              size={ButtonSize.Small}
              data-testid="stage-logs-retry"
              onClick={() => void getLogPage()}
            >
              Retry
            </Button>
          </Actions>
        </div>
      )}
      {!logError && logsRead && reactionStageLogs.length === 0 && (
        <EmptyLogs data-testid="stage-logs-empty">
          <AlertBanner severity={Severity.Info}>{noStageLogsMessage(stage!, reactionStatus, reactionStages)}</AlertBanner>
          {unknownStage && (
            <Row data-testid="stage-logs-stage-links">
              <Muted>Open logs for:</Muted>
              {reactionStages!.map((name) => (
                <EntityLink key={name} to={`/reaction/logs/${group}/${reactionID}/${encodeURIComponent(name)}`}>
                  {name}
                </EntityLink>
              ))}
            </Row>
          )}
        </EmptyLogs>
      )}
      {/* a read that failed or a stage the reaction lacks leaves nothing to page through, so the viewer stays hidden */}
      {!(logError && reactionStageLogs.length === 0) && !unknownStage && (
        <LogBox data-testid="stage-logs-box">
          <LogNav>
            <OverlayTipBottom tip={'Scroll to start of logs'}>
              <IconButton
                data-testid="stage-logs-nav-button"
                variant={ButtonVariant.Primary}
                aria-label="Scroll to start of logs"
                onClick={pageToTop}
              >
                <FaAngleDoubleUp size={18} />
              </IconButton>
            </OverlayTipBottom>
            <OverlayTipBottom tip={'Scroll up in logs'}>
              <IconButton
                data-testid="stage-logs-nav-button"
                variant={ButtonVariant.Primary}
                aria-label="Scroll up in logs"
                onClick={pageUp}
              >
                <FaAngleUp size={18} />
              </IconButton>
            </OverlayTipBottom>
            <OverlayTipBottom tip={'Scroll down in logs'}>
              <IconButton
                data-testid="stage-logs-nav-button"
                variant={ButtonVariant.Primary}
                aria-label="Scroll down in logs"
                onClick={pageDown}
              >
                <FaAngleDown size={18} />
              </IconButton>
            </OverlayTipBottom>
            <OverlayTipBottom tip={'Scroll to end of logs and follow'}>
              <IconButton
                data-testid="stage-logs-nav-button"
                variant={ButtonVariant.Primary}
                aria-label="Scroll to end of logs and follow"
                onClick={pageToEndAndFollow}
              >
                <FaAngleDoubleDown size={18} />
              </IconButton>
            </OverlayTipBottom>
          </LogNav>
          <LogLines>
            {reactionStageLogs &&
              reactionStageLogs.map((line, idx) => (
                <div key={startLogLine + idx} className="raw-log-line" ref={endLogsRef}>
                  <i className="log-line-index secondary-text">{startLogLine + idx}&emsp;</i>
                  <AnsiText text={line}></AnsiText>
                </div>
              ))}
          </LogLines>
        </LogBox>
      )}
    </Page>
  );
};

/**
 * The stage logs page; a different stage of the same reaction gets a fresh view so no state carries over.
 */
const ReactionStageLogs = () => {
  const { reactionID, group, stage } = useParams<{ reactionID: string; group: string; stage: string }>();
  return <StageLogsView key={`${group}/${reactionID}/${stage}`} />;
};

export default ReactionStageLogs;
