import { FC } from 'react';
import styled from 'styled-components';

// project imports
import { BLANK_STAGE, StageRow, StageStep, pinnedVersionName, stageToolChoices } from './stages';
import { Actions, Muted, Row, Section, Select } from '@components/agentic/shared';
import { Button, ButtonSize, ButtonVariant } from '@components/shared/buttons';
import TextInput from '@components/shared/inputs/TextInput';
import { Tool } from '@models/tools';

/** A step's heading note, kept lighter than the heading it follows. */
const StepNote = styled(Muted)`
  font-size: 0.9rem;
  font-weight: 400;
`;

/**
 * One stage's controls on a single line (shrinking as needed so Remove never wraps under the row); the
 * inputs share the selects' height so the row's boxes line up.
 */
const StageLine = styled(Row)`
  margin-bottom: 0.5rem;
  flex-wrap: nowrap;

  input,
  select {
    height: 2.5rem;
    min-width: 0;
  }

  button {
    flex-shrink: 0;
  }
`;

/** The stage name box. */
const StageName = styled(TextInput)`
  max-width: 12rem;
  flex: 1 1 6rem;
`;

/** The tool choice. */
const StageTool = styled(Select)`
  max-width: 16rem;
  flex: 1 1 8rem;
`;

/** The tool version choice. */
const StageVersion = styled(Select)`
  max-width: 14rem;
  flex: 1 1 7rem;
`;

/** Props for {@link StageEditor}. */
interface StageEditorProps {
  /** The tools a stage can be backed by */
  tools: Tool[];
  /** The pipeline's group, or an empty string before one is chosen */
  group: string;
  /** The steps being edited */
  steps: StageStep[];
  /** Called with the new steps */
  onChange: (steps: StageStep[]) => void;
}

/** Edit a pipeline's steps: stages in a step run in parallel and steps run in order. */
export const StageEditor: FC<StageEditorProps> = ({ tools, group, steps, onChange }) => {
  // only Image tools in the pipeline's group can back a pipeline stage
  const imageTools = stageToolChoices(tools, group);
  /** Replace one stage row. */
  const setStage = (step: number, index: number, change: Partial<StageRow>) =>
    onChange(steps.map((rows, s) => (s === step ? rows.map((row, i) => (i === index ? { ...row, ...change } : row)) : rows)));
  return (
    <div data-testid="stage-editor">
      {steps.map((rows, step) => (
        <Section key={step} data-testid={`step-${step}`}>
          <h2>
            Step {step + 1} <StepNote>(stages in a step run in parallel)</StepNote>
          </h2>
          {rows.map((row, index) => {
            const tool = imageTools.find((candidate) => candidate.id === row.tool);
            return (
              <StageLine key={index}>
                <StageName
                  aria-label={`Stage name, step ${step + 1} stage ${index + 1}`}
                  placeholder="Stage name"
                  data-testid={`stage-name-${step}-${index}`}
                  value={row.name}
                  onChange={(event) => setStage(step, index, { name: event.target.value })}
                />
                <StageTool
                  aria-label={`Tool, step ${step + 1} stage ${index + 1}`}
                  data-testid={`stage-tool-${step}-${index}`}
                  value={row.tool}
                  onChange={(event) => setStage(step, index, { tool: event.target.value, instance: '' })}
                >
                  <option value="">Choose a tool</option>
                  {imageTools.map((candidate) => (
                    <option key={candidate.id} value={candidate.id}>
                      {candidate.name}
                    </option>
                  ))}
                </StageTool>
                <StageVersion
                  aria-label={`Tool version, step ${step + 1} stage ${index + 1}`}
                  data-testid={`stage-version-${step}-${index}`}
                  value={row.instance}
                  disabled={!tool}
                  onChange={(event) => setStage(step, index, { instance: event.target.value })}
                >
                  <option value="">Follow latest</option>
                  {(tool?.metadata.Tool.instances ?? []).map((instance) => (
                    <option key={instance.id} value={instance.id}>
                      Pin {pinnedVersionName(instance.version)}
                      {instance.deprecated ? ' (deprecated)' : ''}
                    </option>
                  ))}
                </StageVersion>
                <Button
                  size={ButtonSize.Small}
                  variant={ButtonVariant.Danger}
                  aria-label={`Remove stage ${index + 1} of step ${step + 1}`}
                  onClick={() => onChange(steps.map((current, s) => (s === step ? current.filter((_, i) => i !== index) : current)))}
                >
                  Remove stage
                </Button>
              </StageLine>
            );
          })}
          <Row>
            <Button
              size={ButtonSize.Small}
              variant={ButtonVariant.Secondary}
              data-testid={`add-stage-${step}`}
              onClick={() => onChange(steps.map((current, s) => (s === step ? [...current, { ...BLANK_STAGE }] : current)))}
            >
              Add a parallel stage
            </Button>
            <Button
              size={ButtonSize.Small}
              variant={ButtonVariant.Danger}
              aria-label={`Remove step ${step + 1}`}
              onClick={() => onChange(steps.filter((_, s) => s !== step))}
            >
              Remove step
            </Button>
          </Row>
        </Section>
      ))}
      <Actions>
        <Button
          size={ButtonSize.Small}
          variant={ButtonVariant.Secondary}
          data-testid="add-step"
          onClick={() => onChange([...steps, [{ ...BLANK_STAGE }]])}
        >
          Add a step
        </Button>
      </Actions>
    </div>
  );
};
