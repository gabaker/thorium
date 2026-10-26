import { FC, useEffect, useState } from 'react';

// project imports
import { EntityLink, Mono } from '@components/agentic/shared';
import { getAgenticEntity } from '@thorpi/agentic';
import { ToolInstance } from '@models/tools';

/** Props for {@link PinnedBacking}. */
interface PinnedBackingProps {
  /** The id of the Image tool instance the spec is pinned to */
  id: string;
}

/** The Image tool version an agentic tool is pinned to, by name and linked to its tool; the id shows while the name is unread. */
export const PinnedBacking: FC<PinnedBackingProps> = ({ id }) => {
  const [pinned, setPinned] = useState<ToolInstance | null>(null);
  useEffect(() => {
    let cancelled = false;
    setPinned(null);
    // the name is a nicety, so a failed read (such as a group the user can't see) leaves the id shown
    void getAgenticEntity<ToolInstance>(id, () => undefined).then((found) => {
      if (!cancelled) setPinned(found);
    });
    return () => {
      cancelled = true;
    };
  }, [id]);
  if (!pinned) return <Mono data-testid="pinned-backing">{id}</Mono>;
  return (
    <span data-testid="pinned-backing" title={id}>
      <EntityLink to={`/tools/${pinned.metadata.ToolInstance.tool}`}>{pinned.name}</EntityLink>
    </span>
  );
};
