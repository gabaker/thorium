import { FC, useState } from 'react';
import styled from 'styled-components';

// project imports
import { statusTone } from './display';
import { StageLogView } from './StageLogView';
import { Badge, Mono, Muted, Section } from '@components/agentic/shared';
import { Button, ButtonSize, ButtonVariant } from '@components/shared/buttons';
import { isRunFinished } from '@utilities/runs';
import { Reaction, ReactionStatus } from '@models/reactions';

/** Props for {@link RunPlanPanel}. */
interface RunPlanPanelProps {
  /** The run */
  run: Reaction;
}

/** One step of the plan, spaced from the next step; the last step leaves no margin to add to the card's padding. */
const PlanStep = styled.div`
  &:not(:last-child) {
    margin-bottom: 0.5rem;
  }
`;

/** The stages of a run's plan in order, each expandable to its logs. */
export const RunPlanPanel: FC<RunPlanPanelProps> = ({ run }) => {
  const [open, setOpen] = useState<string[]>([]);
  const [closed, setClosed] = useState<string[]>([]);
  const steps = run.plan?.stages ?? [];
  const finished = isRunFinished(run.status);
  // a failed run opens its stage logs so the failure is visible at once, until the user hides them
  const failed = run.status === ReactionStatus.Failed;
  return (
    <Section data-testid="run-plan">
      <h2>Plan</h2>
      {steps.length === 0 && <Muted>This run has no recorded plan.</Muted>}
      {steps.map((step, index) => (
        <PlanStep key={index}>
          <Muted>
            Step {index + 1}
            {step.length > 1 ? ' (parallel)' : ''}
          </Muted>
          {step.map((stage) => {
            const shown = open.includes(stage.name) || (failed && !closed.includes(stage.name));
            const others = (names: string[]) => names.filter((name) => name !== stage.name);
            return (
              <div key={stage.name} data-testid="plan-stage">
                <Badge $tone={statusTone(run.status)}>{index + 1}</Badge> <Mono>{stage.name}</Mono>{' '}
                <Button
                  size={ButtonSize.XSmall}
                  variant={ButtonVariant.Ghost}
                  data-testid={`toggle-logs-${stage.name}`}
                  onClick={() => {
                    // hiding wins over the failed-run default, and showing again undoes it
                    setOpen(shown ? others(open) : [...open, stage.name]);
                    setClosed(shown ? [...closed, stage.name] : others(closed));
                  }}
                >
                  {shown ? 'Hide logs' : 'Show logs'}
                </Button>
                {shown && <StageLogView group={run.group} reaction={run.id} stage={stage.name} running={!finished} />}
              </div>
            );
          })}
        </PlanStep>
      ))}
    </Section>
  );
};
