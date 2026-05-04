import React from 'react';
import { Card, Tab, Tabs } from 'react-bootstrap';
import { OriginState } from './types';
import OriginDownloaded from './OriginDownloaded';
import OriginTransformed from './OriginTransformed';
import OriginUnpacked from './OriginUnpacked';
import OriginCarved from './OriginCarved';
import OriginWire from './OriginWire';
import OriginIncident from './OriginIncident';
import OriginMemoryDump from './OriginMemoryDump';

interface OriginFormProps {
  originState: OriginState;
  onOriginChange: (field: keyof OriginState, value: string) => void;
  onResetStatus: () => void;
}

const OriginForm: React.FC<OriginFormProps> = ({ originState, onOriginChange, onResetStatus }) => {
  return (
    <Card className="panel">
      <Card.Body>
        <Tabs fill activeKey={originState.originType} onSelect={(k) => onOriginChange('originType', k || 'Downloaded')}>
          <Tab eventKey="Downloaded" title="Downloaded">
            <OriginDownloaded originState={originState} onOriginChange={onOriginChange} onResetStatus={onResetStatus} />
          </Tab>
          <Tab eventKey="Transformed" title="Transformed">
            <OriginTransformed originState={originState} onOriginChange={onOriginChange} onResetStatus={onResetStatus} />
          </Tab>
          <Tab eventKey="Unpacked" title="Unpacked">
            <OriginUnpacked originState={originState} onOriginChange={onOriginChange} onResetStatus={onResetStatus} />
          </Tab>
          <Tab eventKey="Carved" title="Carved">
            <OriginCarved originState={originState} onOriginChange={onOriginChange} onResetStatus={onResetStatus} />
          </Tab>
          <Tab eventKey="Wire" title="Wire">
            <OriginWire originState={originState} onOriginChange={onOriginChange} onResetStatus={onResetStatus} />
          </Tab>
          <Tab eventKey="Incident" title="Incident">
            <OriginIncident originState={originState} onOriginChange={onOriginChange} onResetStatus={onResetStatus} />
          </Tab>
          <Tab eventKey="MemoryDump" title="Memory Dump">
            <OriginMemoryDump originState={originState} onOriginChange={onOriginChange} onResetStatus={onResetStatus} />
          </Tab>
        </Tabs>
      </Card.Body>
    </Card>
  );
};

export default OriginForm;
