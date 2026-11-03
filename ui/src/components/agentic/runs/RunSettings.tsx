import { FC } from 'react';

// project imports
import { slaProblem } from './launch';
import { Field, Select } from '@components/agentic/shared';
import TextInput from '@components/shared/inputs/TextInput';

/** Props for {@link GroupField}. */
interface GroupFieldProps {
  /** The groups the run may use */
  groups: string[];
  /** The chosen group, or an empty string */
  value: string;
  /** Called with the chosen group */
  onChange: (group: string) => void;
  /** The test id of the select */
  testId?: string;
}

/** The run group of one run: automatic when only one group is possible, else a required choice. */
export const GroupField: FC<GroupFieldProps> = ({ groups, value, onChange, testId = 'run-group' }) => (
  <Field>
    <span className="label">Run group</span>
    <Select data-testid={testId} value={value} onChange={(event) => onChange(event.target.value)}>
      <option value="">{groups.length === 1 ? `Automatic (${groups[0]})` : 'Choose a group'}</option>
      {groups.map((name) => (
        <option key={name} value={name}>
          {name}
        </option>
      ))}
    </Select>
  </Field>
);

/** Props for {@link TagsField} and {@link SlaField}. */
interface TextSettingProps {
  /** The value as typed */
  value: string;
  /** Called with the new value */
  onChange: (value: string) => void;
  /** The field label */
  label: string;
  /** A short hint under the label */
  hint?: string;
  /** The test id of the input */
  testId: string;
}

/** A tags field: comma separated tags as typed. */
export const TagsField: FC<TextSettingProps> = ({ value, onChange, label, hint, testId }) => (
  <Field>
    <span className="label">{label}</span>
    {hint && <span className="hint">{hint}</span>}
    <TextInput data-testid={testId} value={value} placeholder="case=1234, urgent" onChange={(event) => onChange(event.target.value)} />
  </Field>
);

/** An SLA field in seconds, flagging a value that isn't a positive whole number. */
export const SlaField: FC<TextSettingProps> = ({ value, onChange, label, hint, testId }) => {
  const slaError = slaProblem(value);
  return (
    <Field>
      <span className="label">{label}</span>
      {hint && <span className="hint">{hint}</span>}
      <TextInput
        data-testid={testId}
        $invalid={slaError !== ''}
        value={value}
        inputMode="numeric"
        onChange={(event) => onChange(event.target.value)}
      />
      {slaError !== '' && (
        <span className="error" data-testid={`${testId}-problem`}>
          {slaError}
        </span>
      )}
    </Field>
  );
};
