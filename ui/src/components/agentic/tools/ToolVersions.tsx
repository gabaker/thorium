import { FC } from 'react';

// project imports
import { ConfirmButton } from '@components/agentic/ConfirmButton';
import { Badge, BadgeTone, InlineList, Mono, Row, Table, TableScroll } from '@components/agentic/shared';
import { Button, ButtonSize, ButtonVariant } from '@components/shared/buttons';
import Time from '@components/shared/Time';
import { toolVersionLabel } from '@utilities/agentic/display';
import { ToolInstanceSummary } from '@models/tools';

/** Props for {@link ToolVersions}. */
interface ToolVersionsProps {
  /** The tool's instances, newest first */
  instances: ToolInstanceSummary[];
  /** The instance the tool follows at latest */
  latest: string | null;
  /** The instance whose spec is shown */
  selected: string;
  /** Whether the tool can be run directly (Image and Agentic tools) */
  runnable: boolean;
  /** Show an instance's spec */
  onSelect: (id: string) => void;
  /** Deprecate or restore an instance */
  onDeprecate: (id: string, deprecated: boolean) => void;
  /** Delete an instance */
  onDelete: (id: string) => void;
  /** Open the direct run form for an instance */
  onRun: (id: string) => void;
  /** Why the user can't deprecate or restore versions, or an empty string when they can */
  editReason?: string;
  /** Why the user can't delete a version, or an empty string when they can */
  deleteReasonOf?: (id: string) => string;
}

/** The versions of a tool with the actions each one allows. */
export const ToolVersions: FC<ToolVersionsProps> = ({
  instances,
  latest,
  selected,
  runnable,
  onSelect,
  onDeprecate,
  onDelete,
  onRun,
  editReason = '',
  deleteReasonOf = () => '',
}) => (
  <TableScroll>
    <Table data-testid="version-table">
      <thead>
        <tr>
          <th>Version</th>
          <th>Status</th>
          <th>Spec hash</th>
          <th>Created</th>
          <th>Instance id</th>
          <th />
        </tr>
      </thead>
      <tbody>
        {instances.map((instance) => {
          const label = toolVersionLabel(instance.version);
          const deleteReason = deleteReasonOf(instance.id);
          return (
            <tr
              key={instance.id}
              data-testid={`version-row-${label}`}
              style={selected === instance.id ? { background: 'var(--thorium-highlight-panel-bg)' } : undefined}
            >
              <td>{label}</td>
              <td>
                <InlineList>
                  {instance.id === latest && <Badge $tone={BadgeTone.Ok}>latest</Badge>}
                  {instance.deprecated && <Badge $tone={BadgeTone.Warn}>deprecated</Badge>}
                  {instance.id !== latest && !instance.deprecated && <Badge $tone={BadgeTone.Neutral}>live</Badge>}
                </InlineList>
              </td>
              <td>
                <Mono style={{ whiteSpace: 'nowrap' }}>{instance.spec_hash.slice(0, 12)}</Mono>
              </td>
              <td style={{ whiteSpace: 'nowrap' }}>
                <Time>{instance.created}</Time>
              </td>
              <td>
                {/* the full id is in the tooltip so a row stays on one line */}
                <Mono title={instance.id} style={{ whiteSpace: 'nowrap' }}>
                  {instance.id.slice(0, 8)}…
                </Mono>
              </td>
              <td>
                {/* the buttons wrap onto a second line in a narrow card so none is pushed past the table's right edge */}
                <Row data-testid={`version-actions-${label}`} style={{ flexWrap: 'wrap', gap: '0.5rem', minWidth: '7rem' }}>
                  <Button
                    size={ButtonSize.Small}
                    variant={ButtonVariant.Secondary}
                    data-testid={`view-spec-${label}`}
                    onClick={() => onSelect(instance.id)}
                  >
                    Spec
                  </Button>
                  {runnable && !instance.deprecated && (
                    <Button
                      size={ButtonSize.Small}
                      variant={ButtonVariant.Primary}
                      data-testid={`run-version-${label}`}
                      onClick={() => onRun(instance.id)}
                    >
                      Run
                    </Button>
                  )}
                  <Button
                    size={ButtonSize.Small}
                    variant={ButtonVariant.Warning}
                    data-testid={`deprecate-${label}`}
                    disabled={editReason !== ''}
                    title={editReason || undefined}
                    onClick={() => onDeprecate(instance.id, !instance.deprecated)}
                  >
                    {instance.deprecated ? 'Restore' : 'Deprecate'}
                  </Button>
                  <ConfirmButton
                    size={ButtonSize.Small}
                    testId={`delete-version-${label}`}
                    disabled={deleteReason !== ''}
                    title={deleteReason}
                    onConfirm={() => onDelete(instance.id)}
                  >
                    Delete
                  </ConfirmButton>
                </Row>
              </td>
            </tr>
          );
        })}
      </tbody>
    </Table>
  </TableScroll>
);
