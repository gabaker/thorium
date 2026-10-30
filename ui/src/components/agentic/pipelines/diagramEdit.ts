// project imports
import { StageToolOption, uniqueStageName } from './stages';
import { getImagesInOrder, insertImageAtPosition, removeImageAtPosition } from '@components/shared/pipeline/order';
import { Pipeline } from '@models/pipelines';

/** A pipeline order: a stage name runs alone in its step and a list of names runs in parallel. */
export type PipelineOrder = Pipeline['order'];

/** The part of a pipeline the order diagram edits: its stages and the tool behind each one. */
export interface PipelineEdit {
  /** The order of the stages */
  order: PipelineOrder;
  /** The id of the tool or pinned tool instance behind each stage, by stage name */
  stageTools: Record<string, string>;
}

/**
 * Drop the stage tools of stages that are no longer in the order.
 *
 * @param order - The pipeline's order.
 * @param stageTools - The stage tools, by stage name.
 * @returns The stage tools of the stages in the order; entries of other stages are left out.
 */
export function syncStageTools(order: PipelineOrder, stageTools: Record<string, string>): Record<string, string> {
  const stages = getImagesInOrder(order);
  return Object.fromEntries(Object.entries(stageTools).filter(([stage]) => stages.has(stage)));
}

/**
 * Add a stage backed by a tool, as the diagram's add and insert menus do.
 *
 * The stage is named after the tool (made unique among the pipeline's stages) and placed where the
 * diagram was clicked: in a new step, or in parallel with a nearby step.
 *
 * @param edit - The pipeline as it is now.
 * @param option - The tool, or pinned tool version, that backs the new stage.
 * @param flowX - The diagram x position that was clicked.
 * @returns The pipeline with the stage in its order and its tool in the stage tools.
 */
export function addToolStage(edit: PipelineEdit, option: StageToolOption, flowX: number): PipelineEdit {
  const name = uniqueStageName(option.base, getImagesInOrder(edit.order));
  return {
    order: insertImageAtPosition(edit.order, name, flowX),
    stageTools: { ...syncStageTools(edit.order, edit.stageTools), [name]: option.value },
  };
}

/**
 * Remove a stage and its tool.
 *
 * @param edit - The pipeline as it is now.
 * @param step - The index of the stage's step.
 * @param parallel - The index of the stage within its step.
 * @returns The pipeline without the stage; a stage with no tool is removed the same way.
 */
export function removeToolStage(edit: PipelineEdit, step: number, parallel: number): PipelineEdit {
  const order = removeImageAtPosition(edit.order, step, parallel);
  return { order, stageTools: syncStageTools(order, edit.stageTools) };
}

/**
 * Reorder the stages, keeping each one's tool.
 *
 * @param edit - The pipeline as it is now.
 * @param order - The new order, made of the same stages.
 * @returns The pipeline with the new order and the stage tools it already had.
 */
export function reorderStages(edit: PipelineEdit, order: PipelineOrder): PipelineEdit {
  return { order, stageTools: syncStageTools(order, edit.stageTools) };
}
