import { AssociationCreate } from '@models';
import { CarvedOriginType, FileUploadStatus, OriginType, ReactionResult } from './types';

/**
 * Matches OriginRequest from api/src/models/files.rs.
 * All optional API fields use empty string / undefined as "not set".
 * `carved_type` and `pcap_url` are UI-only helpers.
 */
export interface OriginState {
  origin_type: OriginType;
  carved_type: CarvedOriginType; // UI-only: sub-type selector for Carved origins
  result_ids: string[];
  url: string;
  name: string;
  tool: string;
  parent: string;
  flags: string[];
  cmd: string;
  sniffer: string;
  source: string;
  destination: string;
  incident: string;
  cover_term: string;
  mission_team: string;
  network: string;
  machine: string;
  location: string;
  memory_type: string;
  reconstructed: string[];
  base_addr: string;
  repo: string;
  commitish: string;
  commit: string;
  system: string;
  supporting: boolean | undefined;
  src_ip: string;
  dest_ip: string;
  src_port: number | undefined;
  dest_port: number | undefined;
  proto: string;
  pcap_url: string; // UI-only: mapped to origin[url] for CarvedPcap submissions
}

const initialOrigin: OriginState = {
  origin_type: OriginType.Downloaded,
  carved_type: CarvedOriginType.Pcap,
  result_ids: [],
  url: '',
  name: '',
  tool: '',
  parent: '',
  flags: [],
  cmd: '',
  sniffer: '',
  source: '',
  destination: '',
  incident: '',
  cover_term: '',
  mission_team: '',
  network: '',
  machine: '',
  location: '',
  memory_type: '',
  reconstructed: [],
  base_addr: '',
  repo: '',
  commitish: '',
  commit: '',
  system: '',
  supporting: undefined,
  src_ip: '',
  dest_ip: '',
  src_port: undefined,
  dest_port: undefined,
  proto: '',
  pcap_url: '',
};

export interface UploadFormState {
  // Form fields (match SampleRequest multipart structure)
  files: any[];
  description: string;
  groups: string[];
  tags: Array<{ key: string; value: string }>;
  tlp: Record<string, boolean>;
  origin: OriginState;
  trigger_depth: number;
  associations: AssociationCreate[];
  reactionsList: any[];

  // Upload tracking
  uploadError: string[];
  runReactionsRes: string;
  uploadSHA256: string[];
  uploadInProgress: boolean;
  activeUploads: any[];
  uploadStatus: Record<string, FileUploadStatus>;
  uploadFailures: Record<string, any>;
  uploadStatusDropdown: Record<string, boolean>;
  uploadReactionRes: ReactionResult[];
  uploadReactions: Record<string, any[]>;
  uploadReactionFailures: number;
  totalUploadSize: number;
  showUploadStatus: boolean;
  controller: AbortController;
}

export type UploadFormAction =
  // Form field actions
  | { type: 'SET_FILES'; payload: any[] }
  | { type: 'SET_DESCRIPTION'; payload: string }
  | { type: 'SET_GROUPS'; payload: string[] }
  | { type: 'SET_TAGS'; payload: Array<{ key: string; value: string }> }
  | { type: 'SET_TLP'; payload: Record<string, boolean> }
  | { type: 'SET_ORIGIN_FIELD'; payload: Partial<OriginState> }
  | { type: 'SET_TRIGGER_DEPTH'; payload: number }
  | { type: 'SET_ASSOCIATIONS'; payload: AssociationCreate[] }
  | { type: 'SET_REACTIONS_LIST'; payload: any[] }

  // Upload tracking actions
  | { type: 'SET_UPLOAD_ERROR'; payload: string[] }
  | { type: 'ADD_UPLOAD_ERROR'; payload: string[] }
  | { type: 'SET_RUN_REACTIONS_RES'; payload: string }
  | { type: 'SET_UPLOAD_SHA256'; payload: string[] }
  | { type: 'SET_UPLOAD_IN_PROGRESS'; payload: boolean }
  | { type: 'SET_ACTIVE_UPLOADS'; payload: any[] }
  | { type: 'ADD_ACTIVE_UPLOAD'; payload: any }
  | { type: 'REMOVE_ACTIVE_UPLOAD'; payload: any }
  | { type: 'SET_UPLOAD_STATUS'; payload: Record<string, FileUploadStatus> }
  | { type: 'UPDATE_UPLOAD_STATUS'; payload: { key: string; value: FileUploadStatus } }
  | { type: 'SET_UPLOAD_FAILURES'; payload: Record<string, any> }
  | { type: 'UPDATE_UPLOAD_FAILURES'; payload: { key: string; value?: any } }
  | { type: 'SET_UPLOAD_STATUS_DROPDOWN'; payload: Record<string, boolean> }
  | { type: 'UPDATE_UPLOAD_STATUS_DROPDOWN'; payload: { key: string; value: boolean } }
  | { type: 'SET_UPLOAD_REACTION_RES'; payload: ReactionResult[] }
  | { type: 'ADD_UPLOAD_REACTION_RES'; payload: ReactionResult }
  | { type: 'UPDATE_UPLOAD_REACTION_RES'; payload: ReactionResult }
  | { type: 'SET_UPLOAD_REACTIONS'; payload: Record<string, any[]> }
  | { type: 'UPDATE_UPLOAD_REACTIONS'; payload: { key: string; value: any[] } }
  | { type: 'SET_UPLOAD_REACTION_FAILURES'; payload: number }
  | { type: 'INCREMENT_UPLOAD_REACTION_FAILURES'; payload: number }
  | { type: 'DECREMENT_UPLOAD_REACTION_FAILURES'; payload?: void }
  | { type: 'SET_TOTAL_UPLOAD_SIZE'; payload: number }
  | { type: 'SET_SHOW_UPLOAD_STATUS'; payload: boolean }
  | { type: 'SET_CONTROLLER'; payload: AbortController }

  // Composite actions
  | { type: 'RESET_STATUS_MESSAGES' }
  | { type: 'RESET_UPLOAD_FAILURES' };

export const initialUploadFormState = (entity: any): UploadFormState => ({
  files: [],
  description: '',
  groups: entity ? entity.groups : [],
  tags: [{ key: '', value: '' }],
  tlp: {
    Red: false,
    White: false,
    Amber: false,
    Green: false,
  },
  origin: { ...initialOrigin },
  trigger_depth: 0,
  associations: [],
  reactionsList: [],

  uploadError: [],
  runReactionsRes: '',
  uploadSHA256: [],
  uploadInProgress: false,
  activeUploads: [],
  uploadStatus: {},
  uploadFailures: {},
  uploadStatusDropdown: {},
  uploadReactionRes: [],
  uploadReactions: {},
  uploadReactionFailures: 0,
  totalUploadSize: 0,
  showUploadStatus: false,
  controller: new AbortController(),
});

/** Merges a key-value pair into a record, or removes the key if value is undefined */
function updateRecord<T>(record: Record<string, T>, key: string, value: T | undefined): Record<string, T> {
  if (value === undefined) {
    const { [key]: _, ...rest } = record;
    return rest;
  }
  return { ...record, [key]: value };
}

export const uploadFormReducer = (state: UploadFormState, action: UploadFormAction): UploadFormState => {
  switch (action.type) {
    // Form fields
    case 'SET_FILES':
      return { ...state, files: action.payload };
    case 'SET_DESCRIPTION':
      return { ...state, description: action.payload };
    case 'SET_GROUPS':
      return { ...state, groups: action.payload };
    case 'SET_TAGS':
      return { ...state, tags: action.payload };
    case 'SET_TLP':
      return { ...state, tlp: action.payload };
    case 'SET_ORIGIN_FIELD':
      return { ...state, origin: { ...state.origin, ...action.payload } };
    case 'SET_TRIGGER_DEPTH':
      return { ...state, trigger_depth: action.payload };
    case 'SET_ASSOCIATIONS':
      return { ...state, associations: action.payload };
    case 'SET_REACTIONS_LIST':
      return { ...state, reactionsList: action.payload };

    // Upload tracking
    case 'SET_UPLOAD_ERROR':
      return { ...state, uploadError: action.payload };
    case 'ADD_UPLOAD_ERROR':
      return { ...state, uploadError: [...state.uploadError, ...action.payload] };
    case 'SET_RUN_REACTIONS_RES':
      return { ...state, runReactionsRes: action.payload };
    case 'SET_UPLOAD_SHA256':
      return { ...state, uploadSHA256: action.payload };
    case 'SET_UPLOAD_IN_PROGRESS':
      return { ...state, uploadInProgress: action.payload };
    case 'SET_ACTIVE_UPLOADS':
      return { ...state, activeUploads: action.payload };
    case 'ADD_ACTIVE_UPLOAD':
      return { ...state, activeUploads: [...state.activeUploads, action.payload] };
    case 'REMOVE_ACTIVE_UPLOAD':
      return { ...state, activeUploads: state.activeUploads.filter((item) => item !== action.payload) };
    case 'SET_UPLOAD_STATUS':
      return { ...state, uploadStatus: action.payload };
    case 'UPDATE_UPLOAD_STATUS':
      return { ...state, uploadStatus: updateRecord(state.uploadStatus, action.payload.key, action.payload.value) };
    case 'SET_UPLOAD_FAILURES':
      return { ...state, uploadFailures: action.payload };
    case 'UPDATE_UPLOAD_FAILURES':
      return { ...state, uploadFailures: updateRecord(state.uploadFailures, action.payload.key, action.payload.value) };
    case 'SET_UPLOAD_STATUS_DROPDOWN':
      return { ...state, uploadStatusDropdown: action.payload };
    case 'UPDATE_UPLOAD_STATUS_DROPDOWN':
      return { ...state, uploadStatusDropdown: updateRecord(state.uploadStatusDropdown, action.payload.key, action.payload.value) };
    case 'SET_UPLOAD_REACTION_RES':
      return { ...state, uploadReactionRes: action.payload };
    case 'ADD_UPLOAD_REACTION_RES':
      return { ...state, uploadReactionRes: [...state.uploadReactionRes, action.payload] };
    case 'UPDATE_UPLOAD_REACTION_RES':
      return {
        ...state,
        uploadReactionRes: state.uploadReactionRes.map((result) => (result.id === action.payload.id ? action.payload : result)),
      };
    case 'SET_UPLOAD_REACTIONS':
      return { ...state, uploadReactions: action.payload };
    case 'UPDATE_UPLOAD_REACTIONS':
      return { ...state, uploadReactions: updateRecord(state.uploadReactions, action.payload.key, action.payload.value) };
    case 'SET_UPLOAD_REACTION_FAILURES':
      return { ...state, uploadReactionFailures: action.payload };
    case 'INCREMENT_UPLOAD_REACTION_FAILURES':
      return { ...state, uploadReactionFailures: state.uploadReactionFailures + action.payload };
    case 'DECREMENT_UPLOAD_REACTION_FAILURES':
      return { ...state, uploadReactionFailures: state.uploadReactionFailures - 1 };
    case 'SET_TOTAL_UPLOAD_SIZE':
      return { ...state, totalUploadSize: action.payload };
    case 'SET_SHOW_UPLOAD_STATUS':
      return { ...state, showUploadStatus: action.payload };
    case 'SET_CONTROLLER':
      return { ...state, controller: action.payload };

    // Composite actions
    case 'RESET_STATUS_MESSAGES':
      return {
        ...state,
        uploadSHA256: [],
        uploadError: [],
        runReactionsRes: '',
        uploadStatus: {},
        uploadReactions: {},
        uploadReactionRes: [],
        uploadReactionFailures: 0,
      };
    case 'RESET_UPLOAD_FAILURES':
      return { ...state, uploadFailures: {} };

    default:
      return state;
  }
};
