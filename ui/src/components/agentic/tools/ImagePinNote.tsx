import { FC } from 'react';
import styled from 'styled-components';

// project imports
import { Muted } from '@components/agentic/shared';

/** The pin note as a small block of secondary text, lighter than the versions table above it. */
const Note = styled(Muted)`
  display: block;
  font-size: 0.8rem;
  line-height: 1.4;
`;

/** Say that an agentic tool can pin a version of an Image tool and what that does to deprecating and deleting it. */
export const ImagePinNote: FC = () => (
  // the API offers no lookup of the tools that pin an Image tool, so the page can only describe the rule, not name the pinning tools
  <Note data-testid="image-pin-note">
    Agentic container tools can pin a version of an Image tool. A pinned version can&apos;t be deleted, and neither can the tool, until the
    tools that pin it are changed or deleted; the refusal names them. Deleting the tool removes its versions one at a time and stops at the
    first pinned one, so the versions before it are already gone when it stops. Deprecating a version leaves the tools that already pin it
    running it, but no new pin can name it.
  </Note>
);
