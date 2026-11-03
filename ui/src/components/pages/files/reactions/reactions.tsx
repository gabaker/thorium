import { ReactNode } from 'react';

// project imports
import { Badge, statusTone } from '@components/shared/badges/Badge';
import { createReaction, deleteReaction } from '@thorpi/reactions';
import { ReactionRequest, ReactionRunResult, ReactionSelection } from '@models/reactions';

type SelectedPipelines = Record<string, Record<string, boolean>>;

// get the status badge of a given reaction/job, in the shared status tone
const getStatusBadge = (status: string): ReactNode => {
  // the API names a failed job either Failed or Errored; both read as Failed
  const label = status === 'Errored' ? 'Failed' : status;
  return <Badge tone={statusTone(label)}>{label}</Badge>;
};

// build a list of reactions from a pipelines details and selected pipelines
const buildReactionsList = (selectedPipelines: SelectedPipelines, tags?: Record<string, string[]>) => {
  const reactionList: Array<{
    pipeline: string;
    group: string;
    args: Record<string, never>;
    sla: number;
    tags?: Record<string, string[]>;
  }> = [];
  Object.keys(selectedPipelines).map((group) => {
    Object.keys(selectedPipelines[group]).map((pipeline) => {
      if (selectedPipelines[group][pipeline]) {
        const body: {
          pipeline: string;
          group: string;
          args: Record<string, never>;
          sla: number;
          tags?: Record<string, string[]>;
        } = {
          pipeline: pipeline,
          group: group,
          args: {},
          sla: 30,
        };
        if (tags && Object.keys(tags).length > 0) {
          body.tags = tags;
        }
        reactionList.push(body);
      }
    });
  });
  return reactionList;
};

// submit reactions for a sha256
const submitReactions = async (sha256: string, reactionList: ReactionSelection[]): Promise<ReactionRunResult[]> => {
  const reactionRunResults: ReactionRunResult[] = [];
  for (const reaction of reactionList) {
    const request = {
      ...reaction,
      samples: [sha256],
      tags: reaction.tags
        ? Object.entries(reaction.tags)
            .flat()
            .filter((v): v is string => typeof v === 'string')
        : [],
    } as ReactionRequest;

    const handleReactionCreationFailure = (error: string) => {
      reactionRunResults.push({
        error: 'Failed to submit ' + reaction.pipeline + ' for ' + sha256 + ': ' + error,
        group: reaction.group,
        pipeline: reaction.pipeline,
      });
    };

    const res = await createReaction(request, handleReactionCreationFailure);
    if (res) {
      reactionRunResults.push({
        id: res.id,
        error: '',
        group: reaction.group,
        pipeline: reaction.pipeline,
      });
    }
  }
  return reactionRunResults;
};

export { buildReactionsList, submitReactions, deleteReaction, getStatusBadge };
export type { SelectedPipelines };
