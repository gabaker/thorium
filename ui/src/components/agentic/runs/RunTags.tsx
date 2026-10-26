import { FC } from 'react';

// project imports
import { Badge, InlineList } from '@components/agentic/shared';
import { userRunTags } from '@utilities/runs';
import { Reaction } from '@models/reactions';

/** Props for {@link RunTags}. */
interface RunTagsProps {
  /** The run */
  run: Pick<Reaction, 'tags' | 'creator' | 'samples' | 'repos' | 'source'>;
}

/**
 * The tags a user gave a run. The API also tags every run with its creator and its inputs' SHA256s so
 * runs can be searched by them; the summary shows those as the Creator and Input files rows instead.
 * Renders nothing when the user gave no tags.
 */
export const RunTags: FC<RunTagsProps> = ({ run }) => {
  const tags = userRunTags(run);
  if (tags.length === 0) return null;
  return (
    <InlineList data-testid="run-tags">
      {tags.map((tag) => (
        <Badge key={tag}>{tag}</Badge>
      ))}
    </InlineList>
  );
};
