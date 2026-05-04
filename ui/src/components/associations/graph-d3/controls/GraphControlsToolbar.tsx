import React, { useCallback, useEffect, useRef, useState } from 'react';
import { FaCog, FaProjectDiagram, FaBolt, FaCircle, FaCamera, FaLongArrowAltRight } from 'react-icons/fa';
import type { ForceGraph3DInstance } from '3d-force-graph';

import type { GraphControls, DisplayAction, SectionKey } from './types';
import { ToolbarContainer, ToolbarIconButton, NodeCount } from './Toolbar.styled';
import ToolbarButton from './ToolbarButton';
import GraphSection from './GraphSection';
import ForcesSection from './ForcesSection';
import NodesSection from './NodesSection';
import EdgesSection from './EdgesSection';
import ExportSection from './ExportSection';

interface GraphControlsToolbarProps {
  graphId: string;
  controls: GraphControls;
  updateControls: React.ActionDispatch<[action: DisplayAction]>;
  graphInstance: ForceGraph3DInstance | null;
  nodeCount: number;
}

const GraphControlsToolbar: React.FC<GraphControlsToolbarProps> = ({ graphId, controls, updateControls, graphInstance, nodeCount }) => {
  const [isOpen, setIsOpen] = useState(false);
  const [activeSection, setActiveSection] = useState<SectionKey | null>(null);
  const toolbarRef = useRef<HTMLDivElement>(null);

  const handleToggleSection = (key: SectionKey) => {
    setActiveSection((prev) => (prev === key ? null : key));
  };

  const handleGearToggle = () => {
    if (isOpen) {
      setActiveSection(null);
    }
    setIsOpen((prev) => !prev);
  };

  // Close the active popover when clicking outside the toolbar
  const handleClickOutside = useCallback(
    (e: MouseEvent) => {
      if (activeSection && toolbarRef.current && !toolbarRef.current.contains(e.target as Node)) {
        // Don't close if the click is inside a popover (they render in a portal)
        const popover = (e.target as Element).closest('.popover');
        if (!popover) {
          setActiveSection(null);
        }
      }
    },
    [activeSection],
  );

  useEffect(() => {
    document.addEventListener('mousedown', handleClickOutside);
    return () => document.removeEventListener('mousedown', handleClickOutside);
  }, [handleClickOutside]);

  return (
    <ToolbarContainer ref={toolbarRef}>
      <ToolbarIconButton $active={isOpen} onClick={handleGearToggle} title="Toggle controls">
        <FaCog size={16} />
      </ToolbarIconButton>

      {isOpen && (
        <>
          <ToolbarButton sectionKey="graph" activeSection={activeSection} onToggle={handleToggleSection} icon={<FaProjectDiagram size={14} />} title="Graph">
            <GraphSection graphId={graphId} controls={controls} updateControls={updateControls} graphInstance={graphInstance} />
          </ToolbarButton>

          <ToolbarButton sectionKey="forces" activeSection={activeSection} onToggle={handleToggleSection} icon={<FaBolt size={14} />} title="Forces">
            <ForcesSection controls={controls} updateControls={updateControls} />
          </ToolbarButton>

          <ToolbarButton sectionKey="nodes" activeSection={activeSection} onToggle={handleToggleSection} icon={<FaCircle size={14} />} title="Nodes">
            <NodesSection controls={controls} updateControls={updateControls} />
          </ToolbarButton>

          <ToolbarButton sectionKey="edges" activeSection={activeSection} onToggle={handleToggleSection} icon={<FaLongArrowAltRight size={14} />} title="Edges">
            <EdgesSection controls={controls} updateControls={updateControls} />
          </ToolbarButton>

          <ToolbarButton sectionKey="export" activeSection={activeSection} onToggle={handleToggleSection} icon={<FaCamera size={14} />} title="Export">
            <ExportSection graphId={graphId} controls={controls} updateControls={updateControls} graphInstance={graphInstance} />
          </ToolbarButton>
        </>
      )}

      <NodeCount>Nodes: {nodeCount}</NodeCount>
    </ToolbarContainer>
  );
};

export default GraphControlsToolbar;
