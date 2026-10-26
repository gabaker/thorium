import { FC, useState } from 'react';
import styled from 'styled-components';

// project imports
import { CardTable } from './runs.styled';
import { RunTime } from './RunTime';
import { actionName } from './stages';
import { StatusUpdateMessage } from './StatusUpdateMessage';
import { Muted, Section, TableScroll } from '@components/agentic/shared';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Button, ButtonVariant } from '@components/shared/buttons';
import { StatusUpdate } from '@models/reactions';

/**
 * The updates table; the Message cell breaks inside long unbroken text (ids, workers) so the table never outgrows the card,
 * while the Time and Action cells keep their text whole (the table scrolls sideways before they wrap).
 */
const UpdatesTable = styled(CardTable)`
  td:nth-child(1),
  td:nth-child(2) {
    white-space: nowrap;
  }

  td:nth-child(3) {
    overflow-wrap: anywhere;
  }
`;

/** The most updates shown before a Show more control. */
export const UPDATES_SHOWN = 100;

/** Props for {@link UpdatesContent}. */
interface UpdatesContentProps {
  /** The status updates read so far, or `null` before the first read finishes */
  updates: StatusUpdate[] | null;
  /** The message of the last failed read, or an empty string if the last read succeeded */
  error: string;
}

/** The body of the updates section: the read error, a loading or empty note, or the updates table. */
export const UpdatesContent: FC<UpdatesContentProps> = ({ updates, error }) => {
  const [all, setAll] = useState(false);
  const shown = updates && !all ? updates.slice(0, UPDATES_SHOWN) : updates;
  return (
    <>
      {/* a failed read says nothing about whether updates exist, so it is never shown as an empty list */}
      {error !== '' && (
        <AlertBanner severity={Severity.Error}>
          <span data-testid="run-updates-error">{error}</span>
        </AlertBanner>
      )}
      {updates === null && error === '' && <Muted>Loading status updates...</Muted>}
      {updates?.length === 0 && error === '' && <Muted>No status updates yet.</Muted>}
      {shown && shown.length > 0 && (
        <TableScroll>
          <UpdatesTable>
            <thead>
              <tr>
                <th>Time</th>
                <th>Action</th>
                <th>Message</th>
              </tr>
            </thead>
            <tbody>
              {shown.map((update, index) => (
                <tr key={index} data-testid="run-update-row">
                  <td>
                    <RunTime value={update.timestamp} />
                  </td>
                  <td>{actionName(update.action)}</td>
                  <td>
                    <StatusUpdateMessage update={update} />
                  </td>
                </tr>
              ))}
            </tbody>
          </UpdatesTable>
        </TableScroll>
      )}
      {updates && shown && updates.length > shown.length && (
        <div style={{ marginTop: '0.75rem' }}>
          <Button variant={ButtonVariant.Secondary} data-testid="run-updates-more" onClick={() => setAll(true)}>
            Show {updates.length - shown.length} more updates
          </Button>
        </div>
      )}
    </>
  );
};

/** Props for {@link RunUpdatesPanel}. */
interface RunUpdatesPanelProps {
  /** The status updates read so far, or `null` before the first read finishes */
  updates: StatusUpdate[] | null;
  /** The message of the last failed read, or an empty string */
  error: string;
}

/** The status updates the run's lifecycle recorded. */
export const RunUpdatesPanel: FC<RunUpdatesPanelProps> = ({ updates, error }) => (
  <Section data-testid="run-updates">
    <h2>Status updates</h2>
    <UpdatesContent updates={updates} error={error} />
  </Section>
);
