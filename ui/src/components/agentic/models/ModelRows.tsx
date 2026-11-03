import { FC, Fragment, ReactNode } from 'react';
import styled from 'styled-components';

// project imports
import {
  banCount,
  camelWords,
  capabilityNames,
  catalogBans,
  credentialRef,
  idSegments,
  isLongLimit,
  isLongNameChunk,
  isSqueezableNameChunk,
  kindWords,
  limitsParts,
  messageParts,
  nameChunks,
  parameterEntries,
  parameterNameChunks,
  providerTarget,
  splitLongNameChunk,
  taggedDetails,
  variantText,
} from './format';
import { Badge, BadgeTone, InlineList, Mono, Muted, SEVERITY_BY_LEVEL, Table as BaseTable } from '@components/agentic/shared';
import AlertBanner from '@components/shared/alerts/AlertBanner';
import Time from '@components/shared/Time';
import { scaling } from '@styles';
import { banKindText, banMessage, sortedBans } from '@utilities/agentic/display';
import { EntityNotification } from '@models/agentic';
import { ModelCatalogEntry, ModelCredential, ModelEndpoint, ModelLimits, ModelProfile } from '@models/modelconfigs';

/** Props for {@link Notifications}. */
interface NotificationsProps {
  /** The entity's ban notifications */
  items: EntityNotification[] | undefined;
  /** How many columns of the table the banners span */
  columns: number;
  /** The test id of the row holding the banners */
  testId: string;
}

/** A notification message that may break anywhere, since it can quote a long model id that would otherwise widen its table. */
const NotificationText = styled.span`
  overflow-wrap: anywhere;
`;

/** Show an entity's message with its backtick quotes set as code. */
const NoteMessage: FC<{ msg: string }> = ({ msg }) => (
  <NotificationText data-testid="model-notification">
    {messageParts(msg).map((part, index) => (part.code ? <code key={index}>{part.text}</code> : part.text))}
  </NotificationText>
);

/** Show an entity's ban notifications as severity banners in a full width row, so a long message is not squeezed into one column. */
export const Notifications: FC<NotificationsProps> = ({ items, columns, testId }) => {
  if (!items || items.length === 0) return null;
  return (
    <NotesRow data-testid={testId}>
      <td colSpan={columns}>
        {items.map((note) => (
          <AlertBanner key={note.id} severity={SEVERITY_BY_LEVEL[note.level]}>
            <NoteMessage msg={note.msg} />
          </AlertBanner>
        ))}
      </td>
    </NotesRow>
  );
};

/**
 * A badge for a ban, which wraps instead of keeping to one line so it cannot widen its table. Its padding is slim and
 * its spacing is left to the {@link BanList} that holds it, so the Bans column is no wider than its longest unbreakable word.
 */
const BanBadge = styled(Badge)`
  max-width: 100%;
  padding-left: 0.35rem;
  padding-right: 0.35rem;
`;

/** The badges of a Bans cell, which wrap onto new lines that start flush with the first one. */
const BanList = styled.div`
  display: flex;
  flex-wrap: wrap;
  gap: 0.25rem;
`;

/**
 * Show a ban kind such as `ModelRemoved`, offering a line break between its words so a long kind
 * cannot widen the column that holds it.
 *
 * @param props - The ban kind.
 * @param props.kind - The ban kind.
 */
const BanKind: FC<{ kind: string }> = ({ kind }) => (
  <>
    {kindWords(kind).map((word, index, words) => (
      // a word can repeat within a kind, so its position makes the key unique
      <Fragment key={`${index}-${word}`}>
        {word}
        {index < words.length - 1 && <wbr />}
      </Fragment>
    ))}
  </>
);

/** Show the bans of a model entity as badges, each carrying its message as a tooltip. */
export const Bans: FC<{ bans: ModelEntityBans; catalog?: ModelCatalogEntry[] }> = ({ bans, catalog }) => {
  const modelBans = catalogBans(catalog);
  if (banCount(bans) + modelBans.length === 0) return <Muted>none</Muted>;
  return (
    <BanList>
      {sortedBans(bans).map((ban) => (
        <BanBadge key={ban.id} tone={BadgeTone.Error} wrap title={banMessage(ban)} data-testid="ban-badge">
          <BanKind kind={banKindText(ban)} />
        </BanBadge>
      ))}
      {modelBans.map(({ modelId, ban }) => (
        <BanBadge key={ban.id} tone={BadgeTone.Error} wrap title={`${modelId}: ${banMessage(ban)}`} data-testid="model-ban-badge">
          <IdText id={modelId} compact />: <BanKind kind={banKindText(ban)} />
        </BanBadge>
      ))}
    </BanList>
  );
};

type ModelEntityBans = ModelEndpoint['metadata']['ModelEndpoint']['bans'];

/**
 * A table with tighter cell padding than the shared one, so eight columns fit the panel at 1024 pixels wide without
 * scrolling. Its last column, the Bans, has no right padding: the panel's own padding already keeps its badges off the edge.
 */
const Table = styled(BaseTable)`
  th,
  td {
    padding-left: 0.4rem;
    padding-right: 0.4rem;
  }

  th:last-child,
  td:last-child {
    padding-right: 0;
  }
`;

/** A row of banners under an entity's row; it has no hover highlight of its own. */
const NotesRow = styled.tr`
  && td {
    padding-top: 0;
  }
`;

/** One line of secondary detail under a cell's main text. */
const Detail = styled.div`
  color: var(--thorium-secondary-text);
  font-size: 0.8rem;
  overflow-wrap: anywhere;
`;

/**
 * The longest piece of an id kept on one line at every width; longer pieces may break anywhere so they cannot widen
 * the table past its panel. It is as long as the longest id kept whole, which holds the vendor prefix of an id such as
 * `NousResearch/Hermes-3-Llama-3.1-405B` whole.
 */
const LONGEST_WHOLE_PIECE = 16;

/**
 * The longest piece of an id that stays on one line while the panel has room for it. A piece between
 * `LONGEST_WHOLE_PIECE` and this length (`cognitivecomputations/`) breaks anywhere only below the `xxl` breakpoint,
 * where eight columns share a narrow panel; wider than that the table keeps it whole.
 */
const LONGEST_ROOMY_PIECE = 24;

/**
 * The longest piece of a compact id kept on one line. A compact id shares a squeezed cell such as a ban badge with
 * other text, and the table already spends the width of the id's longest piece on the Models column, so a piece longer
 * than any word of a common model id (`anthropic.`, `20250929-`) may break anywhere: an OpenRouter or Together id
 * such as `togethercomputer/RedPajama-INCITE-7B-Chat` would otherwise hold its badge wide enough to push the table past its panel.
 */
const LONGEST_COMPACT_PIECE = 12;

/** The longest id kept entirely on one line; a short id such as `stub-model-a` never splits, so no single character is stranded. */
const SHORT_ID_LENGTH = 16;

/** Monospace text for a model id or fingerprint that wraps only after its separators. */
const IdMono = styled(Mono)`
  overflow-wrap: normal;
  word-break: normal;
`;

/** Monospace text for a short model id that always stays on one line. */
const ShortIdMono = styled(IdMono)`
  white-space: nowrap;
`;

/** A piece of an id too long to keep whole, which may break at any character. */
const LongIdPiece = styled.span`
  overflow-wrap: anywhere;
`;

/**
 * A piece of an id that is long but not overlong. It may break at any character only in a narrow panel: `break-word`
 * leaves its whole length as the column's minimum width, so a wide panel keeps the piece whole instead of squeezing
 * the Models column for the Bans column's badges, while `anywhere` below the breakpoint lets the column shrink.
 */
const RoomyIdPiece = styled.span`
  overflow-wrap: break-word;

  @media (max-width: ${scaling.xxl}) {
    overflow-wrap: anywhere;
  }
`;

/** A chunk of an entity name, a limit or an id piece that stays on one line. */
const WholeText = styled.span`
  white-space: nowrap;
`;

/** The last character of a piece with the separator or separators after it (`s/` of `cognitivecomputations/`). */
const TRAILING_SEPARATOR = /.[.:/_@-]+$/;

/** A final piece that absorbed a short tail, so it holds a separator before its last character (`lite-v1`, `v1:0`). */
const HAS_INNER_SEPARATOR = /[.:/_@-]./;

/**
 * Render one piece of an id. A piece that fits on a line is plain text, except a final piece that absorbed a short
 * tail: it stays on one line, because the browser would otherwise break it after its own hyphen and strand the
 * tail (`lite-` / `v1`). A piece longer than `roomy` characters may break at any character; a piece longer than
 * `longest` but within `roomy` breaks anywhere only in a narrow panel. Either way the letter before a trailing
 * separator stays on one line with it (`cognitivecomputation` + `s/`), so no line starts with the separator.
 *
 * @param piece - The piece of the id.
 * @param isLast - Whether the piece is the last of the id.
 * @param longest - The longest piece kept whole at every width, including any tail it absorbed.
 * @param roomy - The longest piece kept whole while the panel is wide; defaults to `longest`.
 */
const renderIdPiece = (piece: string, isLast: boolean, longest: number, roomy: number = longest): ReactNode => {
  const merged = isLast && HAS_INNER_SEPARATOR.test(piece);
  // a piece no longer than the limit stays as it is
  if (piece.length <= longest) return merged ? <WholeText>{piece}</WholeText> : piece;
  // a piece longer than the roomy limit breaks anywhere, so a long piece never widens its column; a piece between the
  // limits breaks anywhere only in a narrow panel, where the column has no room for it
  const Breakable = piece.length > roomy ? LongIdPiece : RoomyIdPiece;
  // the letter before a trailing separator stays with it, so a break never starts the next line with the separator
  const tailStart = TRAILING_SEPARATOR.exec(piece)?.index;
  if (tailStart === undefined || tailStart === 0) return <Breakable>{piece}</Breakable>;
  return (
    <>
      <Breakable>{piece.slice(0, tailStart)}</Breakable>
      <WholeText>{piece.slice(tailStart)}</WholeText>
    </>
  );
};

/** Props for {@link IdText}. */
interface IdTextProps {
  /** The model id or fingerprint */
  id: string;
  /** Whether the id sits in a cell squeezed to its narrowest, so even a short id wraps after its separators */
  compact?: boolean;
}

/**
 * Show a model id or fingerprint in monospace, wrapping it only after a separator such as `-`, `.` or `:` so its
 * words stay whole, and breaking a piece mid-word only when it is too long to fit on its own. An id of at most
 * `SHORT_ID_LENGTH` characters is never broken, unless it is `compact`, which lets the id wrap after any
 * separator. A compact id also breaks a piece mid-word once it is longer than `LONGEST_COMPACT_PIECE`, which still keeps
 * the words of a common id such as `us.anthropic.claude-sonnet-4-5-20250929-v1:0` whole.
 *
 * @param props - The id and whether it is shown compactly.
 * @param props.id - The model id or fingerprint.
 * @param props.compact - Whether the id sits in a cell squeezed to its narrowest.
 */
export const IdText: FC<IdTextProps> = ({ id, compact = false }) => {
  // a short id keeps to one line, except where its cell is too narrow to hold it
  if (!compact && id.length <= SHORT_ID_LENGTH) {
    return (
      <ShortIdMono data-testid="id-text" title={id}>
        {id}
      </ShortIdMono>
    );
  }
  return (
    <IdMono data-testid="id-text" title={id}>
      {idSegments(id).map((piece, index, pieces) => (
        // a piece can repeat within an id, so its position makes the key unique
        <Fragment key={`${index}-${piece}`}>
          {compact
            ? renderIdPiece(piece, index === pieces.length - 1, LONGEST_COMPACT_PIECE)
            : renderIdPiece(piece, index === pieces.length - 1, LONGEST_WHOLE_PIECE, LONGEST_ROOMY_PIECE)}
          {/* no break opportunity follows the last piece: it would add an empty line fragment to the element's client rects */}
          {index < pieces.length - 1 && <wbr />}
        </Fragment>
      ))}
    </IdMono>
  );
};

/**
 * Show a CamelCase variant name, such as a provider's, so it may wrap between its words instead of holding its
 * table column as wide as the whole name.
 *
 * @param props - The variant name.
 * @param props.name - The variant name.
 */
const VariantName: FC<{ name: string }> = ({ name }) => (
  <>
    {camelWords(name).map((word, index, words) => (
      // a word can repeat within the name, so its position makes the key unique
      <Fragment key={`${index}-${word}`}>
        {word}
        {index < words.length - 1 && <wbr />}
      </Fragment>
    ))}
  </>
);

/** The part of an overlong entity name chunk that may break anywhere instead of widening its table. */
const LongNameChunk = styled.span`
  overflow-wrap: anywhere;
`;

/**
 * The part of an ordinary long word of an entity name that may break anywhere only on a narrow viewport. Where
 * the page is wide the word has no break opportunity, because a column allowed to break a word takes less width
 * than the whole word needs even when the table has room for it.
 */
const NarrowBreakNameChunk = styled.span`
  @media (max-width: ${scaling.xl}) {
    overflow-wrap: anywhere;
  }
`;

/**
 * Show a chunk of an entity name. A chunk that fits a column stays whole on one line. A longer one breaks mid-word as
 * a last resort (at every width when it is very long, on a narrow viewport when it is an ordinary word), except for
 * the tail `splitLongNameChunk` gives it, which stays together so no line starts with a separator or holds a single
 * stranded character.
 *
 * @param props - The chunk.
 * @param props.chunk - The chunk from `nameChunks`.
 */
const NameChunk: FC<{ chunk: string }> = ({ chunk }) => {
  // pick how the head of the chunk may break, if at all
  const Head = isLongNameChunk(chunk) ? LongNameChunk : isSqueezableNameChunk(chunk) ? NarrowBreakNameChunk : null;
  if (!Head) return <WholeText>{chunk}</WholeText>;
  const [body, tail] = splitLongNameChunk(chunk);
  return (
    <>
      {body.length > 0 && <Head>{body}</Head>}
      <WholeText>{tail}</WholeText>
    </>
  );
};

/**
 * Show an entity's name. The name wraps only between the chunks of `nameChunks`, so each chunk stays whole and
 * no single character is stranded on a line of its own; a chunk too long for a column breaks mid-word rather than push
 * its table past its panel (on a narrow viewport only for an ordinary long word, see `isSqueezableNameChunk`, and at
 * every width for a very long chunk, see `isLongNameChunk`).
 *
 * @param name - The entity's name.
 */
export const EntityName: FC<{ name: string }> = ({ name }) => (
  <>
    {nameChunks(name).map((chunk, index, chunks) => (
      // a chunk can repeat within the name, so its position makes the key unique
      <Fragment key={`${index}-${chunk}`}>
        <NameChunk chunk={chunk} />
        {index < chunks.length - 1 && <wbr />}
      </Fragment>
    ))}
  </>
);

/** A limit on a line of its own, so a stacked list of limits has no separator to add to the width of its widest limit. */
const StackedLimit = styled(WholeText)`
  display: block;
`;

/**
 * A limit too long to keep whole (see `isLongLimit`) on a line of its own. Unlike a {@link StackedLimit} it may break
 * between its label and its value (`context` / `1000000`), so a long value such as a 1M context window gives up width
 * in a tight table instead of pushing a column out of its panel.
 */
const StackedLongLimit = styled.span`
  display: block;
`;

/** Props for {@link LimitsText}. */
interface LimitsTextProps {
  /** The limits, if any are known */
  limits: ModelLimits | null | undefined;
  /** Whether each limit takes a line of its own with no separator, for a column too narrow to spare the separator's width */
  stacked?: boolean;
}

/**
 * Show a model's limits, keeping each `label value` pair such as `context 128000` on one line. The pairs are
 * separated by a `·` and wrap only between pairs, after the separator, or when `stacked` each take a line of
 * their own. A stacked pair longer than `context 128000` may also break between its label and its value.
 *
 * @param props - The limits and whether to stack them.
 * @param props.limits - The limits, if any are known.
 * @param props.stacked - Whether each limit takes a line of its own with no separator.
 */
const LimitsText: FC<LimitsTextProps> = ({ limits, stacked = false }) => (
  <>
    {limitsParts(limits).map((part, index, parts) =>
      stacked ? (
        // a limit keeps its label with its value unless it is too long for a tight column
        isLongLimit(part) ? (
          <StackedLongLimit key={part}>{part}</StackedLongLimit>
        ) : (
          <StackedLimit key={part}>{part}</StackedLimit>
        )
      ) : (
        <Fragment key={part}>
          {/* the separator stays with the pair before it, so a wrapped line never starts with a dot */}
          <WholeText>{index < parts.length - 1 ? `${part} ·` : part}</WholeText>{' '}
        </Fragment>
      ),
    )}
  </>
);

/** A chunk of a parameter name, kept whole so a name such as `max_output_tokens` is not split into slivers at its underscores. */
const ParameterChunk = styled.span`
  white-space: nowrap;
`;

/** A parameter value, which may break anywhere so a long value cannot widen its table. */
const ParameterValue = styled.span`
  overflow-wrap: anywhere;
`;

/**
 * Show a profile's parameters as `name value` pairs separated by commas. A name wraps only between the chunks of
 * `parameterNameChunks`, after an underscore, and the line wraps between a name and its value or between pairs.
 *
 * @param props - The profile's parameters.
 * @param props.parameters - The profile's parameters as sent by the API.
 */
const ParameterList: FC<{ parameters: Record<string, unknown> | null | undefined }> = ({ parameters }) => {
  const entries = parameterEntries(parameters);
  if (entries.length === 0) return <>-</>;
  return (
    <>
      {entries.map(({ name, value }, index) => (
        <Fragment key={name}>
          {parameterNameChunks(name).map((chunk, position, chunks) => (
            // a chunk can repeat within the name, so its position makes the key unique
            <Fragment key={`${position}-${chunk}`}>
              <ParameterChunk>{chunk}</ParameterChunk>
              {position < chunks.length - 1 && <wbr />}
            </Fragment>
          ))}{' '}
          <ParameterValue>{value}</ParameterValue>
          {index < entries.length - 1 && ', '}
        </Fragment>
      ))}
    </>
  );
};

/** Show a list of model ids separated by commas, each wrapping only after its separators. */
const IdList: FC<{ ids: string[] }> = ({ ids }) => (
  <>
    {ids.length === 0 && <Muted>none</Muted>}
    {ids.map((id, index) => (
      <Fragment key={id}>
        {index > 0 && ', '}
        <IdText id={id} />
      </Fragment>
    ))}
  </>
);

/** A capability name that stays whole; the names wrap between one another rather than inside. */
const Capability = styled.span`
  white-space: nowrap;
`;

/** One model of an endpoint's catalog: its id wrapping only after separators, with its limits underneath. */
const ModelItem = styled.div`
  margin-bottom: 0.25rem;
`;

/** Props for {@link EmptyNote}. */
interface EmptyNoteProps {
  /** What there are none of, for example `model endpoints` */
  what: string;
  /** The thorctl command that makes one */
  command: string;
}

/** Tell a user with no entities of a kind that there are none and where they are managed. */
const EmptyNote: FC<EmptyNoteProps> = ({ what, command }) => (
  <Muted as="div" data-testid="model-empty">
    No {what}. This page is read only; manage them with <Mono>{command}</Mono>.
  </Muted>
);

/** Props for the row tables. */
interface TableProps<T> {
  /** The entities to list */
  items: T[];
  /** Whether the list is still loading, so an empty list is not yet an empty state */
  loading?: boolean;
  /** Whether the list failed to load, so an empty list is not an empty state and the page's error banner speaks instead */
  failed?: boolean;
  /** The ban notifications of the entities by id */
  notifications: Record<string, EntityNotification[]>;
}

/** Props for {@link EndpointTable}. */
interface EndpointTableProps extends TableProps<ModelEndpoint> {
  /** The credentials the user can see, used to name the credential each endpoint references */
  credentials: ModelCredential[];
  /** Whether the credentials failed to load, so a credential missing from the list is unknown rather than hidden from the user */
  credentialsFailed?: boolean;
  /** Whether the credentials are still loading, so a credential missing from the list is not yet known to be hidden from the user */
  credentialsLoading?: boolean;
}

/** Props for {@link ProfileTable}. */
interface ProfileTableProps extends TableProps<ModelProfile> {
  /** The endpoints the user can see, used to name the endpoint each profile calls */
  endpoints: ModelEndpoint[];
}

/** Show the credential an endpoint authenticates with: its name, kind and fingerprint. */
const CredentialRef: FC<{ id: string | null; credentials: ModelCredential[]; failed?: boolean; loading?: boolean }> = ({
  id,
  credentials,
  failed = false,
  loading = false,
}) => {
  const ref = credentialRef(id, credentials);
  if (!ref) return <Muted>none</Muted>;
  return (
    <>
      <div>{ref.kind ? <EntityName name={ref.name} /> : <Mono>{ref.name}</Mono>}</div>
      <Detail>
        {/* an unresolved credential is hidden from the user only when the list loaded; otherwise its details are unknown or not yet in */}
        {ref.kind ? ref.kind : failed ? 'details unavailable' : loading ? 'loading' : 'not visible to you'}
        {ref.fingerprint && (
          <>
            {' · fp '}
            <IdText id={ref.fingerprint} />
          </>
        )}
      </Detail>
    </>
  );
};

/** A table of model endpoints. */
export const EndpointTable: FC<EndpointTableProps> = ({
  items,
  credentials,
  credentialsFailed = false,
  credentialsLoading = false,
  notifications,
  loading = false,
  failed = false,
}) => {
  if (failed && items.length === 0) return null;
  if (!loading && items.length === 0) return <EmptyNote what="model endpoints" command="thorctl models endpoints" />;
  return (
    <Table data-testid="endpoint-table">
      <thead>
        <tr>
          <th>Endpoint</th>
          <th>Provider</th>
          <th>Credential</th>
          <th>Groups</th>
          <th>Models</th>
          <th>Disabled</th>
          <th>Refresh</th>
          <th>Bans</th>
        </tr>
      </thead>
      <tbody>
        {items.map((item) => {
          const meta = item.metadata.ModelEndpoint;
          return (
            <Fragment key={item.id}>
              <tr data-testid={`endpoint-row-${item.name}`}>
                <td>
                  <EntityName name={item.name} />
                </td>
                <td data-testid="endpoint-provider">
                  <div>
                    <VariantName name={variantText(meta.provider)} />
                  </div>
                  {providerTarget(meta.provider).map((target) => (
                    <Detail key={target}>{target}</Detail>
                  ))}
                </td>
                <td data-testid="endpoint-credential">
                  <CredentialRef id={meta.credential} credentials={credentials} failed={credentialsFailed} loading={credentialsLoading} />
                </td>
                <td>{item.groups.join(', ')}</td>
                <td data-testid="endpoint-models">
                  {(meta.catalog ?? []).length === 0 && <Muted>none</Muted>}
                  {(meta.catalog ?? []).map((entry) => {
                    const banned = Object.values(entry.bans ?? {});
                    return (
                      <ModelItem key={entry.model_id}>
                        <InlineList>
                          <IdText id={entry.model_id} />
                          {banned.length > 0 && (
                            <Badge
                              tone={BadgeTone.Error}
                              title={banned.map((ban) => banMessage(ban)).join('; ')}
                              data-testid="model-banned"
                            >
                              banned
                            </Badge>
                          )}
                        </InlineList>
                        {limitsParts(entry.limits).length > 0 && (
                          <Detail>
                            <LimitsText limits={entry.limits} stacked />
                          </Detail>
                        )}
                      </ModelItem>
                    );
                  })}
                </td>
                <td data-testid="endpoint-disabled-models">
                  <IdList ids={meta.disabled_models ?? []} />
                </td>
                <td>{variantText(meta.refresh)}</td>
                <td>
                  <Bans bans={meta.bans} catalog={meta.catalog} />
                </td>
              </tr>
              <Notifications items={notifications[item.id]} columns={8} testId={`endpoint-notes-${item.name}`} />
            </Fragment>
          );
        })}
      </tbody>
    </Table>
  );
};

/** Show a profile's limits and capabilities, falling back to its endpoint's catalog entry where it sets none. */
const ProfileLimits: FC<{ profile: ModelProfile; endpoint: ModelEndpoint | undefined }> = ({ profile, endpoint }) => {
  const meta = profile.metadata.ModelProfile;
  const entry = endpoint?.metadata.ModelEndpoint.catalog?.find((candidate) => candidate.model_id === meta.model_id);
  // a profile's own limits and capabilities replace the catalog entry's
  const limits = meta.limits ?? entry?.limits;
  const capabilities = capabilityNames(meta.capabilities ?? entry?.capabilities);
  const hasLimits = limitsParts(limits).length > 0;
  if (!hasLimits && capabilities.length === 0) return <>-</>;
  return (
    <>
      {hasLimits && (
        <div>
          {/* stacked, so a long value such as a 1M context window never makes this column wider than the table */}
          <LimitsText limits={limits} stacked />
        </div>
      )}
      {capabilities.length > 0 && (
        <Detail>
          {capabilities.map((name, index) => (
            <Fragment key={name}>
              {/* the separating space sits outside the nowrap span so the list can wrap between names */}
              <Capability>{index < capabilities.length - 1 ? `${name},` : name}</Capability>{' '}
            </Fragment>
          ))}
        </Detail>
      )}
    </>
  );
};

/** A table of model profiles. */
export const ProfileTable: FC<ProfileTableProps> = ({ items, endpoints, notifications, loading = false, failed = false }) => {
  if (failed && items.length === 0) return null;
  if (!loading && items.length === 0) return <EmptyNote what="model profiles" command="thorctl models profiles" />;
  return (
    <Table data-testid="profile-table">
      <thead>
        <tr>
          <th>Profile</th>
          <th>Groups</th>
          <th>Endpoint</th>
          <th>Model</th>
          <th>API</th>
          <th>Parameters</th>
          <th>Limits</th>
          <th>Bans</th>
        </tr>
      </thead>
      <tbody>
        {items.map((item) => {
          const meta = item.metadata.ModelProfile;
          const endpoint = endpoints.find((candidate) => candidate.id === meta.endpoint);
          return (
            <Fragment key={item.id}>
              <tr data-testid={`profile-row-${item.name}`}>
                <td>
                  <EntityName name={item.name} />
                </td>
                <td>{item.groups.join(', ')}</td>
                <td data-testid="profile-endpoint">{endpoint ? <EntityName name={endpoint.name} /> : <Mono>{meta.endpoint}</Mono>}</td>
                <td data-testid="profile-model">
                  <IdText id={meta.model_id} />
                </td>
                <td>{meta.api}</td>
                <td data-testid="profile-parameters">
                  <ParameterList parameters={meta.parameters} />
                </td>
                <td data-testid="profile-limits">
                  <ProfileLimits profile={item} endpoint={endpoint} />
                </td>
                <td>
                  <Bans bans={meta.bans} />
                </td>
              </tr>
              <Notifications items={notifications[item.id]} columns={8} testId={`profile-notes-${item.name}`} />
            </Fragment>
          );
        })}
      </tbody>
    </Table>
  );
};

/** Show a credential's kind with its non-secret fields (a role ARN, a tenant and client id) under it. */
const CredentialKind: FC<{ kind: unknown }> = ({ kind }) => {
  const { variant, fields }: { variant: string; fields: string[] } = taggedDetails(kind);
  return (
    <>
      <div>{variant}</div>
      {fields.map((field) => (
        <Detail key={field}>{field}</Detail>
      ))}
    </>
  );
};

/** A timestamp that stays on one line rather than wrapping at its hyphens. */
const WholeTime = styled.span`
  white-space: nowrap;
`;

/** Format a timestamp cell in the shared date and time format, or a muted `none` when there is none. */
const when = (value: string | null | undefined): ReactNode =>
  value ? (
    <WholeTime>
      <Time>{value}</Time>
    </WholeTime>
  ) : (
    <Muted>none</Muted>
  );

/** A table of model credentials; secrets are never part of the response, only the fingerprint is shown. */
export const CredentialTable: FC<TableProps<ModelCredential>> = ({ items, notifications, loading = false, failed = false }) => {
  if (failed && items.length === 0) return null;
  if (!loading && items.length === 0) return <EmptyNote what="model credentials" command="thorctl models credentials" />;
  return (
    <Table data-testid="credential-table">
      <thead>
        <tr>
          <th>Credential</th>
          <th>Groups</th>
          <th>Kind</th>
          <th>Fingerprint</th>
          <th>Rotated</th>
          <th>Expires</th>
          <th>Bans</th>
        </tr>
      </thead>
      <tbody>
        {items.map((item) => {
          const meta = item.metadata.ModelCredential;
          return (
            <Fragment key={item.id}>
              <tr data-testid={`credential-row-${item.name}`}>
                <td>
                  <EntityName name={item.name} />
                </td>
                <td>{item.groups.join(', ')}</td>
                <td data-testid="credential-kind">
                  <CredentialKind kind={meta.kind} />
                </td>
                <td data-testid="credential-fingerprint">
                  {meta.secrets?.fingerprint ? <IdText id={meta.secrets.fingerprint} /> : <Muted>none</Muted>}
                </td>
                <td>{when(meta.rotated)}</td>
                <td>{when(meta.expires)}</td>
                <td>
                  <Bans bans={meta.bans} />
                </td>
              </tr>
              <Notifications items={notifications[item.id]} columns={7} testId={`credential-notes-${item.name}`} />
            </Fragment>
          );
        })}
      </tbody>
    </Table>
  );
};
