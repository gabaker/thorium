import { FC } from 'react';

// project imports
import { BannerStack, CodeSpans, EntityLink, KeyValues, Mono, Muted, Table } from '../shared';
import { InstanceStatusBadge } from '../InstanceStatusBadge';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { exclusionText, inclusionText } from '@utilities/agentic/display';
import { ToolSetInstance } from '@models/toolsets';

/** Props for {@link ToolSetInstanceView}. */
interface Props {
  /** The toolset instance to show */
  instance: ToolSetInstance;
}

/** Show one toolset instance: status, warnings, members, and excluded tools. */
export const ToolSetInstanceView: FC<Props> = ({ instance }) => {
  const meta = instance.metadata.ToolSetInstance;
  return (
    <div data-testid="toolset-instance-view">
      <h3>Showing {instance.name}</h3>
      <KeyValues>
        <dt>Instance</dt>
        <dd>
          {instance.name} <Mono>{instance.id}</Mono>
        </dd>
        <dt>Status</dt>
        <dd>
          <InstanceStatusBadge status={meta.status} />
        </dd>
        <dt>Triggered by</dt>
        <dd>{meta.triggered_by || <Muted>none</Muted>}</dd>
        <dt>Digest</dt>
        <dd>
          <Mono>{meta.digest}</Mono>
        </dd>
      </KeyValues>
      {meta.warnings.length > 0 && (
        <BannerStack>
          {meta.warnings.map((warning) => (
            <AlertBanner key={warning} severity={Severity.Warning}>
              <CodeSpans text={warning} />
            </AlertBanner>
          ))}
        </BannerStack>
      )}
      <h3>Members ({meta.members.length})</h3>
      {meta.members.length === 0 ? (
        <Muted>No members.</Muted>
      ) : (
        <Table data-testid="toolset-members">
          <thead>
            <tr>
              <th>Tool</th>
              <th>Version</th>
              <th>Function</th>
              <th>Inclusion</th>
            </tr>
          </thead>
          <tbody>
            {meta.members.map((member) => (
              <tr key={member.tool_instance}>
                <td>
                  <EntityLink to={`/tools/${member.tool}`}>{member.name}</EntityLink>
                </td>
                <td>{member.version}</td>
                <td>
                  <Mono>{member.function_name}</Mono>
                </td>
                <td>{inclusionText(member.inclusion)}</td>
              </tr>
            ))}
          </tbody>
        </Table>
      )}
      {meta.excluded.length > 0 && (
        <>
          <h3>Excluded ({meta.excluded.length})</h3>
          <Table data-testid="toolset-excluded">
            <thead>
              <tr>
                <th>Tool</th>
                <th>Reason</th>
              </tr>
            </thead>
            <tbody>
              {meta.excluded.map((item) => (
                <tr key={`${item.tool}-${item.reason}`}>
                  <td>{item.name}</td>
                  <td>{exclusionText(item)}</td>
                </tr>
              ))}
            </tbody>
          </Table>
        </>
      )}
    </div>
  );
};
