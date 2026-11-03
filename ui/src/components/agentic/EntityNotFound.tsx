import { FC, ReactNode } from 'react';

// project imports
import { EntityLink, Muted, PageHead, PageTitle, Section } from '@components/agentic/shared';
import { Button, ButtonVariant } from '@components/shared/buttons';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import Page from '@components/pages/Page';
import { entityKindNoun, withArticle } from '@utilities/agentic/display';

/** Props for {@link EntityNotFound}. */
interface EntityNotFoundProps {
  /** What the page shows, capitalised (for example `Toolset`) */
  what: string;
  /** Whether the entity is still loading */
  loading: boolean;
  /** The route of the page that lists these entities */
  listPath: string;
  /** The name of the list page the link points to (for example `toolsets`) */
  listName: string;
  /** The API's message for the failed read, shown as the detail; empty when there is none */
  error: string;
  /** The id that was opened and the kind of entity it turned out to be, when it exists but is not this page's kind */
  other?: { id: string; kind: string };
  /** Set when the read itself failed (such as a server error), so the page does not call the entity missing */
  failed?: boolean;
  /** Reads the entity again; offered as a Retry button when the read failed */
  onRetry?: () => void;
  /** A banner shown above the title, for the outcome of an action that removed the entity (for example a deletion result) */
  notice?: ReactNode;
}

/** A page body for an entity that could not be read: a spinner while loading, then a titled message with a link back to its list. */
export const EntityNotFound: FC<EntityNotFoundProps> = ({
  what,
  loading,
  listPath,
  listName,
  error,
  failed = false,
  onRetry,
  other,
  notice,
}) => (
  <Page title={`${what} · Thorium`}>
    <LoadingSpinner loading={loading} />
    {!loading && (
      <>
        {notice}
        <PageHead>
          <PageTitle data-testid="entity-not-found-title">{failed ? `${what} could not be loaded` : `${what} not found`}</PageTitle>
          <EntityLink to={listPath}>Back to {listName}</EntityLink>
        </PageHead>
        <Section data-testid="entity-not-found">
          {failed ? (
            // the read failed, so the entity may well exist: do not claim it is missing
            <p>
              The {what.toLowerCase()} could not be read. Try again, or browse the {listName} you can see to find the one you want.
            </p>
          ) : other ? (
            // the entity was read, so say what it is instead of claiming it cannot be seen
            <p>
              {other.id} is {withArticle(entityKindNoun(other.kind))}, not {withArticle(what.toLowerCase())}. Browse the {listName} you can
              see to find the one you want.
            </p>
          ) : (
            <p>
              This {what.toLowerCase()} does not exist, or you are not in a group that can see it. Browse the {listName} you can see to find
              the one you want.
            </p>
          )}
          {error && <Muted>{error}</Muted>}
          {failed && onRetry && (
            <div style={{ marginTop: '0.75rem' }}>
              <Button variant={ButtonVariant.Primary} data-testid="entity-retry" onClick={onRetry}>
                Retry
              </Button>
            </div>
          )}
        </Section>
      </>
    )}
  </Page>
);
