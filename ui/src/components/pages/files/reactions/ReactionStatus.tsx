import { useQueryClient } from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import { FaTrash } from 'react-icons/fa';
import { Link } from 'react-router';
import styled from 'styled-components';

// project imports
import { getStatusBadge } from './reactions';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Button, ButtonSize, ButtonVariant, IconButton } from '@components/shared/buttons';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import Modal, { ModalSize } from '@components/shared/modal/Modal';
import Subtitle from '@components/shared/titles/Subtitle';
import { OverlayTipTop, OverlayTipLeft } from '@components/shared/overlay/tips';
import { useAuth } from '@utilities/auth';
import { listReactions, deleteReaction } from '@thorpi/reactions';
import { invalidateRunLists } from '@utilities/queries/runs';
import { reactionPath } from '@utilities/runs';
import { spacers } from '@styles';
import type { Reaction, ReactionRunResult } from '@models/reactions';

/** The reaction tab: the header panel, the reaction rows, and the delete results, kept off the page edges. */
const ReactionsTab = styled.div`
  display: flex;
  flex-direction: column;
  gap: ${spacers.two};
  margin: 0 ${spacers.four} ${spacers.five};
`;

/** The header row of the reaction list, drawn as a panel above the rows. */
const HeaderPanel = styled.div`
  padding: ${spacers.three} ${spacers.two};
  border: 1px solid var(--thorium-panel-border);
  border-radius: 6px;
  color: var(--thorium-text);
  background-color: var(--thorium-panel-bg);
`;

/** One reaction's row; every cell links to the reaction except the selection checkbox. */
const ReactionLine = styled.div`
  padding: ${spacers.two};
  color: var(--thorium-text);
  background-color: var(--thorium-panel-bg);

  &:hover {
    background-color: var(--thorium-highlight-panel-bg);
    box-shadow:
      inset 0 0 1px var(--thorium-panel-border),
      0 0 4px var(--thorium-highlight-panel-border);
  }

  a {
    color: inherit;
  }
`;

/** A cell of the reaction list; it pads only its sides, so a cell's height stays its lines' height. */
const Cell = styled.div`
  padding: 0 ${spacers.two};
`;

/** The selection column: the delete button and the select-all box in the header, a row's box in a row. */
const SelectionCell = styled(Cell)`
  gap: ${spacers.three};
`;

/** The reactions listed for deletion in the confirmation dialog. */
const DeleteList = styled.ul`
  margin: 0;
  padding: 0;
  list-style: none;
  text-align: center;
  font-weight: 700;
`;

/** The props of {@link DeleteReactionAlerts}. */
interface DeleteReactionAlertsProps {
  /** The outcome of each requested deletion; an empty error means it succeeded. */
  responses: ReactionRunResult[];
}

/**
 * Show one banner per requested deletion: the error for a failure, a notice for a success.
 *
 * @param props - The deletion outcomes.
 * @returns The banners.
 */
const DeleteReactionAlerts = ({ responses }: DeleteReactionAlertsProps) => (
  <>
    {responses.map((deleteResponse, idx) =>
      deleteResponse.error ? (
        <AlertBanner key={idx}>{deleteResponse.error}</AlertBanner>
      ) : (
        <AlertBanner key={idx} severity={Severity.Info}>
          {`Successfully deleted reaction ${deleteResponse.id}`}
          {` for pipeline ${deleteResponse.pipeline} from group ${deleteResponse.group}!`}
        </AlertBanner>
      ),
    )}
  </>
);

// module-level flag to pause auto-refresh during deletion
let deleteInProgress = false;

interface ReactionStatusProps {
  sha256: string;
  autoRefresh: boolean;
}

const ReactionStatus = ({ sha256, autoRefresh }: ReactionStatusProps) => {
  const client = useQueryClient();
  const [loading, setLoading] = useState(false);
  const [reactionsList, setReactionsList] = useState<Reaction[]>([]);
  const [reactionsMap, setReactionsMap] = useState<Record<string, Reaction>>({});
  const [reactionsListSelections, setReactionsListSelections] = useState<Record<string, boolean>>({});
  const [reactionsAllSelected, setReactionsAllSelected] = useState(false);
  const { userInfo, checkCookie } = useAuth();
  const [showDeleteModal, setShowDeleteModal] = useState(false);
  const [showDeleteItems] = useState(5);
  const [deleteReactionResponses, setDeleteReactionResponses] = useState<ReactionRunResult[]>([]);

  // fetch all reactions across user's groups
  const getReactionsList = async () => {
    setLoading(true);
    if (!deleteInProgress) {
      const reactions: Reaction[] = [];
      if (userInfo && userInfo.groups) {
        for (const group of userInfo.groups) {
          let moreReactions = true;
          let cursor: string | undefined = undefined;
          while (moreReactions) {
            const reactionsListRes = await listReactions(
              group,
              () => {
                void checkCookie();
              },
              '',
              sha256,
              true,
              cursor ?? null,
              10000,
            );
            if (reactionsListRes && 'details' in reactionsListRes) {
              reactions.push(...reactionsListRes.details);
              if (reactionsListRes.cursor == undefined) {
                moreReactions = false;
              } else {
                cursor = String(reactionsListRes.cursor);
              }
            } else {
              moreReactions = false;
            }
          }
        }
        setReactionsList(reactions);
        const newMap: Record<string, Reaction> = {};
        reactions.forEach((reaction) => (newMap[reaction.id] = reaction));
        setReactionsMap(newMap);
      }
      setLoading(false);
    }
  };

  // trigger reaction list fetch when component is viewed
  useEffect(() => {
    if (autoRefresh) {
      void getReactionsList();
      const intervalId = setInterval(() => {
        void getReactionsList();
      }, 30000);
      return () => {
        clearInterval(intervalId);
      };
    }
  }, [userInfo, sha256, autoRefresh]);

  const handleSelectionChange = (key: string) => {
    setDeleteReactionResponses([]);
    setReactionsListSelections((prevState) => {
      const newState = { ...prevState };
      newState[key] = prevState[key] == undefined ? true : !prevState[key];
      return newState;
    });
    setReactionsAllSelected(false);
  };

  const handleSelectAll = () => {
    setDeleteReactionResponses([]);
    const newSelections: Record<string, boolean> = {};
    for (const reaction of reactionsList) {
      if (reaction.id) {
        newSelections[reaction.id] = !reactionsAllSelected;
      }
    }
    setReactionsListSelections(newSelections);
    setReactionsAllSelected(!reactionsAllSelected);
  };

  const handleShowDeleteModal = () => {
    setShowDeleteModal(true);
  };

  const handleCloseDeleteModal = () => {
    setShowDeleteModal(false);
  };

  // check if any selections are active
  const anySelected = (): boolean => {
    return Object.values(reactionsListSelections).some((v) => v);
  };

  // build truncated list for the delete confirmation modal
  const truncateSelections = (selections: Record<string, boolean>): string[] => {
    let truncatedList: string[] = [];
    Object.keys(selections).map((reactionID) => {
      if (selections[reactionID] && reactionsMap[reactionID]) {
        truncatedList.push(`${reactionsMap[reactionID].pipeline} : ${reactionsMap[reactionID].group}`);
      }
    });
    if (truncatedList.length > 5) {
      const truncateMsg = `${truncatedList.length - 5} more selections ...`;
      truncatedList = truncatedList.slice(0, showDeleteItems);
      truncatedList.push(truncateMsg);
    }
    return truncatedList;
  };

  // delete all selected reactions
  const handleDeleteClick = async () => {
    setShowDeleteModal(false);
    setLoading(true);
    deleteInProgress = true;
    setReactionsListSelections({});
    setReactionsAllSelected(false);

    const deleteErrors: ReactionRunResult[] = [];
    for (const reactionID of Object.keys(reactionsListSelections)) {
      if (reactionsListSelections[reactionID] && reactionsMap[reactionID]) {
        const reaction = reactionsMap[reactionID];
        const handleError = (error: string) => {
          deleteErrors.push({
            error,
            id: reaction.id,
            group: reaction.group,
            pipeline: reaction.pipeline,
          });
        };
        const res = await deleteReaction(reaction.group, reaction.id, handleError);
        if (res) {
          deleteErrors.push({
            error: '',
            id: reaction.id,
            group: reaction.group,
            pipeline: reaction.pipeline,
          });
        }
      }
    }
    // the cached run lists may still hold the deleted reactions
    void invalidateRunLists(client);
    setLoading(false);
    deleteInProgress = false;
    await getReactionsList();
    setDeleteReactionResponses(deleteErrors);
  };

  return (
    <ReactionsTab id="reactionstatus-tab">
      {!loading && reactionsList.length == 0 ? (
        <AlertBanner severity={Severity.Info}>
          <h3>No Reactions Found</h3>
        </AlertBanner>
      ) : (
        <>
          <LoadingSpinner loading={loading}></LoadingSpinner>
          {!loading && (
            <>
              <HeaderPanel className="reactions-grid">
                <Cell className="reactions-pipeline">
                  <Subtitle>Run</Subtitle>
                </Cell>
                <Cell className="reactions-creator">
                  <Subtitle>Creator</Subtitle>
                </Cell>
                <Cell className="reactions-group">
                  <Subtitle>Group</Subtitle>
                </Cell>
                <Cell className="reactions-status">
                  <Subtitle>Status</Subtitle>
                </Cell>
                <Cell className="reactions-id">
                  <Subtitle>Reaction ID</Subtitle>
                </Cell>
                <SelectionCell className="reactions-selection">
                  <OverlayTipTop
                    tip={`Delete selected reactions. Only system admins,
                          group owners/managers, and the submitter can delete a reaction.`}
                  >
                    <IconButton
                      size={ButtonSize.Small}
                      aria-label="Delete selected reactions"
                      disabled={!anySelected()}
                      onClick={handleShowDeleteModal}
                    >
                      <FaTrash />
                    </IconButton>
                  </OverlayTipTop>
                  <OverlayTipLeft tip={'Select All Reactions'}>
                    <input type="checkbox" aria-label="Select all reactions" onChange={handleSelectAll} checked={reactionsAllSelected} />
                  </OverlayTipLeft>
                </SelectionCell>
              </HeaderPanel>
              {reactionsList.map((reaction, idx) => (
                <ReactionLine key={`${reaction.id}_${idx}`} className="reactions-grid">
                  <Link to={reactionPath(reaction)} className="no-decoration reactions-pipeline">
                    <Cell>{reaction.pipeline}</Cell>
                  </Link>
                  <Link to={reactionPath(reaction)} className="no-decoration reactions-creator" title={reaction.creator}>
                    <Cell>{reaction.creator}</Cell>
                  </Link>
                  <Link to={reactionPath(reaction)} className="no-decoration reactions-group" title={reaction.group}>
                    <Cell>{reaction.group}</Cell>
                  </Link>
                  <Link to={reactionPath(reaction)} className="no-decoration reactions-status">
                    <Cell>{getStatusBadge(reaction.status)}</Cell>
                  </Link>
                  <Link to={reactionPath(reaction)} className="no-decoration reactions-id" title={reaction.id}>
                    <Cell>{reaction.id}</Cell>
                  </Link>
                  <SelectionCell className="reactions-selection">
                    <input
                      type="checkbox"
                      aria-label={`Select reaction ${reaction.id}`}
                      onChange={() => {
                        handleSelectionChange(reaction.id);
                      }}
                      checked={reactionsListSelections[reaction.id] ?? false}
                    />
                  </SelectionCell>
                </ReactionLine>
              ))}
            </>
          )}
        </>
      )}
      <DeleteReactionAlerts responses={deleteReactionResponses} />
      <Modal
        show={showDeleteModal}
        onClose={handleCloseDeleteModal}
        size={ModalSize.Small}
        title="Confirm deletion?"
        actions={
          <>
            <Button variant={ButtonVariant.Secondary} onClick={handleCloseDeleteModal}>
              Cancel
            </Button>
            <Button
              variant={ButtonVariant.Danger}
              onClick={() => {
                void handleDeleteClick();
              }}
            >
              Confirm
            </Button>
          </>
        }
      >
        <p>Do you really want to delete the following reactions:</p>
        <DeleteList>
          {truncateSelections(reactionsListSelections).map((reactionString, idx) => (
            <li key={idx}>{reactionString}</li>
          ))}
        </DeleteList>
      </Modal>
    </ReactionsTab>
  );
};

export default ReactionStatus;
