import { FC } from 'react';

// project imports
import { EntityLink, InlineList, Muted, NoWrap } from './shared';
import { AnyAgenticEntity } from '@models/agentic';

/** Props for {@link UsedBy}. */
interface UsedByProps {
  /** The entities that use the one being shown */
  users: Pick<AnyAgenticEntity, 'id' | 'name'>[];
  /** Builds the route of an entity's page */
  linkTo: (id: string) => string;
  /** Whether the entities that could use this one are still being read */
  loading: boolean;
  /** The error from reading them, or an empty string */
  error: string;
}

/** Link the entities that use another one, or say none do or that the answer isn't known yet. */
export const UsedBy: FC<UsedByProps> = ({ users, linkTo, loading, error }) => {
  // an answer from a list that is still loading or failed to load would be wrong, so say so instead
  if (error) return <Muted>unavailable</Muted>;
  if (loading) return <Muted>loading</Muted>;
  if (users.length === 0) return <Muted>none</Muted>;
  return (
    <InlineList>
      {users.map((user) => (
        <NoWrap key={user.id}>
          <EntityLink to={linkTo(user.id)}>{user.name}</EntityLink>
        </NoWrap>
      ))}
    </InlineList>
  );
};
