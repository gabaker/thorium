import styled from 'styled-components';

// project imports
import { errorOutline } from '@components/shared/inputs/FieldError';

// Fixed width for every section's title column so titles line up at the same x
// regardless of title length (mirrors the image form scaffolding).
export const SECTION_TITLE_WIDTH = '210px';
export const PIPELINE_CREATE_MAX_WIDTH = '900px';

/// A horizontal section row: title column on the left, fields on the right
export const SectionRow = styled.div`
  display: flex;
  gap: 8px;
  align-items: flex-start;
`;

/// The fixed-width title column used by the create form's sections
export const TitleCol = styled.div`
  flex: 0 0 ${SECTION_TITLE_WIDTH};
`;

/// The flexible field column that holds a section's inputs
export const FieldCol = styled.div`
  flex: 1;
`;

/// Empty spacer column that aligns edit-mode sections with the create form
export const EditSpacer = styled.div`
  display: none;
`;

/// The fixed-width title column used by the inline edit form's sections
export const EditMiddle = styled.div`
  flex: 0 0 ${SECTION_TITLE_WIDTH};
`;

/// The flexible field column for the inline edit form
export const EditFieldCol = styled.div`
  flex: 1;
`;

/// A single labeled form field group with bottom spacing
export const FieldGroup = styled.div`
  margin-bottom: 8px;
`;

/// The label rendered above a form field
export const Label = styled.label`
  display: block;
  font-size: 13px;
  font-weight: 600;
  color: var(--thorium-secondary-text);
  margin-bottom: 4px;
`;

/// A themed text input used across the pipeline form. When `$error` is set the input
/// gains a danger border + glow so the user can see which field still needs attention.
export const Input = styled.input<{ $error?: boolean }>`
  width: 100%;
  padding: 6px 12px;
  border: 1px solid var(--thorium-panel-border);
  border-radius: 4px;
  background: var(--thorium-secondary-panel-bg);
  color: var(--thorium-text);
  font-size: 14px;
  ${({ $error }) => $error && errorOutline}

  &:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
`;

/// A themed multi-line text input used for the description field
export const TextArea = styled.textarea`
  width: 100%;
  padding: 6px 12px;
  border: 1px solid var(--thorium-panel-border);
  border-radius: 4px;
  background: var(--thorium-secondary-panel-bg);
  color: var(--thorium-text);
  font-size: 14px;
  resize: vertical;
`;

/// A themed select input used across the pipeline form. `$error` adds a danger outline.
export const Select = styled.select<{ $error?: boolean }>`
  width: 100%;
  padding: 6px 12px;
  border: 1px solid var(--thorium-panel-border);
  border-radius: 4px;
  background: var(--thorium-secondary-panel-bg);
  color: var(--thorium-text);
  font-size: 14px;
  ${({ $error }) => $error && errorOutline}
`;

/// Wraps the pipeline fields form; fills the field column so the inputs line up with
/// the Order and Triggers sections rather than being capped narrower.
export const PipelineFieldsWrapper = styled.div`
  width: 100%;
`;

/// The outer wrapper centering the create page content
export const PipelineCreateWrapper = styled.div`
  max-width: ${PIPELINE_CREATE_MAX_WIDTH};
  margin: 0 auto;
`;
