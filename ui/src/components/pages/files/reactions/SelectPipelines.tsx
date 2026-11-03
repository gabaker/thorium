import { Fragment, useEffect, useState } from 'react';
import styled from 'styled-components';

// project imports
import { orderComparePipelineName } from './pipelines';
import { buildReactionsList, SelectedPipelines } from './reactions';
import AlertBanner from '@components/shared/alerts/AlertBanner';
import { Button, ButtonVariant } from '@components/shared/buttons';
import Title from '@components/shared/titles/Title';
import Markdown from '@components/shared/syntax/Markdown';
import { OverlayTipTop } from '@components/shared/overlay/tips';
import { cleanDescription } from '@utilities/description';
import { listPipelines } from '@thorpi/pipelines';
import { spacers } from '@styles';
import type { Pipeline } from '@models/pipelines';
import type { ReactionSelection } from '@models/reactions';
import type { UserInfo } from '@models/users';

/** The pipeline picker's panel: each group's name over its pipeline buttons, centred. */
const PickerPanel = styled.div`
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: ${spacers.three};
  padding: ${spacers.four};
  text-align: center;
  border: 1px solid var(--thorium-panel-border);
  border-radius: 6px;
  color: var(--thorium-text);
  background-color: var(--thorium-panel-bg);
`;

/** One group's pipeline buttons, wrapped onto as many lines as they need. */
const PipelineButtons = styled.div`
  display: flex;
  flex-wrap: wrap;
  justify-content: center;
  gap: ${spacers.three};
`;

interface SelectPipelinesProps {
  userInfo: UserInfo | null;
  setReactionsList: (list: ReactionSelection[]) => void;
  setError?: (errors: string[]) => void;
  currentSelections?: ReactionSelection[];
  sha256?: string;
}

const SelectPipelines = ({ userInfo, setReactionsList, setError, currentSelections }: SelectPipelinesProps) => {
  const [pipelines, setPipelines] = useState<Record<string, Pipeline[]>>({});
  const [selectedPipelines, setSelectedPipelines] = useState<SelectedPipelines>({});
  const [pipelinesListErrors, setPipelinesListErrors] = useState<string[]>([]);

  // get detailed pipelines info
  useEffect(() => {
    let isSubscribed = true;
    const fetchData = async () => {
      const allPipelines: Record<string, Pipeline[]> = {};
      const selectablePipelines: SelectedPipelines = {};
      const errors: string[] = [];
      if (userInfo && userInfo.groups) {
        for (const group of userInfo.groups) {
          const groupPipelines = (await listPipelines(group, (error: string) => errors.push(error), true)) as Pipeline[] | null;
          if (groupPipelines) {
            allPipelines[group] = [...groupPipelines];
            groupPipelines.forEach((pipeline: Pipeline) => {
              if (pipeline.group in selectablePipelines) {
                selectablePipelines[pipeline.group][pipeline.name] = false;
              } else {
                selectablePipelines[pipeline.group] = { [pipeline.name]: false };
              }
            });
          }
        }
      }
      if (currentSelections) {
        currentSelections.forEach((selection) => {
          if (!(selection.group in selectablePipelines)) {
            selectablePipelines[selection.group] = {};
          }
          selectablePipelines[selection.group][selection.pipeline] = true;
        });
      }
      setPipelinesListErrors(errors);
      if (isSubscribed) {
        setPipelines(allPipelines);
        setSelectedPipelines(selectablePipelines);
      }
    };
    void fetchData();
    return () => {
      isSubscribed = false;
    };
  }, [userInfo]);

  // handle pipeline button click
  const handlePipelineClick = (group: string, pipelineName: string) => {
    const selected = structuredClone(selectedPipelines);
    selected[group][pipelineName] = !selectedPipelines[group][pipelineName];
    setSelectedPipelines(selected);
    setReactionsList(buildReactionsList(selected));
    if (setError) setError([]);
  };

  // a pipeline's toggle button, drawn filled while the pipeline is selected
  const pipelineButton = (pipeline: Pipeline) => {
    const selected = selectedPipelines[pipeline.group]?.[pipeline.name] ?? false;
    return (
      <Button
        variant={selected ? ButtonVariant.Primary : ButtonVariant.Secondary}
        aria-pressed={selected}
        onClick={() => handlePipelineClick(pipeline.group, pipeline.name)}
      >
        <b>{pipeline.name}</b>
      </Button>
    );
  };

  return (
    <PickerPanel>
      {pipelinesListErrors.map((error, idx) => (
        <AlertBanner key={`pipeline-list-error-${idx}`}>{error}</AlertBanner>
      ))}
      {userInfo?.groups?.map((group) => {
        if (!pipelines[group]?.length) return null;
        return (
          <div key={group}>
            <Title small>{group}</Title>
            <PipelineButtons>
              {pipelines[group]
                .sort((a, b) => orderComparePipelineName(a, b))
                .map((pipeline) => {
                  const description = cleanDescription(pipeline.description);
                  const key = `${pipeline.group}_${pipeline.name}`;
                  return description ? (
                    <OverlayTipTop key={key} wide tip={(<Markdown>{description}</Markdown>) as unknown as string}>
                      {pipelineButton(pipeline)}
                    </OverlayTipTop>
                  ) : (
                    <Fragment key={key}>{pipelineButton(pipeline)}</Fragment>
                  );
                })}
            </PipelineButtons>
          </div>
        );
      })}
    </PickerPanel>
  );
};

export default SelectPipelines;
