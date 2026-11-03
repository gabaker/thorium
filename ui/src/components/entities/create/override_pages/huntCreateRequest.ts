// project imports
import { Hunt, HuntCreateRequest, HuntCorpusBoundsRequest, HuntEngine, HuntSelector, HUNT_ENGINES } from '@models/entities/hunts';
import { requestTagsToTagEntryList, tagEntriesToRequestTags } from '@utilities/tags';

/**
 * The selector shapes the create form can build, in display order.
 */
export enum HuntSelectorMode {
  Rules = 'Rules',
  Tag = 'Tag',
  All = 'All',
  RuleSet = 'RuleSet',
}

/**
 * One corpus tag filter row as the dictionary input produces it.
 */
export type HuntCorpusTagEntry = {
  key: string;
  value: string;
};

/**
 * The hunt create form's state: everything the page collects before building the request.
 */
export type HuntCreateFormState = {
  name: string;
  groups: string[];
  engine: HuntEngine;
  selectorMode: HuntSelectorMode;
  // the explicit rule ids when the mode is Rules
  selectorRules: string[];
  // the tag pair when the mode is Tag
  selectorTagKey: string;
  selectorTagValue: string;
  // the rule set id when the mode is RuleSet
  selectorRuleSet: string;
  // optional corpus bounds; empty values are omitted so the server defaults apply
  corpusGroups: string[];
  corpusStart: string;
  corpusEnd: string;
  corpusTags: HuntCorpusTagEntry[];
  corpusCaseInsensitive: boolean;
  description: string;
};

export const BlankHuntCreateFormState: HuntCreateFormState = {
  name: '',
  groups: [],
  engine: HuntEngine.Yara,
  selectorMode: HuntSelectorMode.Rules,
  selectorRules: [],
  selectorTagKey: '',
  selectorTagValue: '',
  selectorRuleSet: '',
  corpusGroups: [],
  corpusStart: '',
  corpusEnd: '',
  corpusTags: [],
  corpusCaseInsensitive: false,
  description: '',
};

/**
 * Build the selector union shape for the chosen mode.
 *
 * @param state - The form state to read the mode's inputs from.
 * @returns The externally tagged selector (`All` is a bare string on the wire).
 */
function buildSelector(state: HuntCreateFormState): HuntSelector {
  switch (state.selectorMode) {
    case HuntSelectorMode.Rules:
      // dedupe while preserving pick order
      return { Rules: [...new Set(state.selectorRules)] };
    case HuntSelectorMode.Tag:
      // trim like every other tag input in the form; pasted whitespace would match
      // nothing and draw an opaque "resolved to 0 usable rules" 400
      return { Tag: { key: state.selectorTagKey.trim(), value: state.selectorTagValue.trim() } };
    case HuntSelectorMode.All:
      return 'All';
    case HuntSelectorMode.RuleSet:
      return { RuleSet: state.selectorRuleSet };
  }
}

/**
 * Fold the dictionary rows into the strict-AND tag filter map, skipping blank pairs.
 *
 * Delegates to the shared tag helper in trim mode so the prototype-key handling lives
 * in exactly one place.
 *
 * @param entries - The dictionary rows as entered.
 * @returns The key -> values map the corpus bounds expect.
 */
function foldCorpusTags(entries: HuntCorpusTagEntry[]): Record<string, string[]> {
  return tagEntriesToRequestTags(entries, true);
}

/**
 * Validate the selector shapes the server only rejects with an opaque error.
 *
 * Everything else (empty name, no groups, ...) draws a usable server message, so only
 * the two guaranteed-bad selector shapes are blocked client-side.
 *
 * @param state - The form state to validate.
 * @returns A user-facing message describing the problem, or an empty string when postable.
 */
export function validateHuntForm(state: HuntCreateFormState): string {
  // a Tag selector with a blank key can only ever resolve to zero rules
  if (state.selectorMode === HuntSelectorMode.Tag && state.selectorTagKey.trim() === '') {
    return 'A rules-by-tag hunt requires a tag key.';
  }
  // a RuleSet selector with no pick would post an empty selector object (a serde 422)
  if (state.selectorMode === HuntSelectorMode.RuleSet && !state.selectorRuleSet) {
    return 'A rule set hunt requires a rule set to be selected.';
  }
  return '';
}

/**
 * Render a date bound for the corpus summary, falling back to the raw string.
 *
 * @param stamp - The ISO timestamp the form holds.
 * @returns The locale rendering of the bound, or the raw string when unparseable.
 */
function describeCorpusInstant(stamp: string): string {
  // render like the details page's lane times, but never trust the string to parse
  const date = new Date(stamp);
  return Number.isNaN(date.getTime()) ? stamp : date.toLocaleString();
}

/**
 * Describe the corpus bounds the form would actually post, for the collapsed panel summary.
 *
 * The summary must never claim the server defaults while prefilled bounds (the copy/re-run
 * flow) would silently narrow the corpus, so each set bound is surfaced from its value.
 *
 * @param state - The form state to describe.
 * @returns A summary of every set bound, or an empty string when the server defaults apply.
 */
export function describeCorpusBounds(state: HuntCreateFormState): string {
  const parts: string[] = [];
  // surface each bound that buildHuntCreateRequest would send
  if (state.corpusGroups.length > 0) {
    parts.push(`groups: ${state.corpusGroups.join(', ')}`);
  }
  if (state.corpusStart !== '') {
    parts.push(`newest: ${describeCorpusInstant(state.corpusStart)}`);
  }
  if (state.corpusEnd !== '') {
    parts.push(`oldest: ${describeCorpusInstant(state.corpusEnd)}`);
  }
  // count the tag pairs that survive the fold (blank rows never post)
  const tagCount = Object.values(foldCorpusTags(state.corpusTags)).reduce((count, values) => count + values.length, 0);
  if (tagCount > 0) {
    parts.push(`${tagCount} tag filter${tagCount === 1 ? '' : 's'}${state.corpusCaseInsensitive ? ' (case-insensitive)' : ''}`);
  }
  return parts.length === 0 ? '' : `Corpus bounded — ${parts.join('; ')}`;
}

/**
 * Build the JSON body for `POST /entities/hunt/` from the create form's state.
 *
 * Untouched corpus bounds omit the whole `corpus` field so the server defaults the window
 * to everything up to creation; within a touched corpus only the set bounds are sent.
 *
 * @param state - The complete form state.
 * @returns The request body to POST.
 */
export function buildHuntCreateRequest(state: HuntCreateFormState): HuntCreateRequest {
  const request: HuntCreateRequest = {
    name: state.name,
    groups: state.groups,
    engine: state.engine,
    selector: buildSelector(state),
  };
  // assemble the corpus only from bounds the user actually set
  const bounds: HuntCorpusBoundsRequest = {};
  // never post a corpus group outside the hunt's groups: the API hard-400s on one, so a
  // pick that outlived a groups change is dropped rather than sent
  const corpusGroups = state.corpusGroups.filter((group) => state.groups.includes(group));
  if (corpusGroups.length > 0) {
    bounds.groups = corpusGroups;
  }
  if (state.corpusStart !== '') {
    bounds.start = state.corpusStart;
  }
  if (state.corpusEnd !== '') {
    bounds.end = state.corpusEnd;
  }
  const tags = foldCorpusTags(state.corpusTags);
  if (Object.keys(tags).length > 0) {
    bounds.tags = tags;
    if (state.corpusCaseInsensitive) {
      bounds.tags_case_insensitive = true;
    }
  }
  if (Object.keys(bounds).length > 0) {
    request.corpus = { Files: bounds };
  }
  if (state.description !== '') {
    request.description = state.description;
  }
  return request;
}

/**
 * Prefill the create form from an existing hunt (the details copy button's re-run flow).
 *
 * The stored selector maps back onto its form mode; corpus bounds copy verbatim so the
 * re-run covers the same window unless the user widens it.
 *
 * @param hunt - The hunt to copy.
 * @returns A form state mirroring the hunt's create-time inputs.
 */
export function huntToFormState(hunt: Hunt): HuntCreateFormState {
  const meta = hunt.metadata.Hunt;
  const state = structuredClone(BlankHuntCreateFormState);
  state.name = `${hunt.name} - rerun`;
  state.groups = [...hunt.groups];
  // a hunt from a newer server can carry an engine this build does not know; normalize it
  // to the default engine so the page can never crash on an unregistered engine
  const knownEngine = Object.hasOwn(HUNT_ENGINES, meta.engine);
  state.engine = knownEngine ? meta.engine : BlankHuntCreateFormState.engine;
  state.description = hunt.description ?? '';
  // map the stored selector back onto its form mode, narrowing every known variant
  // explicitly: a variant from a newer server falls back to All, the one mode that is
  // always valid to submit, instead of a RuleSet mode with nothing picked
  if (typeof meta.selector === 'string') {
    state.selectorMode = HuntSelectorMode.All;
  } else if ('Rules' in meta.selector) {
    state.selectorMode = HuntSelectorMode.Rules;
    state.selectorRules = [...meta.selector.Rules];
  } else if ('Tag' in meta.selector) {
    state.selectorMode = HuntSelectorMode.Tag;
    state.selectorTagKey = meta.selector.Tag.key;
    state.selectorTagValue = meta.selector.Tag.value;
  } else if ('RuleSet' in meta.selector) {
    state.selectorMode = HuntSelectorMode.RuleSet;
    state.selectorRuleSet = meta.selector.RuleSet;
  } else {
    state.selectorMode = HuntSelectorMode.All;
  }
  // an unknown engine's rule picks are foreign to the normalized engine; drop them so
  // stale foreign candidates are never prefilled into the pickers
  if (!knownEngine) {
    state.selectorRules = [];
    state.selectorRuleSet = '';
  }
  // copy the corpus bounds verbatim (Files is the only kind the UI offers)
  if ('Files' in meta.corpus) {
    state.corpusGroups = [...meta.corpus.Files.groups];
    state.corpusStart = meta.corpus.Files.start;
    state.corpusEnd = meta.corpus.Files.end;
    state.corpusTags = requestTagsToTagEntryList(meta.corpus.Files.tags);
    state.corpusCaseInsensitive = meta.corpus.Files.tags_case_insensitive;
  }
  return state;
}
