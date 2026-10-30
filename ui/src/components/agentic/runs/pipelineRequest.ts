// project imports
import { parsePositiveInt } from '@utilities/agentic/forms';
import { Pipeline } from '@models/pipelines';
import { ReactionRequest, RepoDependencyRequest } from '@models/reactions';

/** The values of the pipeline run form. */
export interface PipelineRunInputs {
  /** The pipeline's group */
  group: string;
  /** The pipeline's name */
  pipeline: string;
  /** The samples to run on */
  samples: string[];
  /** The repos to run on */
  repos: RepoDependencyRequest[];
  /** Tags as typed, comma separated */
  tags: string;
  /** The SLA as typed */
  sla: string;
}

/**
 * Build the reaction request that runs a pipeline.
 *
 * @param inputs - The form values.
 * @returns The request for `POST /reactions/`; blank tags and SLA are left out.
 */
export function buildPipelineRunRequest(inputs: PipelineRunInputs): ReactionRequest {
  const sla = parsePositiveInt(inputs.sla);
  const request: ReactionRequest = {
    group: inputs.group,
    pipeline: inputs.pipeline,
    args: {},
    tags: inputs.tags
      .split(',')
      .map((tag) => tag.trim())
      .filter((tag) => tag !== ''),
    samples: inputs.samples,
    buffers: {},
    repos: inputs.repos,
    cache: { generic: {}, files: [] },
  };
  if (sla !== undefined) request.sla = sla;
  return request;
}

/**
 * Tell why a pipeline can't be run, if it can't.
 *
 * @param pipeline - The pipeline.
 * @returns A message when the pipeline is banned or a stage has no tool, otherwise `null`.
 */
export function pipelineBlocker(pipeline: Pick<Pipeline, 'bans' | 'order' | 'stage_tools'>): string | null {
  if (pipeline.bans && Object.keys(pipeline.bans).length > 0) return 'The pipeline has bans and cannot be run';
  const missing = pipeline.order.flat().filter((stage) => !(pipeline.stage_tools && stage in pipeline.stage_tools));
  if (missing.length > 0) return `These stages name an image with no tool: ${missing.join(', ')}`;
  return null;
}
