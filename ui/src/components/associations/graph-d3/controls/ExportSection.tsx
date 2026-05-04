import React from 'react';
import { Button } from 'react-bootstrap';

import { exportJPEG, exportPNG } from '../export';
import type { GraphSectionProps } from './types';
import { PopoverBody } from './Toolbar.styled';

const ExportSection: React.FC<GraphSectionProps> = ({ graphId, graphInstance }) => (
  <PopoverBody>
    <Button variant="" size="sm" className="secondary-btn" style={{ width: '100%' }} onClick={() => exportPNG(graphId, graphInstance)}>
      PNG
    </Button>
    <Button variant="" size="sm" className="secondary-btn" style={{ width: '100%' }} onClick={() => exportJPEG(graphId, graphInstance)}>
      JPEG
    </Button>
  </PopoverBody>
);

export default ExportSection;
