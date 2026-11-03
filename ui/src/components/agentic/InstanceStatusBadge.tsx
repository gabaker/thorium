import { FC, ReactNode } from 'react';
import styled from 'styled-components';

// project imports
import { Badge, CodeSpans, InlineList, statusTone } from '@components/agentic/shared';
import { instanceStatusErrors, isInstanceReady } from '@utilities/agentic/display';
import { InstanceStatus } from '@models/toolsets';

/** The reasons an instance is invalid; they wrap inside their cell however long they are. */
const Reasons = styled.div`
  margin-top: 0.25rem;
  font-size: 0.8rem;
  color: var(--thorium-secondary-text);
  overflow-wrap: anywhere;
`;

/** Props for {@link InstanceStatusBadge}. */
interface InstanceStatusBadgeProps {
  /** The instance's status */
  status: InstanceStatus;
  /** Further badges that sit in the same row as the status, such as a warnings count */
  children?: ReactNode;
}

/** A short status badge for a workflow or toolset instance, with the reasons an invalid one failed wrapped beneath it. */
export const InstanceStatusBadge: FC<InstanceStatusBadgeProps> = ({ status, children }) => {
  const ready = isInstanceReady(status);
  const errors = instanceStatusErrors(status);
  return (
    <>
      <InlineList>
        <Badge tone={statusTone(ready ? 'Ready' : 'Invalid')} title={errors.join('; ').replaceAll('`', '') || undefined}>
          {ready ? 'Ready' : 'Invalid'}
        </Badge>
        {children}
      </InlineList>
      {errors.map((error) => (
        <Reasons key={error} data-testid="instance-status-reason">
          <CodeSpans text={error} />
        </Reasons>
      ))}
    </>
  );
};
