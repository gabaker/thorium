import React from 'react';
import { GoSidebarExpand, GoSidebarCollapse } from 'react-icons/go';

import NodeInfo from '../graph/NodeInfo';
import EdgeInfo from '../graph/EdgeInfo';
import type { SelectedElement } from './controls/types';
import type { TreeNode } from '@models/trees';
import { PreviewContainer, PreviewHeader, PreviewToggleButton, MinimizeButton } from './AssociationGraph3D.styled';

interface DataPreviewPanelProps {
  selectedElement: SelectedElement | null;
  nodeData: TreeNode | undefined;
  minimized: boolean;
  onToggleMinimize: () => void;
}

const DataPreviewPanel: React.FC<DataPreviewPanelProps> = ({
  selectedElement,
  nodeData,
  minimized,
  onToggleMinimize,
}) => {
  if (!selectedElement) return null;

  if (minimized) {
    return (
      <PreviewToggleButton onClick={onToggleMinimize}>
        <GoSidebarExpand size={14} />
      </PreviewToggleButton>
    );
  }

  return (
    <PreviewContainer>
      <PreviewHeader>
        <MinimizeButton onClick={onToggleMinimize}>
          <GoSidebarCollapse size={14} />
        </MinimizeButton>
      </PreviewHeader>
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
    </PreviewContainer>
  );
};

export default DataPreviewPanel;
