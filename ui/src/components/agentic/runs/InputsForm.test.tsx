import { renderToStaticMarkup } from 'react-dom/server';
import { MemoryRouter } from 'react-router-dom';
import { describe, expect, it } from 'vitest';

// project imports
import { InputsForm } from './InputsForm';
import { BLANK_INPUTS, InputsMode, InputsRequirement, InputsState } from './inputs';

/** Render the inputs form with the given values and props. */
const render = (
  value: Partial<InputsState> = {},
  props: { showPrompt?: boolean; promptOptional?: boolean; requirement?: InputsRequirement } = {},
): string =>
  renderToStaticMarkup(
    <MemoryRouter>
      <InputsForm value={{ ...BLANK_INPUTS, ...value }} onChange={() => undefined} repoUrls={[]} {...props} />
    </MemoryRouter>,
  );

describe('InputsForm prompt', () => {
  it('labels the prompt plainly by default', () => {
    const markup = render();
    expect(markup).toContain('>Prompt<');
    expect(markup).not.toContain('Prompt (optional)');
  });
  it('marks the prompt optional for runs where only some stages read it', () => {
    const markup = render({}, { promptOptional: true });
    expect(markup).toContain('Prompt (optional)');
    expect(markup).toContain('only used by stages that run agentic tools');
  });
  it('leaves the prompt out when it is not offered', () => {
    expect(render({}, { showPrompt: false })).not.toContain('run-prompt');
  });
});

describe('InputsForm selector', () => {
  it('offers Files, Repos, and Both, with Files chosen by default', () => {
    const markup = render();
    expect(markup).toMatch(/data-testid="inputs-mode-files"[^>]*>|aria-selected="true"[^>]*data-testid="inputs-mode-files"/);
    expect(markup).toContain('data-testid="inputs-mode-repos"');
    expect(markup).toContain('data-testid="inputs-mode-both"');
  });
  it('shows the compact dropzone and the sample tokens in Files mode, and no repo', () => {
    const markup = render({ mode: InputsMode.Files });
    expect(markup).toContain('data-testid="dropzone-compact"');
    expect(markup).toContain('data-testid="run-file"');
    expect(markup).toContain('data-testid="run-samples"');
    expect(markup).not.toContain('data-testid="run-repo"');
  });
  it('shows only the repo in Repos mode', () => {
    const markup = render({ mode: InputsMode.Repos });
    expect(markup).toContain('data-testid="run-repo"');
    expect(markup).not.toContain('data-testid="dropzone-compact"');
    expect(markup).not.toContain('data-testid="run-samples"');
  });
  it('shows both sections in Both mode', () => {
    const markup = render({ mode: InputsMode.Both });
    expect(markup).toContain('data-testid="run-repo"');
    expect(markup).toContain('data-testid="run-samples"');
  });
  it('says the inputs are optional for a pipeline and required otherwise', () => {
    expect(render({}, { requirement: InputsRequirement.Optional })).toContain('Inputs (optional)');
    expect(render()).toContain('needs at least one input');
  });
  it('lists the chosen files in the dropzone with a remove control', () => {
    const markup = render({ files: [new File(['x'], 'one.bin')] });
    expect(markup).toContain('one.bin');
    expect(markup).toContain('aria-label="Remove one.bin"');
  });
});
