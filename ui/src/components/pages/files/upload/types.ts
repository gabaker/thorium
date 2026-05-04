import { AssociationCreate, AssociationKind, AssociationTarget, BlankAssociationCreate } from '@models/associations';
import { Entity } from '@models/entities';
import { TagEntry } from '@models/tags';

export { AssociationKind, BlankAssociationCreate };
export type { AssociationCreate, AssociationTarget, Entity, TagEntry };

export const PARALLEL_UPLOAD_LIMIT = 5;

export const TLP_COLORS: Record<string, string> = {
  CLEAR: 'tlp-clear',
  GREEN: 'tlp-green',
  AMBER: 'tlp-amber',
  'AMBER+STRICT': 'tlp-amber',
  RED: 'tlp-red',
};

export type TLPSelection = Record<string, boolean>;

export const DEFAULT_TLP_SELECTION: TLPSelection = Object.fromEntries(
  Object.keys(TLP_COLORS).map((key) => [key, false]),
);

export type OriginType = 'Downloaded' | 'Transformed' | 'Unpacked' | 'Carved' | 'Wire' | 'Incident' | 'MemoryDump';
export type CarvedSubType = 'Pcap' | 'Unknown';

export interface OriginState {
  originType: OriginType;
  carvedType: CarvedSubType;
  originUrl: string;
  originName: string;
  originTool: string;
  originParentFile: string;
  originToolFlags: string;
  originSniffer: string;
  originSource: string;
  originDestination: string;
  originIncident: string;
  originCoverTerm: string;
  originMissionTeam: string;
  originNetwork: string;
  originMachine: string;
  originLocation: string;
  originMemoryType: string;
  originReconstructed: string;
  originBaseAddress: string;
  originSourceIp: string;
  originDestinationIp: string;
  originSourcePort: string;
  originDestinationPort: string;
  originProtocol: string;
  originCarvedPcapUrl: string;
}

export const DEFAULT_ORIGIN_STATE: OriginState = {
  originType: 'Downloaded',
  carvedType: 'Pcap',
  originUrl: '',
  originName: '',
  originTool: '',
  originParentFile: '',
  originToolFlags: '',
  originSniffer: '',
  originSource: '',
  originDestination: '',
  originIncident: '',
  originCoverTerm: '',
  originMissionTeam: '',
  originNetwork: '',
  originMachine: '',
  originLocation: '',
  originMemoryType: '',
  originReconstructed: '',
  originBaseAddress: '',
  originSourceIp: '',
  originDestinationIp: '',
  originSourcePort: '',
  originDestinationPort: '',
  originProtocol: '',
  originCarvedPcapUrl: '',
};

export interface FileUploadStatus {
  progress: number;
  size: number;
  type: 'info' | 'success' | 'warning' | 'danger';
  msg: string;
  sha256: string;
  fileFail: boolean;
  reactionFail: boolean;
}

export interface ReactionSubmitResult {
  id?: string;
  error: string;
  group: string;
  pipeline: string;
  path?: string;
  size?: number;
  sha256?: string;
}

export interface ReactionResultEntry {
  id: string;
  sha256: string;
  result: ReactionSubmitResult;
  submission: FileSubmission;
}

export interface FileSubmission {
  path: string;
  size: number;
}

export interface DropzoneFile extends File {
  readonly path: string;
}

export interface UploadProps {
  entity: Entity | undefined;
}
