// project imports
import { InstanceStatus, ToolSetExclusion, ToolSetExclusionReason, ToolSetInclusion } from '@models/toolsets';
import { ToolInstanceEntity, ToolVersion } from '@models/tools';
import { ModelBan } from '@models/modelconfigs';
import { AgenticKinds } from '@models/agentic';

/** The words a person uses for each kind of entity, lower case so they read inside a sentence. */
const ENTITY_KIND_NOUNS: Record<AgenticKinds, string> = {
  [AgenticKinds.Tool]: 'tool',
  [AgenticKinds.ToolInstance]: 'tool version',
  [AgenticKinds.Agent]: 'agent',
  [AgenticKinds.ToolSet]: 'toolset',
  [AgenticKinds.ToolSetInstance]: 'toolset instance',
  [AgenticKinds.WorkflowCatalog]: 'workflow',
  [AgenticKinds.WorkflowInstance]: 'workflow instance',
  [AgenticKinds.ModelEndpoint]: 'model endpoint',
  [AgenticKinds.ModelProfile]: 'model profile',
  [AgenticKinds.ModelCredential]: 'model credential',
};

/**
 * Name a kind of entity the way the pages do rather than by its API name.
 *
 * @param kind - The entity's kind as the API names it, such as `WorkflowCatalog`.
 * @returns The lower-case noun, such as `workflow`; an unknown kind is returned as it is.
 */
export function entityKindNoun(kind: string): string {
  return ENTITY_KIND_NOUNS[kind as AgenticKinds] ?? kind;
}

/**
 * Put the article that reads right before a noun in front of it.
 *
 * @param noun - The word to name, such as `agent` or `Harness`.
 * @returns The noun with `a` or `an` in front, such as `an agent` or `a Harness`.
 */
export function withArticle(noun: string): string {
  // "an" precedes a vowel sound, which every noun used here that starts with a vowel has
  return `${/^[AEIOUaeiou]/.test(noun) ? 'an' : 'a'} ${noun}`;
}

/**
 * Get the label of a tool version.
 *
 * @param version - A semantic or custom version.
 * @returns The label as it was published, or an empty string if no version is set.
 */
export function toolVersionLabel(version: ToolVersion | null | undefined): string {
  if (!version) return '';
  return 'SemVer' in version ? version.SemVer : version.Custom;
}

/**
 * Name a tool instance the way the CLI does.
 *
 * @param tool - The tool's name.
 * @param version - The instance's version.
 * @returns `<tool>@<version>`.
 */
export function toolInstanceName(tool: string, version: ToolVersion): string {
  return `${tool}@${toolVersionLabel(version)}`;
}

/**
 * Name a workflow or toolset instance the way the CLI does.
 *
 * @param root - The workflow's or toolset's name.
 * @param revision - The instance's revision.
 * @returns `<root>#r<revision>`.
 */
export function rootInstanceName(root: string, revision: number): string {
  return `${root}#r${revision}`;
}

/**
 * Tell whether a workflow or toolset instance can be used.
 *
 * @param status - The instance's status.
 * @returns `true` for a Ready instance.
 */
export function isInstanceReady(status: InstanceStatus): boolean {
  return status === 'Ready';
}

/**
 * List the reasons a workflow or toolset instance is invalid.
 *
 * @param status - The instance's status.
 * @returns The build errors, or an empty list for a Ready instance or one with no recorded reason.
 */
export function instanceStatusErrors(status: InstanceStatus): string[] {
  return status === 'Ready' ? [] : (status.Invalid?.errors ?? []);
}

/**
 * Describe a workflow or toolset instance's status as one line.
 *
 * @param status - The instance's status.
 * @returns `Ready`, or `Invalid: <errors>`.
 */
export function instanceStatusText(status: InstanceStatus): string {
  if (status === 'Ready') return 'Ready';
  const errors = status.Invalid?.errors ?? [];
  return errors.length > 0 ? `Invalid: ${errors.join('; ')}` : 'Invalid';
}

/**
 * Shorten a content hash for a table cell.
 *
 * @param hash - A hash such as `v1:2297ee7d...`; a leading `v<digits>:` scheme prefix is not counted.
 * @param length - How many characters of the digest to keep.
 * @returns The digest's first characters, without the scheme prefix.
 */
export function shortHash(hash: string, length = 12): string {
  return hash.replace(/^v\d+:/, '').slice(0, length);
}

/**
 * Describe how a tool got into a toolset instance.
 *
 * @param inclusion - The member's inclusion.
 * @returns Words for the inclusion, such as `explicit, follows latest`.
 */
export function inclusionText(inclusion: ToolSetInclusion): string {
  switch (inclusion) {
    case ToolSetInclusion.ExplicitLatest:
      return 'explicit, follows latest';
    case ToolSetInclusion.ExplicitPinned:
      return 'explicit, pinned';
    case ToolSetInclusion.Selector:
      return 'matched by selector';
    default:
      return String(inclusion);
  }
}

/**
 * Describe why a tool was left out of a toolset instance.
 *
 * @param item - The excluded tool.
 * @returns Words for the reason, such as `declares effects, toolset is read only`.
 */
export function exclusionText(item: Pick<ToolSetExclusion, 'reason' | 'missing_groups'>): string {
  switch (item.reason) {
    case ToolSetExclusionReason.NotReadOnly:
      return 'declares effects, toolset is read only';
    case ToolSetExclusionReason.NotAgentic:
      return 'not an agentic tool';
    case ToolSetExclusionReason.NoLatest:
      return 'every version is deprecated';
    case ToolSetExclusionReason.NotTaggedInAllGroups:
      return `not tagged in ${item.missing_groups.join(', ')}`;
    default:
      return String(item.reason);
  }
}

/**
 * List the bans of a model entity, newest first.
 *
 * @param bans - The entity's bans by id.
 * @returns The bans sorted by the time they were made, newest first.
 */
export function sortedBans(bans: Record<string, ModelBan> | undefined): ModelBan[] {
  return Object.values(bans ?? {}).sort((a, b) => b.time_banned.localeCompare(a.time_banned));
}

/** The order the version's Tool tags are shown in; any other Tool tag follows, alphabetically. */
const TOOL_TAG_ORDER = ['ToolId', 'ToolVersion', 'ToolKind', 'ToolSpecHash'];

/**
 * Pick a tool instance's Tool* tags in a stable order, as the API returns tags as an unordered map.
 *
 * @template T - The type of a tag's values.
 * @param tags - The instance's tags by key.
 * @returns The entries whose key starts with `Tool`, in the fixed display order then alphabetically.
 */
export function sortedToolTags<T>(tags: Record<string, T>): [string, T][] {
  // keys outside the fixed order sort after it
  const rank = (key: string) => {
    const index = TOOL_TAG_ORDER.indexOf(key);
    return index < 0 ? TOOL_TAG_ORDER.length : index;
  };
  return Object.entries(tags)
    .filter(([key]) => key.startsWith('Tool'))
    .sort(([a], [b]) => rank(a) - rank(b) || a.localeCompare(b));
}

/**
 * Describe a ban's kind in one short phrase.
 *
 * @param ban - The ban to describe.
 * @returns The ban kind's variant name, or a generic label when it can't be read.
 */
export function banKindText(ban: ModelBan): string {
  const kind = ban.ban_kind;
  if (typeof kind === 'string') return kind;
  const keys = Object.keys(kind ?? {});
  return keys.length > 0 ? keys[0] : 'Ban';
}

/**
 * Reduce a markdown description to a short plain-text summary for list rows.
 *
 * Skips leading rules and headings, keeps the first paragraph, strips inline markdown and HTML,
 * and cuts the result to a length at a word boundary.
 *
 * @param description - The description as written, possibly markdown.
 * @param max - The most characters to keep before an ellipsis.
 * @returns The plain-text summary, or an empty string when there is no text.
 */
export function summarizeDescription(description: string | null | undefined, max = 200): string {
  if (!description) return '';
  const paragraph: string[] = [];
  for (const raw of description.split(/\r?\n/)) {
    const line = raw.trim();
    // rules and headings separate sections, so they end a paragraph that has started
    const separator = /^([-*_]{3,}|#{1,6}(\s|$))/.test(line);
    if (line === '' || separator) {
      if (paragraph.length > 0) break;
      continue;
    }
    paragraph.push(line.replace(/^([-*+]|\d+\.)\s+/, ''));
  }
  const text = paragraph
    .join(' ')
    // images vanish and links keep their text
    .replace(/!\[[^\]]*\]\([^)]*\)/g, '')
    .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/<[^>]*>/g, '')
    .replace(/&nbsp;/g, ' ')
    .replace(/&[a-z]+;|&#\d+;/gi, '')
    .replace(/`|\*\*|\*/g, '')
    .replace(/\s+/g, ' ')
    .trim();
  if (text.length <= max) return text;
  // cut at the last space before the limit so a word is never split
  const cut = text.slice(0, max);
  const space = cut.lastIndexOf(' ');
  return `${(space > max / 2 ? cut.slice(0, space) : cut).replace(/[\s,;:.-]+$/, '')}…`;
}

/**
 * Count something with its noun in the right number.
 *
 * @param count - How many there are.
 * @param singular - The noun for exactly one.
 * @param plural - The noun for any other count; defaults to the singular with an `s`.
 * @returns The count and the noun, such as `1 tool` or `2 tools`.
 */
export function countLabel(count: number, singular: string, plural = `${singular}s`): string {
  return `${count} ${count === 1 ? singular : plural}`;
}

/**
 * Get the message a ban carries.
 *
 * @param ban - The ban to read.
 * @returns The hand-written message, the provider error, or the referenced id, depending on the kind; the kind name when it carries none.
 */
export function banMessage(ban: ModelBan): string {
  const kind = ban.ban_kind;
  if (typeof kind === 'string' || !kind) return banKindText(ban);
  const detail = Object.values(kind)[0];
  if (typeof detail === 'string') return `${banKindText(ban)}: ${detail}`;
  if (detail && typeof detail === 'object') {
    const { status, message } = detail as { status?: unknown; message?: unknown };
    const parts = [status, message].filter((part) => typeof part === 'string' || typeof part === 'number');
    if (parts.length > 0) return `${banKindText(ban)}: ${parts.join(' ')}`;
  }
  return banKindText(ban);
}

/**
 * Describe a spec's portability as one line per model API family, so the reader can tell which flag is false and which notes belong to it.
 *
 * @param portability - The portability report of a tool version.
 * @returns A line for strict mode and a line for Gemini, each naming whether the spec is compatible and listing its notes.
 */
export function portabilityLines(portability: NonNullable<ToolInstanceEntity['portability']>): { strict: string; gemini: string } {
  const line = (name: string, compatible: boolean, label: string, notes: string[]) =>
    `${name}: ${compatible ? 'compatible' : 'not compatible'}${notes.length > 0 ? ` (${label}: ${notes.join('; ')})` : ''}`;
  return {
    strict: line('Strict', portability.strict_compatible, 'issues', portability.strict_issues),
    gemini: line('Gemini', portability.gemini_compatible, 'rewrites', portability.gemini_rewrites),
  };
}

/**
 * Explain what a tool delete already removed before the refusal that stopped it.
 *
 * Deleting a tool removes its versions one at a time, so a refusal part way leaves the earlier ones gone.
 * That outcome comes first, on its own line, so it is not read as part of the API error and its trace id.
 *
 * @param refusal - The error the API gave for the version it refused to delete.
 * @param deleted - The versions deleted before the refusal.
 * @returns The refusal alone when nothing was deleted, otherwise the versions already gone, a line break, and the refusal.
 */
export function partialDeleteMessage(refusal: string, deleted: ToolVersion[]): string {
  if (deleted.length === 0) return refusal;
  const labels = deleted.map((version) => toolVersionLabel(version)).join(', ');
  const outcome = `The tool was not deleted, but ${deleted.length === 1 ? 'version' : 'versions'} ${labels} ${deleted.length === 1 ? 'was' : 'were'} already deleted.`;
  return `${outcome}\n${refusal}`;
}

/** The longest identifier, in characters, that a table cell keeps on one line even when its column is narrow. */
export const SHORT_NAME_LIMIT = 28;

/**
 * Decide whether an identifier is short enough to stay whole (never split at a hyphen) in a table cell.
 *
 * @param name - The identifier to measure, such as an agent or tool name.
 * @returns True when the name is at most {@link SHORT_NAME_LIMIT} characters long.
 */
export function isShortName(name: string): boolean {
  return name.length <= SHORT_NAME_LIMIT;
}

/**
 * Decide whether an entity read that failed with an HTTP status may work if it is tried again.
 *
 * A request the API rejected as malformed or refused outright (a bad id, 401, 403, 404) fails the same way every time, so it is
 * reported as a missing entity with no Retry. No response at all (status 0), a timeout, a rate limit, and server errors may clear.
 *
 * @param status - The HTTP status of the failed read, or 0 when the server gave no response.
 * @returns True when the status is 0, 408, 429, or a server error (500 and above).
 */
export function isRetryableReadStatus(status: number): boolean {
  return status === 0 || status === 408 || status === 429 || status >= 500;
}
