import { FC, useState } from 'react';
import styled from 'styled-components';

// project imports
import { CheckList, Field, Muted, Row, Select } from './shared';
import TextInput from '@components/shared/inputs/TextInput';
import { describeOption, duplicateNames } from '@utilities/agentic/groups';
import { PinChoice } from '@utilities/agentic/pins';
import { AnyAgenticEntity } from '@models/agentic';

/** A form field that keeps a gap above its label when it follows other page content. */
const SpacedField = styled(Field)<{ $spaced: boolean }>`
  ${({ $spaced }) => ($spaced ? 'margin-top: 0.75rem;' : '')}
`;

/** One version pin: the picked root's name beside its version select, wrapping under it when the row is narrow. */
const PinRow = styled.label`
  display: flex;
  align-items: center;
  gap: 0.5rem;
  flex-wrap: wrap;
`;

/** The name of the root a pin row chooses a version for. */
const PinName = styled.span`
  flex: 1 1 10rem;
  overflow-wrap: anywhere;
`;

/** The version select of a pin row, sharing the row with the name instead of filling it. */
const PinSelect = styled(Select)`
  flex: 1 1 14rem;
  width: auto;
`;

/** One choice in a picker: a value, its display text, and optionally a note and whether it can't be picked. */
export interface PickerOption {
  /** The value the picker reports */
  value: string;
  /** The text shown for the choice */
  text: string;
  /** A note shown after the text, such as the groups of a same-named entity */
  note?: string;
  /** Whether the choice can't be picked */
  disabled?: boolean;
}

/**
 * Describe entities as picker choices, noting the groups of same-named entities and why an entity can't be picked.
 *
 * @param entities - The entities to choose from.
 * @param groups - The groups of the entity being edited; entities missing any of them can't be picked.
 * @returns One choice per entity, in the order given.
 */
export function entityOptions(entities: AnyAgenticEntity[], groups: string[] = []): PickerOption[] {
  // names that repeat get a short id so the entries can be told apart
  const repeated = duplicateNames(entities);
  return entities.map((entity) => {
    const { note, disabled } = describeOption(entity, groups, repeated.has(entity.name));
    return { value: entity.id, text: entity.name, note, disabled };
  });
}

/**
 * Keep the choices whose text or note holds every word of a search, ignoring case.
 *
 * @param options - The choices.
 * @param query - The search as typed; blank keeps every choice.
 * @returns The matching choices, in the order given.
 */
export function filterOptions(options: PickerOption[], query: string): PickerOption[] {
  const words = query
    .toLowerCase()
    .split(/\s+/)
    .filter((word) => word !== '');
  return options.filter((option) => {
    const haystack = `${option.text} ${option.note ?? ''}`.toLowerCase();
    return words.every((word) => haystack.includes(word));
  });
}

/** Props for {@link CheckOptions}. */
interface CheckOptionsProps {
  /** The test id of the list */
  testId: string;
  /** The test id of each checkbox, from its choice */
  optionTestId: (option: PickerOption) => string;
  /** The choices to show */
  options: PickerOption[];
  /** The values currently checked */
  selected: string[];
  /** Called with the new checked values */
  onChange: (selected: string[]) => void;
  /** What the list says when it has no choices */
  emptyText: string;
}

/** A scrolling list of checkboxes that edits a list of values. */
const CheckOptions: FC<CheckOptionsProps> = ({ testId, optionTestId, options, selected, onChange, emptyText }) => (
  <CheckList data-testid={testId}>
    {options.length === 0 && <Muted>{emptyText}</Muted>}
    {options.map((option) => (
      <label key={option.value} className={option.disabled ? 'unavailable' : undefined}>
        <input
          type="checkbox"
          data-testid={optionTestId(option)}
          data-entity-id={option.value}
          checked={selected.includes(option.value)}
          disabled={option.disabled}
          onChange={() => onChange(toggleValue(selected, option.value))}
        />
        {option.text}
        {option.note && <Muted>({option.note})</Muted>}
      </label>
    ))}
  </CheckList>
);

/** The search box of a picker, sharing its row with the count. */
const SearchInput = styled(TextInput)`
  flex: 1 1 14rem;
  width: auto;
`;

/** Props for {@link toggleValue}-driven check lists. */
interface ChecksProps {
  /** The field label */
  label: string;
  /** A short hint under the label */
  hint?: string;
  /** The test id prefix for the checkboxes */
  testId: string;
  /** The options: a value, its display text, and optionally a note and whether it can't be picked */
  options: PickerOption[];
  /** The values currently checked */
  selected: string[];
  /** Called with the new checked values */
  onChange: (selected: string[]) => void;
  /** Whether the list follows other content on the page and needs a gap above it */
  spaced?: boolean;
}

/**
 * Add a value to a list, or remove it when the list already has it.
 *
 * @param values - The current values.
 * @param value - The value to toggle.
 * @returns A new list with the value toggled.
 */
export function toggleValue(values: string[], value: string): string[] {
  return values.includes(value) ? values.filter((entry) => entry !== value) : [...values, value];
}

/** A labelled list of checkboxes that edits a list of values. */
export const Checks: FC<ChecksProps> = ({ label, hint, testId, options, selected, onChange, spaced = false }) => (
  <SpacedField as="div" $spaced={spaced}>
    <span className="label">{label}</span>
    {hint && <span className="hint">{hint}</span>}
    <CheckOptions
      testId={testId}
      optionTestId={(option) => `${testId}-${option.text}`}
      options={options}
      selected={selected}
      onChange={onChange}
      emptyText="none"
    />
  </SpacedField>
);

/** Props for {@link SearchPicker}. */
interface SearchPickerProps {
  /** The field label */
  label: string;
  /** A short hint under the label */
  hint?: string;
  /** The test id of the list; the search box is `<testId>-search` and each checkbox `<testId>-<value>` */
  testId: string;
  /** The choices */
  options: PickerOption[];
  /** The values currently picked */
  selected: string[];
  /** Called with the new picked values */
  onChange: (selected: string[]) => void;
  /** What the list says when there is nothing to choose from */
  emptyText?: string;
}

/** A labelled multi-select: a search box above a list of checkboxes, with a count of the choices shown. */
export const SearchPicker: FC<SearchPickerProps> = ({ label, hint, testId, options, selected, onChange, emptyText = 'none' }) => {
  const [query, setQuery] = useState('');
  const shown = filterOptions(options, query);
  return (
    <Field as="div">
      <span className="label">{label}</span>
      {hint && <span className="hint">{hint}</span>}
      <Row>
        <SearchInput
          data-testid={`${testId}-search`}
          aria-label={`Search ${label.toLowerCase()}`}
          placeholder="Search by name or group"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
        />
        <Muted data-testid={`${testId}-count`}>
          {query.trim() === '' ? `${options.length} available` : `${shown.length} of ${options.length}`}, {selected.length} chosen
        </Muted>
      </Row>
      <CheckOptions
        testId={testId}
        optionTestId={(option) => `${testId}-${option.value}`}
        options={shown}
        selected={selected}
        onChange={onChange}
        emptyText={options.length === 0 ? emptyText : 'No matches.'}
      />
    </Field>
  );
};

/** Props for {@link GroupChecks}. */
interface GroupChecksProps {
  /** The group names to choose from */
  groups: string[];
  /** The groups checked */
  selected: string[];
  /** Called with the new checked groups */
  onChange: (selected: string[]) => void;
  /** The field label */
  label?: string;
}

/** A check list of groups. */
export const GroupChecks: FC<GroupChecksProps> = ({ groups, selected, onChange, label = 'Groups' }) => (
  <Checks
    label={label}
    testId="groups"
    options={groups.map((group) => ({ value: group, text: group }))}
    selected={selected}
    onChange={onChange}
  />
);

/** Props for {@link EntityChecks}. */
interface EntityChecksProps<T extends AnyAgenticEntity> {
  /** The field label */
  label: string;
  /** A short hint under the label */
  hint?: string;
  /** The test id prefix */
  testId: string;
  /** The entities to choose from */
  entities: T[];
  /** The groups of the entity being edited; entities missing any of them can't be picked */
  groups?: string[];
  /** The entity ids checked */
  selected: string[];
  /** Called with the new checked ids */
  onChange: (selected: string[]) => void;
  /** Whether the list follows other content on the page and needs a gap above it */
  spaced?: boolean;
}

/** A check list of entities by name that edits a list of entity ids; each shows its groups so same-named entities differ. */
export function EntityChecks<T extends AnyAgenticEntity>({
  label,
  hint,
  testId,
  entities,
  groups = [],
  selected,
  onChange,
  spaced,
}: EntityChecksProps<T>) {
  return (
    <Checks
      label={label}
      hint={hint}
      testId={testId}
      options={entityOptions(entities, groups)}
      selected={selected}
      onChange={onChange}
      spaced={spaced}
    />
  );
}

/** Props for {@link EntitySelect}. */
interface EntitySelectProps<T extends AnyAgenticEntity> {
  /** The field label */
  label: string;
  /** The test id of the select */
  testId: string;
  /** The entities to choose from */
  entities: T[];
  /** The groups of the entity being edited; entities missing any of them can't be picked */
  groups?: string[];
  /** The chosen entity id, or an empty string for none */
  value: string;
  /** Called with the chosen id */
  onChange: (value: string) => void;
  /** The text of the empty option */
  emptyText?: string;
  /** Whether the select follows other content on the page and needs a gap above it */
  spaced?: boolean;
}

/** A labelled select that chooses one entity by name, noting the groups of same-named or unavailable entities. */
export function EntitySelect<T extends AnyAgenticEntity>({
  label,
  testId,
  entities,
  groups = [],
  value,
  onChange,
  emptyText = 'None',
  spaced = false,
}: EntitySelectProps<T>) {
  // names that repeat get a short id so the entries can be told apart
  const repeated = duplicateNames(entities);
  return (
    <SpacedField $spaced={spaced}>
      <span className="label">{label}</span>
      <Select data-testid={testId} value={value} onChange={(event) => onChange(event.target.value)}>
        <option value="">{emptyText}</option>
        {entityOptions(entities, groups).map((option) => {
          // the note is only added when it is needed to tell entries apart or to explain why one is unavailable
          const needsNote = option.disabled || repeated.has(option.text);
          return (
            <option key={option.value} value={option.value} disabled={option.disabled}>
              {needsNote ? `${option.text} (${option.note})` : option.text}
            </option>
          );
        })}
      </Select>
    </SpacedField>
  );
}

/** Props for {@link VersionPins}. */
interface VersionPinsProps {
  /** The field label */
  label: string;
  /** The test id prefix; each select is `<testId>-<name>` */
  testId: string;
  /** The picked roots and the versions each can be pinned to */
  choices: PinChoice[];
  /** The chosen instance id for each pinned root; a missing or empty entry follows latest */
  pins: Record<string, string>;
  /** Called with the new pins */
  onChange: (pins: Record<string, string>) => void;
}

/** One select per picked root that keeps the link at latest or pins it to one version. */
export const VersionPins: FC<VersionPinsProps> = ({ label, testId, choices, pins, onChange }) => {
  if (choices.length === 0) return null;
  return (
    <Field as="div" data-testid={testId}>
      <span className="label">{label}</span>
      <span className="hint">Latest follows new versions as they are published; pinned stays on one version</span>
      {choices.map((choice) => (
        <PinRow key={choice.id}>
          <PinName>{choice.name}</PinName>
          <PinSelect
            data-testid={`${testId}-${choice.name}`}
            aria-label={`${choice.name} version`}
            value={pins[choice.id] ?? ''}
            onChange={(event) => onChange({ ...pins, [choice.id]: event.target.value })}
          >
            <option value="">{choice.latestText}</option>
            {choice.options.map((option) => (
              <option key={option.value} value={option.value}>
                {option.text}
              </option>
            ))}
          </PinSelect>
        </PinRow>
      ))}
    </Field>
  );
};
