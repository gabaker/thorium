import styled from 'styled-components';

// project imports
import { CellNote, GroupPills, GroupsCell, NameCell, NoWrapCell, TableFrame, TokenTable } from './ScopedTokens.styled';
import { formatExpire } from './scopedTokens';
import { Mono } from '@components/agentic/shared';
import { Badge, BadgeTone } from '@components/shared/badges/Badge';
import { ScopedToken } from '@models/users';

// The reaction id in the page's text colour (the default code colour clashes with the themes) and
// breaking anywhere so the long id wraps inside its cell.
const RunReaction = styled(Mono)`
  color: var(--thorium-text);
`;

// The run table needs less room than the table of user tokens, which also carries a column of controls.
const RunTable = styled(TokenTable)`
  min-width: 40rem;
`;

/**
 * The groups a token is limited to, as wrapping pills.
 *
 * @param groups - The group names.
 */
export const TokenGroups: React.FC<{ groups: string[] }> = ({ groups }) => {
  // an empty scope still renders a cell value so the column does not look broken
  if (groups.length === 0) return <CellNote>-</CellNote>;
  return (
    <GroupPills>
      {[...groups].sort().map((group) => (
        <Badge key={group} tone={BadgeTone.Accent} wrap title={group} data-testid="token-group">
          {group}
        </Badge>
      ))}
    </GroupPills>
  );
};

/**
 * A read-only table of run tokens.
 *
 * The API mints a run token for a workflow or tool run and ends it with the run, so the table has no
 * controls: no copy, activate, edit, or delete.
 *
 * @param tokens - The run tokens to list.
 */
const RunTokenTable: React.FC<{ tokens: ScopedToken[] }> = ({ tokens }) => (
  <TableFrame>
    <RunTable aria-label="Run tokens">
      <thead>
        <tr>
          <th>Name</th>
          <th>Run</th>
          <th>Tool instances</th>
          <th>Groups</th>
          <th>Value expiry</th>
          <th>Token expiry</th>
        </tr>
      </thead>
      <tbody>
        {tokens.map((tok) => (
          <tr key={tok.name} data-run-token="true">
            <NameCell>{tok.name}</NameCell>
            <td>
              <RunReaction title="The reaction this token is limited to">{tok.run?.reaction ?? '-'}</RunReaction>
            </td>
            <td>{tok.run?.tool_instances.length ?? 0}</td>
            <GroupsCell>
              <TokenGroups groups={tok.groups} />
            </GroupsCell>
            <NoWrapCell>{formatExpire(tok.token_expiration)}</NoWrapCell>
            <NoWrapCell>{formatExpire(tok.expires)}</NoWrapCell>
          </tr>
        ))}
      </tbody>
    </RunTable>
  </TableFrame>
);

export default RunTokenTable;
