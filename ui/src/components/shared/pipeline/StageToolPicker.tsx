import React from 'react';
import { Link } from 'react-router-dom';
import Select, { CSSObjectWithLabel } from 'react-select';

// project imports
import { PickerNote } from './PipelineOrderFlow.styled';
import { StageToolOption } from '@components/agentic/pipelines/stages';
import { createReactSelectStyles } from '@utilities/select';

/** The react-select styles shared by the diagram's menus. */
const baseSelectStyles = createReactSelectStyles('White', 'rgb(160, 162, 163)');

/**
 * The menu's styles: wide enough that an option such as `tool · follow latest` stays on one line, and
 * never wider than the area the diagram leaves for it.
 */
const selectStyles = {
  ...baseSelectStyles,
  menu: (base: CSSObjectWithLabel) => ({
    ...(baseSelectStyles.menu(base) as CSSObjectWithLabel),
    width: 'max-content',
    minWidth: '100%',
    maxWidth: 'min(32rem, 90vw)',
  }),
  option: (base: CSSObjectWithLabel) => ({
    ...(baseSelectStyles.option(base) as CSSObjectWithLabel),
    whiteSpace: 'nowrap',
    overflow: 'hidden',
    textOverflow: 'ellipsis',
  }),
};

/** Props for {@link StageToolPicker}. */
interface StageToolPickerProps {
  /** The tools, and pinned versions, a new stage can be backed by */
  options: StageToolOption[];
  /** The pipeline's group, named in the empty state */
  group: string;
  /** Whether the group's tools are still loading */
  loading: boolean;
  /** The error from loading the tools, or an empty string */
  error: string;
  /** Called with the tool the user chose */
  onSelect: (option: StageToolOption) => void;
  /** Called when the picker loses focus */
  onBlur: () => void;
}

/** The diagram's add-stage menu: a searchable list of the group's stage tools, or why there are none. */
const StageToolPicker: React.FC<StageToolPickerProps> = ({ options, group, loading, error, onSelect, onBlur }) => {
  if (loading) return <PickerNote data-testid="stage-tool-loading">Loading tools...</PickerNote>;
  if (error !== '') return <PickerNote data-testid="stage-tool-error">{error}</PickerNote>;
  if (options.length === 0) {
    return (
      <PickerNote data-testid="stage-tool-empty">
        No stage tools in group {group}. Every stage runs an Image tool in the pipeline&apos;s group.{' '}
        <Link to="/tools/create">Create a tool</Link>
      </PickerNote>
    );
  }
  return (
    <Select<StageToolOption>
      options={options}
      onChange={(option) => option && onSelect(option)}
      styles={selectStyles}
      placeholder="Choose a tool"
      aria-label="Tool for the new stage"
      autoFocus
      menuIsOpen
      onBlur={onBlur}
    />
  );
};

export default StageToolPicker;
