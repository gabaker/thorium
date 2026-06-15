import React from 'react';
import { OverlayTrigger, Tooltip } from 'react-bootstrap';
import { GoSidebarExpand, GoSidebarCollapse } from 'react-icons/go';

// project imports
import NodeInfo from '../shared/NodeInfo';
import EdgeInfo from '../shared/EdgeInfo';
import type { SelectedElement } from './controls/types';
import type { TreeNode } from '@models/trees';
import { PreviewContainer, PreviewScroll, PreviewToggleButton, PreviewCollapseButton } from './Shared';

// spec: ./AssociationGraph.spec.md

interface DataPreviewPanelProps {
  selectedElement: SelectedElement | null;
  nodeData: TreeNode | undefined;
  minimized: boolean;
  onToggleMinimize: () => void;
}

const DataPreviewPanel: React.FC<DataPreviewPanelProps> = ({ selectedElement, nodeData, minimized, onToggleMinimize }) => {
  if (!selectedElement) return null;

  if (minimized) {
    return (
      <OverlayTrigger placement="left" overlay={<Tooltip>Show Node Info</Tooltip>}>
        <PreviewToggleButton onClick={onToggleMinimize}>
          <GoSidebarExpand size={14} />
        </PreviewToggleButton>
      </OverlayTrigger>
    );
  }

  return (
    <PreviewContainer>
      {/* collapse button floats over the top-right corner, above the content, so it no longer takes a header row */}
      <OverlayTrigger placement="left" overlay={<Tooltip>Minimize</Tooltip>}>
        <PreviewCollapseButton onClick={onToggleMinimize}>
          <GoSidebarCollapse size={14} />
        </PreviewCollapseButton>
      </OverlayTrigger>
      <PreviewScroll>
        {selectedElement.kind === 'node' && nodeData && <NodeInfo node={nodeData} />}
        {selectedElement.kind === 'link' && (
          <EdgeInfo
            edge={{
              data: {
                source: selectedElement.source,
                target: selectedElement.target,
                label: selectedElement.label,
              },
            }}
          />
        )}
      </PreviewScroll>
    </PreviewContainer>
  );
};

export default DataPreviewPanel;
