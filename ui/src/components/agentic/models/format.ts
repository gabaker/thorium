import { ModelBan, ModelCapabilities, ModelCatalogEntry, ModelCredential, ModelLimits } from '@models/modelconfigs';

/**
 * Describe a refresh state or credential kind that may be a bare string or a tagged object.
 *
 * @param value - The serialized value.
 * @returns The variant name.
 */
export function variantText(value: unknown): string {
  if (typeof value === 'string') return value;
  if (value && typeof value === 'object') {
    // a refresh status carries its name in `state`; other tagged values use their one key
    const state = (value as Record<string, unknown>)['state'];
    return typeof state === 'string' ? state : (Object.keys(value)[0] ?? '');
  }
  return '';
}

/**
 * Count the bans on a model entity.
 *
 * @param bans - The bans by id.
 * @returns How many bans there are.
 */
export function banCount(bans: Record<string, ModelBan> | undefined): number {
  return Object.keys(bans ?? {}).length;
}

/** A ban on one catalog model of an endpoint. */
export interface CatalogBan {
  /// The model the ban is on
  modelId: string;
  /// The ban
  ban: ModelBan;
}

/**
 * List the bans on an endpoint's catalog models, which are kept apart from the endpoint's own bans.
 *
 * @param catalog - The endpoint's catalog.
 * @returns One entry per ban, in catalog order.
 */
export function catalogBans(catalog: ModelCatalogEntry[] | undefined): CatalogBan[] {
  return (catalog ?? []).flatMap((entry) => Object.values(entry.bans ?? {}).map((ban) => ({ modelId: entry.model_id, ban })));
}

/**
 * Describe a scalar or nested value from a serialized model setting as short text.
 *
 * @param value - The value to describe.
 * @returns Strings, numbers, and booleans as text; anything nested as JSON.
 */
function scalarText(value: unknown): string {
  if (typeof value === 'string') return value;
  if (typeof value === 'number' || typeof value === 'boolean') return String(value);
  return JSON.stringify(value);
}

/**
 * List the set fields of a tagged value (a provider or a credential kind).
 *
 * @param value - The serialized tagged value, `{ Variant: { field: ... } }` or a bare variant name.
 * @returns The variant name and its non-empty fields as `name: value` text.
 */
export function taggedDetails(value: unknown): { variant: string; fields: string[] } {
  const variant = variantText(value);
  const inner = value && typeof value === 'object' ? (value as Record<string, unknown>)[variant] : undefined;
  if (!inner || typeof inner !== 'object') return { variant, fields: [] };
  const fields = Object.entries(inner as Record<string, unknown>)
    // unset fields and empty maps or lists carry nothing worth showing
    .filter(([, field]) => field !== null && field !== undefined && !(typeof field === 'object' && Object.keys(field).length === 0))
    .map(([name, field]) => `${name}: ${scalarText(field)}`);
  return { variant, fields };
}

/**
 * Get the address an endpoint's provider is called at.
 *
 * @param provider - The endpoint's serialized provider.
 * @returns The provider's base URL or endpoint URL when it has one, else its other settings as text.
 */
export function providerTarget(provider: unknown): string[] {
  const variant = variantText(provider);
  const inner = provider && typeof provider === 'object' ? (provider as Record<string, unknown>)[variant] : undefined;
  if (!inner || typeof inner !== 'object') return [];
  const fields = inner as Record<string, unknown>;
  // the first of these that is set is where requests go
  for (const key of ['base_url', 'endpoint_url', 'endpoint']) {
    if (typeof fields[key] === 'string') return [fields[key]];
  }
  return taggedDetails(provider).fields;
}

/**
 * Describe token limits as the separate `label value` parts they are made of.
 *
 * @param limits - The limits, if any are known.
 * @returns For example `['context 128000', 'output 8192']`, or an empty list when none are set.
 */
export function limitsParts(limits: ModelLimits | null | undefined): string[] {
  const parts: string[] = [];
  if (limits?.context_window != null) parts.push(`context ${limits.context_window}`);
  if (limits?.max_output_tokens != null) parts.push(`output ${limits.max_output_tokens}`);
  return parts;
}

/**
 * The longest `label value` pair kept on one line, which is `context 128000`: a pair for a 1M context window
 * (`context 1000000`) is one character longer and may break between its label and its value.
 */
const MAX_WHOLE_LIMIT = 'context 128000'.length;

/**
 * Whether a `label value` pair from `limitsParts` is too long to keep on one line in a tight table.
 *
 * @param part - A pair such as `context 1000000`.
 * @returns True when the pair is longer than `MAX_WHOLE_LIMIT` characters.
 */
export function isLongLimit(part: string): boolean {
  return part.length > MAX_WHOLE_LIMIT;
}

/**
 * Describe token limits in one short phrase.
 *
 * @param limits - The limits, if any are known.
 * @returns For example `context 128000 · output 8192`, or an empty string when none are set.
 */
export function limitsText(limits: ModelLimits | null | undefined): string {
  return limitsParts(limits).join(' · ');
}

/**
 * List the capabilities a model has.
 *
 * @param capabilities - The capability flags.
 * @returns The names of the flags that are set, in the order the API sends them.
 */
export function capabilityNames(capabilities: ModelCapabilities | null | undefined): string[] {
  return Object.entries(capabilities ?? {})
    .filter(([, enabled]) => enabled)
    .map(([name]) => name);
}

/** One parameter of a model profile, with its value described as text. */
export interface ParameterEntry {
  /** The parameter's name, for example `max_output_tokens` */
  name: string;
  /** The parameter's value as text */
  value: string;
}

/**
 * List a profile's set parameters.
 *
 * @param parameters - The profile's parameters.
 * @returns One entry per parameter, in the order the API sends them; unset and empty parameters are left out.
 */
export function parameterEntries(parameters: Record<string, unknown> | null | undefined): ParameterEntry[] {
  return Object.entries(parameters ?? {})
    .filter(([, value]) => value !== null && value !== undefined && !(Array.isArray(value) && value.length === 0))
    .map(([name, value]) => ({ name, value: scalarText(value) }));
}

/**
 * Describe a profile's parameters in one short phrase.
 *
 * @param parameters - The profile's parameters.
 * @returns For example `temperature 0.2, max_output_tokens 512`; unset and empty parameters are left out.
 */
export function parametersText(parameters: Record<string, unknown> | null | undefined): string {
  return parameterEntries(parameters)
    .map(({ name, value }) => `${name} ${value}`)
    .join(', ');
}

/**
 * Describe the credential an endpoint authenticates with.
 *
 * @param id - The credential id the endpoint references, if any.
 * @param credentials - The credentials the user can see.
 * @returns The credential's name, kind and fingerprint, or just the id when the credential isn't visible; null when the endpoint has none.
 */
export function credentialRef(
  id: string | null | undefined,
  credentials: ModelCredential[],
): { name: string; kind: string; fingerprint: string } | null {
  if (!id) return null;
  const found = credentials.find((credential) => credential.id === id);
  if (!found) return { name: id, kind: '', fingerprint: '' };
  const meta = found.metadata.ModelCredential;
  return { name: found.name, kind: variantText(meta.kind), fingerprint: meta.secrets?.fingerprint ?? '' };
}

/**
 * List the model ids an endpoint serves.
 *
 * @param catalog - The endpoint's catalog.
 * @returns The model ids in catalog order.
 */
export function catalogIds(catalog: { model_id: string }[] | undefined): string[] {
  return (catalog ?? []).map((entry) => entry.model_id);
}

/** The fewest characters a final piece of an id or entity name holds; a shorter tail stays attached to the piece before it. */
export const MIN_TAIL = 4;

/** The characters after which a model id or fingerprint may wrap onto a new line. */
const ID_SEPARATORS = /(?<=[.:/_@-])/;

/**
 * Split a model id or fingerprint into the pieces it may wrap between, each piece ending with its separator.
 *
 * Ids such as `us.anthropic.claude-sonnet-4-5-20250929-v1:0` have no spaces, so a table either widens to fit them
 * whole or breaks them at an arbitrary letter; wrapping only after a separator keeps every word of the id intact.
 * A final piece shorter than `MIN_TAIL` stays with the piece before it, so `...-v1:0` never leaves a lone `0` on a line.
 *
 * @param id - The model id or fingerprint.
 * @returns The pieces of the id in order; joined they give the id back.
 */
export function idSegments(id: string): string[] {
  const segments = id.split(ID_SEPARATORS).filter((segment) => segment.length > 0);
  // a short tail rejoins the piece before it instead of standing alone
  if (segments.length > 1 && segments[segments.length - 1].length < MIN_TAIL) {
    const tail = segments.pop() ?? '';
    segments[segments.length - 1] += tail;
  }
  return segments;
}

/**
 * Split a CamelCase variant name such as `OpenAiCompatible` into its words.
 *
 * A provider's name is one long word to the browser, which would keep it on a line of its own and so
 * set the narrowest its table column can be; the words let it wrap between them.
 *
 * @param name - The variant name.
 * @returns The words of the name in order; joined they give the name back.
 */
export function camelWords(name: string): string[] {
  // a break falls where a lower case letter or digit meets an upper case letter
  return name.split(/(?<=[a-z0-9])(?=[A-Z])/);
}

/** The fewest characters a chunk of an entity name holds before the name may wrap after it. */
const MIN_NAME_CHUNK = 7;

/** The most characters a chunk of an entity name grows to by absorbing further pieces; a single longer piece is a chunk of its own. */
const MAX_NAME_CHUNK = 10;

/**
 * The most characters a chunk of an entity name stays whole at on every viewport, with or without a separator
 * (`-`, `_` or `.`). A word such as `summarization` or `observability-` fits its column even when the eight-column
 * Profiles and Endpoints tables are squeezed into a 1024 pixel page with every other column at its narrowest; a
 * longer word there would hold its column wide enough to push the table past its panel.
 */
const MAX_WHOLE_NAME_CHUNK = 14;

/**
 * The most characters a chunk of an entity name stays whole at on a wide viewport. A longer chunk breaks mid-word at
 * every width, since no column has room for it; a chunk between this and `MAX_WHOLE_NAME_CHUNK` breaks only on a
 * narrow viewport (see `isSqueezableNameChunk`).
 */
const MAX_ROOMY_NAME_CHUNK = 24;

/** The fewest characters a chunk holds when it is closed early because absorbing the next piece would exceed `MAX_NAME_CHUNK`. */
const MIN_NAME_BREAK = 3;

/**
 * Split an entity name into the chunks it may wrap between.
 *
 * A name breaks only after a `-`, `_` or `.`, and only once the chunk so far is at least `MIN_NAME_CHUNK`
 * characters long, and a trailing piece shorter than `MIN_TAIL` stays with the chunk before it, so a name
 * such as `jup-mock-endpoint-a` wraps as `jup-mock-` / `endpoint-a` and never leaves a single character alone
 * on a line. A tail that would take that chunk past `MAX_NAME_CHUNK` instead takes the chunk's last piece
 * with it (`fast-tier-` / `v2` becomes `fast-` / `tier-v2`), or stands alone when that chunk is a single piece
 * (`observability-` / `key`), so a short tail never makes a chunk wider than the cap. A chunk shorter than
 * `MIN_NAME_CHUNK` stops growing, once it has `MIN_NAME_BREAK` characters, when the next piece would take it past
 * `MAX_NAME_CHUNK`: each chunk is kept whole on one line, so a chunk made wide by short hyphen pieces
 * (`by-ratelimit`) would widen its table column past the panel. A short name is a single chunk.
 *
 * @param name - The entity's name.
 * @returns The chunks of the name in order; joined they give the name back.
 */
export function nameChunks(name: string): string[] {
  const chunks: string[] = [];
  let current = '';
  // split after each separator, then group the pieces into chunks that are long enough to stand alone
  for (const piece of name.split(/(?<=[-_.])/).filter((candidate) => candidate.length > 0)) {
    // close the chunk early when the next piece would make it too wide to keep on one line
    if (current.length >= MIN_NAME_BREAK && current.length + piece.length > MAX_NAME_CHUNK) {
      chunks.push(current);
      current = '';
    }
    current += piece;
    if (current.length >= MIN_NAME_CHUNK) {
      chunks.push(current);
      current = '';
    }
  }
  // a short tail rejoins the previous chunk instead of standing alone, unless the chunk would grow too wide to keep whole
  if (current.length > 0) {
    const previous = chunks.length > 0 ? chunks[chunks.length - 1] : '';
    if (previous.length > 0 && current.length < MIN_TAIL) {
      if (previous.length + current.length <= MAX_NAME_CHUNK) {
        // the tail fits behind the previous chunk without making it wider than the cap
        chunks[chunks.length - 1] += current;
      } else {
        // the previous chunk is too wide to take the tail, so its last piece moves down to keep the tail company
        const pieces = previous.split(/(?<=[-_.])/).filter((candidate) => candidate.length > 0);
        const moved = pieces.length > 1 ? (pieces.pop() ?? '') : '';
        if (moved.length > 0) chunks[chunks.length - 1] = pieces.join('');
        // a lone character never stands on a line of its own, even when its chunk is a single wide piece
        if (moved.length === 0 && current.length === 1) chunks[chunks.length - 1] += current;
        else chunks.push(moved + current);
      }
    } else {
      chunks.push(current);
    }
  }
  return chunks;
}

/**
 * Whether a chunk of an entity name is too long to stay whole at any width, so it may break mid-word.
 *
 * A chunk past `MAX_ROOMY_NAME_CHUNK` characters is wider than any column has room for, with or without a separator.
 *
 * @param chunk - A chunk of an entity name from `nameChunks`.
 * @returns Whether the chunk may break at any character.
 */
export function isLongNameChunk(chunk: string): boolean {
  // a chunk past the wide-viewport limit breaks at every width
  return chunk.length > MAX_ROOMY_NAME_CHUNK;
}

/**
 * Whether a chunk of an entity name is an ordinary word that stays whole where there is room and may break mid-word
 * only where the page is narrow.
 *
 * Such a chunk passes `MAX_WHOLE_NAME_CHUNK` characters without being long enough for `isLongNameChunk`
 * (`classification-eu`, `internationalization`). A table of several of them can be wider than its panel on a narrow
 * viewport, where breaking one as a last resort keeps the table inside the panel. On a wide viewport the chunk stays
 * whole, since a table column that is allowed to break takes less than the room it has.
 *
 * @param chunk - A chunk of an entity name from `nameChunks`.
 * @returns Whether the chunk may break at any character on a narrow viewport only.
 */
export function isSqueezableNameChunk(chunk: string): boolean {
  // a chunk past the every-width limit but not so long that it breaks at every width
  return chunk.length > MAX_WHOLE_NAME_CHUNK && !isLongNameChunk(chunk);
}

/**
 * Split an overlong name chunk into the body that may break anywhere and the tail that stays on one line, so a
 * break never strands a separator or a single character on a line of its own.
 *
 * When fewer than `MIN_TAIL` characters follow the chunk's last separator, the tail is the letter before that
 * separator with everything after it (`y-` of `observability-`, `s-b` of `...credentials-b`, `d--` of `word--`),
 * so no line starts with a separator. Otherwise, including a chunk with no separator, the tail is the chunk's last
 * `MIN_TAIL` characters (`nabc` of `summarizationabc`).
 *
 * @param chunk - A chunk of an entity name that is too long to stay whole.
 * @returns The breakable body, which may be empty, and the tail to keep on one line; joined they give the chunk back.
 */
export function splitLongNameChunk(chunk: string): [string, string] {
  // find the run of separators nearest the end, if only a short tail follows it
  const separators = /[-_.]+(?=[^-_.]*$)/.exec(chunk);
  const followed = separators ? chunk.length - separators.index - separators[0].length : chunk.length;
  // the tail starts one letter before that run, or `MIN_TAIL` characters before the end
  const start = separators && followed < MIN_TAIL ? Math.max(separators.index - 1, 0) : Math.max(chunk.length - MIN_TAIL, 0);
  return [chunk.slice(0, start), chunk.slice(start)];
}

/**
 * Split a ban kind such as `ModelRemoved` into its words, so a badge can wrap between them.
 *
 * The kind is cut before each capital letter that follows a lowercase letter or a digit, so `EndpointUnreachable`
 * gives `Endpoint` and `Unreachable` and `NotAllowedByDeployment` gives four words; a kind in one word stays whole.
 *
 * @param kind - The ban kind.
 * @returns The words of the kind in order; joined they give the kind back.
 */
export function kindWords(kind: string): string[] {
  return kind.split(/(?<=[a-z0-9])(?=[A-Z])/).filter((word) => word.length > 0);
}

/** The fewest characters a chunk of a parameter name holds before the name may wrap after it. */
const MIN_PARAMETER_CHUNK = 5;

/**
 * Split a parameter name into the chunks it may wrap between.
 *
 * A name breaks only after an underscore, and only once the chunk so far is at least `MIN_PARAMETER_CHUNK`
 * characters long, so `max_output_tokens` wraps as `max_output_` / `tokens` rather than leaving `max_` alone
 * on a line, and `stop_sequences` wraps as `stop_` / `sequences` so it cannot widen a narrow column. A trailing
 * piece shorter than `MIN_TAIL` stays with the chunk before it.
 *
 * @param name - The parameter's name.
 * @returns The chunks of the name in order; joined they give the name back.
 */
export function parameterNameChunks(name: string): string[] {
  const chunks: string[] = [];
  let current = '';
  // group the pieces between underscores into chunks that are long enough to stand alone
  for (const piece of name.split(/(?<=_)/).filter((candidate) => candidate.length > 0)) {
    current += piece;
    if (current.length >= MIN_PARAMETER_CHUNK) {
      chunks.push(current);
      current = '';
    }
  }
  // a short tail rejoins the previous chunk instead of standing alone
  if (current.length > 0) {
    if (chunks.length > 0 && current.length < MIN_TAIL) chunks[chunks.length - 1] += current;
    else chunks.push(current);
  }
  return chunks;
}

/** One run of a message: plain text, or text that was set in backticks. */
export interface MessagePart {
  /** The text of the run, without its backticks */
  text: string;
  /** Whether the run was set in backticks and should be shown as code */
  code: boolean;
}

/**
 * Split a message on its backtick quotes so quoted runs can be shown as code instead of with raw backticks.
 *
 * @param message - The message, for example ``model `gpt-x` is banned``.
 * @returns The runs of the message in order; an unmatched backtick stays as plain text.
 */
export function messageParts(message: string): MessagePart[] {
  const parts: MessagePart[] = [];
  // a quoted run is a pair of backticks around at least one character
  const pattern = /`([^`]+)`/g;
  let last = 0;
  for (const match of message.matchAll(pattern)) {
    if (match.index > last) parts.push({ text: message.slice(last, match.index), code: false });
    parts.push({ text: match[1], code: true });
    last = match.index + match[0].length;
  }
  if (last < message.length) parts.push({ text: message.slice(last), code: false });
  return parts;
}
