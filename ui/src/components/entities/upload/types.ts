/** Matches api/src/models/files.rs — OriginTypes enum */
export enum OriginType {
  Downloaded = 'Downloaded',
  Unpacked = 'Unpacked',
  Transformed = 'Transformed',
  Wire = 'Wire',
  Incident = 'Incident',
  MemoryDump = 'MemoryDump',
  Source = 'Source',
  Carved = 'Carved',
  None = 'None',
}

/** Matches api/src/models/files.rs — CarvedOriginTypes enum */
export enum CarvedOriginType {
  Pcap = 'Pcap',
  Unknown = 'Unknown',
}

/** Matches api/src/models/files.rs — PcapNetworkProtocol enum (serde serialized values) */
export enum PcapNetworkProtocol {
  TCP = 'TCP',
  UDP = 'UDP',
}

export const TLPColors: Record<string, string> = {
  CLEAR: 'tlp-clear',
  GREEN: 'tlp-green',
  AMBER: 'tlp-amber',
  'AMBER+STRICT': 'tlp-amber',
  RED: 'tlp-red',
};

export type TLPState = {
  [key: string]: boolean;
};

export type FileUploadStatus = {
  progress: number;
  size: number;
  type: string;
  msg: string;
  sha256: string;
  fileFail: boolean;
  reactionFail: boolean;
};

export type ReactionResult = {
  id: string;
  sha256: string;
  result: any;
  submission: any;
};
