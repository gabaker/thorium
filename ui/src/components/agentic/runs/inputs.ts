// project imports
import { HashCheck, hashChecksBlocker } from '@utilities/hashes';
import { buildSampleForm, inputsProblem, parseRepoInput, promptFile, sampleHashProblem } from '@utilities/runs';
import { CommitishKinds, RepoDependencyRequest } from '@models/reactions';

/** Which inputs a run is given besides its prompt: samples, a repo, or both. */
export enum InputsMode {
  Files = 'files',
  Repos = 'repos',
  Both = 'both',
}

/** The raw values of the run inputs form. */
export interface InputsState {
  /** Which input sections are shown and sent; values typed into a hidden section are kept but not sent */
  mode: InputsMode;
  /** Prompt text, uploaded as `prompt.txt` */
  prompt: string;
  /** Local files to upload as samples */
  files: File[];
  /** Existing sample SHA256s, as entered tokens */
  samples: string[];
  /** A repo as `<url>[:<commitish>]` */
  repo: string;
  /** The kind of commitish when the repo names one */
  repoKind: CommitishKinds;
}

/** The blank inputs form. */
export const BLANK_INPUTS: InputsState = {
  mode: InputsMode.Files,
  prompt: '',
  files: [],
  samples: [],
  repo: '',
  repoKind: CommitishKinds.Branch,
};

/**
 * Whether a mode shows (and sends) the samples section.
 *
 * @param mode - The inputs mode.
 * @returns `true` for Files and Both.
 */
export const showsFiles = (mode: InputsMode): boolean => mode !== InputsMode.Repos;

/**
 * Whether a mode shows (and sends) the repo section.
 *
 * @param mode - The inputs mode.
 * @returns `true` for Repos and Both.
 */
export const showsRepos = (mode: InputsMode): boolean => mode !== InputsMode.Files;

/**
 * Build the starting inputs from the launch page's URL.
 *
 * A page that links here can name samples (`sample`, repeatable), a repo (`repo`, optionally with `repo_kind`), and the
 * mode (`inputs`). Without a mode, a link that names only a repo opens the Repos mode, one that names both opens Both,
 * and anything else opens Files.
 *
 * @param params - The launch page's URL parameters.
 * @returns The inputs to start the form with.
 */
export function initialInputs(params: URLSearchParams): InputsState {
  // a SHA256 names one sample however its case is written, so each is kept once, lowercased
  const samples = [
    ...new Set(
      params
        .getAll('sample')
        .flatMap((entry) => entry.split(',').map((sample) => sample.trim().toLowerCase()))
        .filter((sample) => sample !== ''),
    ),
  ];
  const repo = params.get('repo') ?? '';
  const repoKind = Object.values(CommitishKinds).find((kind) => kind === params.get('repo_kind')) ?? BLANK_INPUTS.repoKind;
  const named = Object.values(InputsMode).find((mode) => mode === params.get('inputs'));
  let mode = InputsMode.Files;
  if (named) mode = named;
  else if (repo !== '' && samples.length > 0) mode = InputsMode.Both;
  else if (repo !== '') mode = InputsMode.Repos;
  return { ...BLANK_INPUTS, mode, samples, repo, repoKind };
}

/**
 * Keep only the values of the shown sections, so a hidden section's values are never sent.
 *
 * @param state - The form values, hidden sections included.
 * @returns The values to send: the prompt always, files and samples only in Files or Both, the repo only in Repos or Both.
 */
export function visibleInputs(state: InputsState): InputsState {
  return {
    ...state,
    files: showsFiles(state.mode) ? state.files : [],
    samples: showsFiles(state.mode) ? state.samples : [],
    repo: showsRepos(state.mode) ? state.repo : '',
  };
}

/** What a run kind requires of its inputs. */
export enum InputsRequirement {
  /** At least one prompt, file, sample, or repo (workflow and tool runs; the API refuses a run without one) */
  AtLeastOne = 'at-least-one',
  /** Nothing: the run may start with no inputs (pipeline runs) */
  Optional = 'optional',
}

/**
 * Name why the shown inputs can't start a run yet, for the hint beside a disabled Run button.
 *
 * @param state - The form values, hidden sections included.
 * @param checks - The checks of the entered sample hashes.
 * @param requirement - What the run kind requires of its inputs.
 * @returns A sentence naming the cause, or `null` when the shown inputs are ready.
 */
export function inputsBlocker(state: InputsState, checks: Record<string, HashCheck>, requirement: InputsRequirement): string | null {
  const shown = visibleInputs(state);
  // entered samples must all be found before anything is uploaded
  const samples = hashChecksBlocker(shown.samples, checks);
  if (samples) return samples;
  if (requirement === InputsRequirement.Optional) return null;
  const given = shown.prompt.trim() !== '' || shown.files.length > 0 || shown.samples.length > 0 || shown.repo.trim() !== '';
  if (given) return null;
  if (state.mode === InputsMode.Files) return 'Add a file, an existing sample, or a prompt to run on.';
  if (state.mode === InputsMode.Repos) return 'Enter a repo or a prompt to run on.';
  return 'Add a file, an existing sample, a repo, or a prompt to run on.';
}

/** Uploads a form as a sample and returns its SHA256, or `null` after reporting an error. */
export type SampleUploader = (form: FormData) => Promise<string | null>;

/**
 * Turn the shown inputs into the samples and repos of a run, uploading the prompt and files first.
 *
 * @param state - The form values; only the shown sections are used.
 * @param groups - The groups to upload new samples to.
 * @param upload - Uploads a sample form and returns its SHA256.
 * @param requirement - What the run kind requires; an Optional run may have no inputs at all.
 * @returns The inputs, or an error message when a required input is missing, a hash isn't a SHA256, or an upload failed.
 */
export async function resolveInputs(
  state: InputsState,
  groups: string[],
  upload: SampleUploader,
  requirement: InputsRequirement = InputsRequirement.AtLeastOne,
): Promise<{ samples: string[]; repos: RepoDependencyRequest[]; error: string | null }> {
  const shown = visibleInputs(state);
  const samples = [...shown.samples];
  const repo = parseRepoInput(shown.repo, shown.repoKind);
  const repos = repo ? [repo] : [];
  const hasUploads = shown.prompt.trim() !== '' || shown.files.length > 0;
  // a hash that can't name a sample is refused here rather than by the API
  let problem = sampleHashProblem(samples);
  if (!problem && requirement === InputsRequirement.AtLeastOne) problem = inputsProblem(samples, repos, hasUploads);
  if (problem) return { samples: [], repos: [], error: problem };
  // upload the typed prompt as a small text file the run reads as a sample
  if (shown.prompt.trim() !== '') {
    const sha = await upload(buildSampleForm(promptFile(shown.prompt), groups, 'Run prompt'));
    if (!sha) return { samples: [], repos: [], error: 'The prompt could not be uploaded' };
    samples.push(sha);
  }
  // upload each chosen file as another sample, in the order listed
  for (const file of shown.files) {
    const sha = await upload(buildSampleForm(file, groups));
    if (!sha) return { samples: [], repos: [], error: `${file.name} could not be uploaded` };
    if (!samples.includes(sha)) samples.push(sha);
  }
  return { samples, repos, error: null };
}

/**
 * Say which of the shown inputs every run receives, for the list of runs to launch.
 *
 * @param state - The form values, hidden sections included; only the shown sections are named.
 * @returns A sentence naming the prompt, files, samples, and repo that will be sent, or saying there are none.
 */
export function inputsSummary(state: InputsState): string {
  const shown = visibleInputs(state);
  const plural = (count: number, noun: string) => `${count} ${noun}${count === 1 ? '' : 's'}`;
  const parts: string[] = [];
  if (shown.prompt.trim() !== '') parts.push('the prompt');
  if (shown.files.length > 0) parts.push(plural(shown.files.length, 'file'));
  if (shown.samples.length > 0) parts.push(plural(shown.samples.length, 'existing sample'));
  if (shown.repo.trim() !== '') parts.push(`the repo ${shown.repo.trim()}`);
  if (parts.length === 0) return 'Each run receives no inputs yet.';
  const list = parts.length === 1 ? parts[0] : `${parts.slice(0, -1).join(', ')} and ${parts[parts.length - 1]}`;
  return `Each run receives ${list}.`;
}
