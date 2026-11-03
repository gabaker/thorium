import { FC } from 'react';
import styled from 'styled-components';

// project imports
import { InputsMode, InputsRequirement, InputsState, showsFiles, showsRepos } from './inputs';
import { Field, Select, TextArea } from '@components/agentic/shared';
import { HashTokenInput } from '@components/shared/inputs/HashTokenInput';
import TextInput from '@components/shared/inputs/TextInput';
import { Tabs } from '@components/shared/tabs';
import UploadDropzone, { DropzoneSize } from '@components/shared/UploadDropzone';
import { HashCheck } from '@utilities/hashes';
import { CommitishKinds } from '@models/reactions';

/** The block of the chosen input sections, set off from the selector above it. */
const InputsPanel = styled.div`
  margin-top: 0.75rem;
`;

/** The labels of the input modes, in the order the selector shows them. */
const MODE_TABS = [
  { key: InputsMode.Files, label: 'Files', testId: 'inputs-mode-files' },
  { key: InputsMode.Repos, label: 'Repos', testId: 'inputs-mode-repos' },
  { key: InputsMode.Both, label: 'Both', testId: 'inputs-mode-both' },
];

/** Props for {@link InputsForm}. */
interface InputsFormProps {
  /** The current values */
  value: InputsState;
  /** Called with the changed values */
  onChange: (value: InputsState) => void;
  /** The repos Thorium knows, offered as suggestions */
  repoUrls: string[];
  /** The checks of the entered sample hashes */
  checks?: Record<string, HashCheck>;
  /** What the run kind requires of its inputs, which the copy states */
  requirement?: InputsRequirement;
  /** Whether the prompt is marked optional, for runs where only some steps read it (a static pipeline) */
  promptOptional?: boolean;
}

/** The input fields of a run: the prompt, then a choice of samples (files and existing samples), a repo, or both. */
export const InputsForm: FC<InputsFormProps> = ({
  value,
  onChange,
  repoUrls,
  checks = {},
  requirement = InputsRequirement.AtLeastOne,
  promptOptional = false,
}) => (
  <>
    <Field>
      <span className="label">{promptOptional ? 'Prompt (optional)' : 'Prompt'}</span>
      <span className="hint">
        {promptOptional ? 'Optional: only used by stages that run agentic tools. ' : ''}
        Uploaded as a text file named prompt.txt and given to the run as an input.
      </span>
      <TextArea data-testid="run-prompt" value={value.prompt} onChange={(event) => onChange({ ...value, prompt: event.target.value })} />
    </Field>
    {/* a div, not a label: a label would pass clicks on its tabs and tokens to its first control */}
    <Field as="div">
      <span className="label">{requirement === InputsRequirement.Optional ? 'Inputs (optional)' : 'Inputs'}</span>
      <span className="hint" data-testid="inputs-requirement">
        {requirement === InputsRequirement.Optional
          ? 'A pipeline may run with no inputs. Only the inputs shown are sent.'
          : 'The run needs at least one input: a prompt, a file, an existing sample, or a repo. Only the inputs shown are sent.'}
      </span>
      <Tabs active={value.mode} onChange={(mode) => onChange({ ...value, mode })} aria-label="Run inputs" tabs={MODE_TABS} />
      <InputsPanel role="tabpanel" id={`tabpanel-${value.mode}`} aria-labelledby={`tab-${value.mode}`}>
        {showsFiles(value.mode) && (
          <>
            <Field as="div">
              <span className="label">Files</span>
              <span className="hint">Each file is uploaded as a sample to the run group.</span>
              <UploadDropzone
                size={DropzoneSize.Compact}
                inputTestId="run-file"
                selectedFiles={value.files}
                onChange={(files) => onChange({ ...value, files })}
              />
            </Field>
            <Field as="div">
              <span className="label">Existing samples</span>
              <span className="hint">SHA256s already in Thorium; each is checked as it is added.</span>
              <HashTokenInput
                testId="run-samples"
                aria-label="Existing samples"
                hashes={value.samples}
                checks={checks}
                linkTo={(hash) => `/file/${hash}`}
                onChange={(samples) => onChange({ ...value, samples })}
              />
            </Field>
          </>
        )}
        {showsRepos(value.mode) && (
          <>
            <Field>
              <span className="label">Repo</span>
              <span className="hint">A repo URL, optionally followed by :commit, :branch, or :tag.</span>
              <TextInput
                data-testid="run-repo"
                list="run-repo-urls"
                value={value.repo}
                placeholder="github.com/org/project:main"
                onChange={(event) => onChange({ ...value, repo: event.target.value })}
              />
              <datalist id="run-repo-urls">
                {repoUrls.map((url) => (
                  <option key={url} value={url} />
                ))}
              </datalist>
            </Field>
            <Field>
              <span className="label">Commitish kind</span>
              <Select
                data-testid="run-repo-kind"
                value={value.repoKind}
                onChange={(event) => onChange({ ...value, repoKind: event.target.value as CommitishKinds })}
              >
                {Object.values(CommitishKinds).map((kind) => (
                  <option key={kind} value={kind}>
                    {kind}
                  </option>
                ))}
              </Select>
            </Field>
          </>
        )}
      </InputsPanel>
    </Field>
  </>
);
