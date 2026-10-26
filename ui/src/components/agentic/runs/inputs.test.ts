import { describe, expect, it, vi } from 'vitest';

// project imports
import { BLANK_INPUTS, initialInputs, inputsBlocker, InputsMode, InputsRequirement, resolveInputs, visibleInputs } from './inputs';
import { HashCheckStatus } from '@utilities/hashes';

/** A well-formed sample SHA256. */
const SHA = 'a'.repeat(64);

describe('resolveInputs', () => {
  it('refuses a typed sample that is not a SHA256 and uploads nothing', async () => {
    const upload = vi.fn();
    const result = await resolveInputs({ ...BLANK_INPUTS, prompt: 'p', samples: [SHA, 'nothex'] }, ['g'], upload);
    expect(result.error).toMatch(/"nothex" is not a sample SHA256/);
    expect(upload).not.toHaveBeenCalled();
  });
  it('refuses a form with no input and uploads nothing', async () => {
    const upload = vi.fn();
    const result = await resolveInputs(BLANK_INPUTS, ['g'], upload);
    expect(result.error).toMatch(/at least one input/);
    expect(upload).not.toHaveBeenCalled();
  });
  it('uploads the prompt as prompt.txt and adds its hash', async () => {
    const upload = vi.fn().mockResolvedValue('sha-p');
    const result = await resolveInputs({ ...BLANK_INPUTS, prompt: 'hello', samples: [SHA] }, ['g'], upload);
    expect(result).toEqual({ samples: [SHA, 'sha-p'], repos: [], error: null });
    const form = upload.mock.calls[0][0] as FormData;
    expect((form.get('data') as File).name).toBe('prompt.txt');
    expect(form.getAll('groups')).toEqual(['g']);
  });
  it('uploads the chosen file too and parses the repo', async () => {
    const upload = vi.fn().mockResolvedValueOnce('sha-f');
    const file = new File(['x'], 'a.bin');
    const result = await resolveInputs(
      { ...BLANK_INPUTS, mode: InputsMode.Both, files: [file], repo: 'github.com/o/r:main' },
      ['g'],
      upload,
    );
    expect(result.samples).toEqual(['sha-f']);
    expect(result.repos[0]).toMatchObject({ url: 'github.com/o/r', commitish: 'main' });
  });
  it('reports a failed upload', async () => {
    const upload = vi.fn().mockResolvedValue(null);
    expect((await resolveInputs({ ...BLANK_INPUTS, prompt: 'p' }, ['g'], upload)).error).toMatch(/prompt/);
    expect((await resolveInputs({ ...BLANK_INPUTS, files: [new File(['x'], 'a.bin')] }, ['g'], upload)).error).toMatch(/a.bin/);
  });
  it('runs on a repo alone', async () => {
    const upload = vi.fn();
    const result = await resolveInputs({ ...BLANK_INPUTS, mode: InputsMode.Repos, repo: 'github.com/o/r' }, ['g'], upload);
    expect(result.error).toBeNull();
    expect(upload).not.toHaveBeenCalled();
  });
  it('uploads every chosen file in order', async () => {
    const upload = vi.fn().mockResolvedValueOnce('sha-1').mockResolvedValueOnce('sha-2');
    const files = [new File(['1'], 'one.bin'), new File(['2'], 'two.bin')];
    const result = await resolveInputs({ ...BLANK_INPUTS, files }, ['g'], upload);
    expect(result.samples).toEqual(['sha-1', 'sha-2']);
    expect(upload).toHaveBeenCalledTimes(2);
  });
  it('lets a pipeline run with no inputs at all', async () => {
    const upload = vi.fn();
    const result = await resolveInputs(BLANK_INPUTS, ['g'], upload, InputsRequirement.Optional);
    expect(result).toEqual({ samples: [], repos: [], error: null });
  });
});

describe('input modes', () => {
  const filled = { ...BLANK_INPUTS, files: [new File(['x'], 'a.bin')], samples: [SHA], repo: 'github.com/o/r' };
  it('sends only the files and samples in Files mode', () => {
    const shown = visibleInputs({ ...filled, mode: InputsMode.Files });
    expect(shown.files).toHaveLength(1);
    expect(shown.samples).toEqual([SHA]);
    expect(shown.repo).toBe('');
  });
  it('sends only the repo in Repos mode, keeping the hidden values in the state', () => {
    const state = { ...filled, mode: InputsMode.Repos };
    const shown = visibleInputs(state);
    expect(shown.files).toEqual([]);
    expect(shown.samples).toEqual([]);
    expect(shown.repo).toBe('github.com/o/r');
    expect(state.samples).toEqual([SHA]);
  });
  it('sends both in Both mode', () => {
    const shown = visibleInputs({ ...filled, mode: InputsMode.Both });
    expect(shown.samples).toEqual([SHA]);
    expect(shown.repo).toBe('github.com/o/r');
  });
  it('submits only the shown repo when samples are hidden', async () => {
    const upload = vi.fn().mockResolvedValue('sha-f');
    const result = await resolveInputs({ ...filled, mode: InputsMode.Repos }, ['g'], upload);
    expect(upload).not.toHaveBeenCalled();
    expect(result.samples).toEqual([]);
    expect(result.repos).toHaveLength(1);
  });
});

describe('initialInputs', () => {
  it('opens Files with no prefill', () => {
    expect(initialInputs(new URLSearchParams()).mode).toBe(InputsMode.Files);
  });
  it('opens Files with linked samples, one parameter each', () => {
    const inputs = initialInputs(new URLSearchParams(`sample=${SHA}&sample=${'B'.repeat(64)}`));
    expect(inputs.mode).toBe(InputsMode.Files);
    expect(inputs.samples).toEqual([SHA, 'b'.repeat(64)]);
  });
  it('keeps a linked sample once however its case or repeats are written', () => {
    const upper = SHA.toUpperCase();
    const inputs = initialInputs(new URLSearchParams(`sample=${SHA}&sample=${upper}&sample=${SHA},${upper}, `));
    expect(inputs.samples).toEqual([SHA]);
  });
  it('opens Repos for a linked repo alone and Both for a repo with samples', () => {
    expect(initialInputs(new URLSearchParams('repo=github.com/o/r')).mode).toBe(InputsMode.Repos);
    expect(initialInputs(new URLSearchParams(`repo=github.com/o/r&sample=${SHA}`)).mode).toBe(InputsMode.Both);
  });
  it('keeps a named mode and a commitish kind', () => {
    const inputs = initialInputs(new URLSearchParams('inputs=both&repo=r&repo_kind=Tag'));
    expect(inputs.mode).toBe(InputsMode.Both);
    expect(inputs.repoKind).toBe('Tag');
  });
});

describe('inputsBlocker', () => {
  const valid = { [SHA]: { status: HashCheckStatus.Valid } };
  it('names the missing input of each mode for a run that needs one', () => {
    expect(inputsBlocker(BLANK_INPUTS, {}, InputsRequirement.AtLeastOne)).toMatch(/file, an existing sample, or a prompt/);
    expect(inputsBlocker({ ...BLANK_INPUTS, mode: InputsMode.Repos }, {}, InputsRequirement.AtLeastOne)).toMatch(/repo or a prompt/);
  });
  it('asks for nothing when a pipeline has no inputs', () => {
    expect(inputsBlocker(BLANK_INPUTS, {}, InputsRequirement.Optional)).toBeNull();
  });
  it('follows the mode: hidden samples neither satisfy nor block', () => {
    const state = { ...BLANK_INPUTS, mode: InputsMode.Repos, samples: ['nothex'] };
    expect(inputsBlocker(state, {}, InputsRequirement.AtLeastOne)).toMatch(/repo or a prompt/);
    expect(inputsBlocker({ ...state, mode: InputsMode.Files }, {}, InputsRequirement.AtLeastOne)).toMatch(/"nothex" is not a SHA256/);
  });
  it('waits while a sample is checked and passes once it is found', () => {
    const state = { ...BLANK_INPUTS, samples: [SHA] };
    expect(inputsBlocker(state, {}, InputsRequirement.AtLeastOne)).toMatch(/Checking/);
    expect(inputsBlocker(state, valid, InputsRequirement.AtLeastOne)).toBeNull();
  });
  it('names a sample that was not found or is not accessible', () => {
    const state = { ...BLANK_INPUTS, samples: [SHA] };
    const missing = { [SHA]: { status: HashCheckStatus.Missing } };
    expect(inputsBlocker(state, missing, InputsRequirement.Optional)).toMatch(/not found or is not accessible/);
  });
});
