import { Tags } from '@models/tags';
import { Entity, Entities, EntityTypes } from '@models/entities/entities';
import { Sample } from '@models/files';

export interface IncidentTag {
  key: string;
  value: string;
}

export interface IncidentSummaryProps {
  incidentTag: IncidentTag;
}

// Stubbed Incident entity — mirrors the Entity pattern used by Device/Vendor/Collection.
// Once the backend supports Incident as a first-class entity kind, this can move to
// @models/entities/incidents.ts and Entities enum can be extended.
export interface IncidentMeta {
  cover_term?: string;
  mission_team?: string;
  network?: string;
  severity?: 'critical' | 'high' | 'medium' | 'low' | 'informational';
  status?: 'active' | 'contained' | 'eradicated' | 'recovered' | 'closed';
  started?: string;
  closed?: string;
}

export interface Incident extends Omit<EntityTypes, 'kind' | 'metadata'> {
  kind: 'Incident';
  metadata: IncidentMeta;
}

export interface NodeTypeCounts {
  files: number;
  repos: number;
  tags: number;
  entities: Map<string, number>;
}

export interface FileExtensionCount {
  extension: string;
  count: number;
}

export interface IncidentSummaryData {
  loading: boolean;
  error: string | null;
  files: Sample[];
  nodeTypeCounts: NodeTypeCounts;
  fileExtensions: FileExtensionCount[];
  totalNodes: number;
}
