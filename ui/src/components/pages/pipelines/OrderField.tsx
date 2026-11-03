import React from 'react';
import styled from 'styled-components';

// project imports
import PipelineOrderFlow from '@components/shared/pipeline/PipelineOrderFlow';

const Note = styled.div`
  color: var(--thorium-secondary-text);
  font-size: 13px;
  margin: 4px 0;
`;

/// Holds the diagram clear of the row that follows the Order field
const DiagramSpacer = styled.div`
  margin-bottom: 0.75rem;
`;

interface OrderFieldProps {
  /// The current pipeline order (stages of images, parallel stages are nested arrays)
  order: (string | string[])[];
  /// The tool (or pinned tool instance) id behind each stage, by stage name
  stageTools?: Record<string, string>;
  /// Called with the updated order and the stage tools that go with it whenever the user edits it
  onChange: (order: (string | string[])[], stageTools: Record<string, string>) => void;
  /// The group whose tools can back the stages added to the order
  group: string;
  /// The set of banned image names to flag in the diagram
  bannedImages?: Set<string>;
}

/// Editable pipeline order field. Renders the order diagram (which handles an empty order in
/// edit mode) so every stage is added on the diagram, each backed by one of the group's tools.
const OrderField: React.FC<OrderFieldProps> = ({ order, stageTools, onChange, group, bannedImages }) => {
  // the diagram's add menu needs the group's tools, so a group must be chosen first
  if (!group) {
    return <Note>Select a group to configure the pipeline order.</Note>;
  }
  return (
    <>
      <Note>Double-click to add a stage backed by a tool · right-click a node to insert/remove · drag a node to reorder.</Note>
      <Note>Drag the background to pan · scroll to zoom.</Note>
      <DiagramSpacer>
        <PipelineOrderFlow order={order} stageTools={stageTools} onOrderChange={onChange} bannedImages={bannedImages} group={group} />
      </DiagramSpacer>
    </>
  );
};

export default OrderField;
