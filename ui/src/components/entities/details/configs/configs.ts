import { JSX } from 'react';

// project imports
import { createEntityDetailsPage, MetadataComponent } from '../EntityDetails';
import CollectionDetailsConfig from './CollectionDetailsConfig';
import DeviceDetailsConfig from './DeviceDetailsConfig';
import FolderDetailsConfig from './FolderDetailsConfig';
import FileSystemDetailsConfig from './FileSystemDetailsConfig';
import VendorDetailsConfig from './VendorDetailsConfig';
import WindowsProcessTreeDetailsConfig from './WindowsProcessTreeDetailsConfig';
import WindowsProcessDetailsConfig from './WindowsProcessDetailsConfig';
import NetworkConnectionsDetailsConfig from './NetworkConnectionDetailsConfig';
import OtherDetailsConfig from './OtherDetailsConfig';
import FlagDetailsConfig from './FlagDetailsConfig';
import IncidentDetailsConfig from './IncidentDetailsConfig';
import CompiledFunctionDetailsConfig from './CompiledFunctionDetailsConfig';
import DecompiledFunctionDetailsConfig from './DecompiledFunctionDetailsConfig';
import PeSectionDetailsConfig from './PeSectionDetailsConfig';
import PeImportDetailsConfig from './PeImportDetailsConfig';
import { Entities, EntityTypeMap } from '@models/entities';
import SigmaRuleDetailsConfig from './SigmaRuleDetailsConfig';

// spec: ../EntityDetails.spec.md

export type EntityDetailsConfig<T extends keyof EntityTypeMap> = {
  getEntityDetails: (entityID: string, setError: (err: string) => void, updateEntity: (entity: EntityTypeMap[T]) => void) => void;
  EntityMetaInfo: MetadataComponent<T>;
  BlankEntity: EntityTypeMap[T];
  icon: (size: number) => JSX.Element;
  supportsGraphic?: boolean;
};

type EntityConfigMap = {
  [K in keyof EntityTypeMap]: EntityDetailsConfig<K>;
};

export const EntitiesDetailsConfig = {
  [Entities.Collection]: CollectionDetailsConfig,
  [Entities.Device]: DeviceDetailsConfig,
  [Entities.FileSystem]: FileSystemDetailsConfig,
  [Entities.Folder]: FolderDetailsConfig,
  [Entities.NetworkConnection]: NetworkConnectionsDetailsConfig,
  [Entities.Other]: OtherDetailsConfig,
  [Entities.Flag]: FlagDetailsConfig,
  [Entities.SigmaRule]: SigmaRuleDetailsConfig,
  [Entities.Vendor]: VendorDetailsConfig,
  [Entities.WindowsProcessTree]: WindowsProcessTreeDetailsConfig,
  [Entities.WindowsProcess]: WindowsProcessDetailsConfig,
  [Entities.Incident]: IncidentDetailsConfig,
  [Entities.CompiledFunction]: CompiledFunctionDetailsConfig,
  [Entities.DecompiledFunction]: DecompiledFunctionDetailsConfig,
  [Entities.PeSection]: PeSectionDetailsConfig,
  [Entities.PeImport]: PeImportDetailsConfig,
} satisfies EntityConfigMap;

export const EntityDetailsPages = {
  [Entities.Collection]: createEntityDetailsPage(CollectionDetailsConfig),
  [Entities.Device]: createEntityDetailsPage(DeviceDetailsConfig),
  [Entities.FileSystem]: createEntityDetailsPage(FileSystemDetailsConfig),
  [Entities.Folder]: createEntityDetailsPage(FolderDetailsConfig),
  [Entities.NetworkConnection]: createEntityDetailsPage(NetworkConnectionsDetailsConfig),
  [Entities.Other]: createEntityDetailsPage(OtherDetailsConfig),
  [Entities.Flag]: createEntityDetailsPage(FlagDetailsConfig),
  [Entities.SigmaRule]: createEntityDetailsPage(SigmaRuleDetailsConfig),
  [Entities.Vendor]: createEntityDetailsPage(VendorDetailsConfig),
  [Entities.WindowsProcessTree]: createEntityDetailsPage(WindowsProcessTreeDetailsConfig),
  [Entities.WindowsProcess]: createEntityDetailsPage(WindowsProcessDetailsConfig),
  [Entities.Incident]: createEntityDetailsPage(IncidentDetailsConfig),
  [Entities.CompiledFunction]: createEntityDetailsPage(CompiledFunctionDetailsConfig),
  [Entities.DecompiledFunction]: createEntityDetailsPage(DecompiledFunctionDetailsConfig),
  [Entities.PeSection]: createEntityDetailsPage(PeSectionDetailsConfig),
  [Entities.PeImport]: createEntityDetailsPage(PeImportDetailsConfig),
} satisfies { [K in keyof EntityTypeMap]: React.ComponentType };
