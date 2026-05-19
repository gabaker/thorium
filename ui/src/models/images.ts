import { SemVer } from './semver';
import { OutputDisplayType } from './results';

export type ImageVersion = {
  SemVer?: SemVer;
  Custom?: string;
};

export enum ImageScaler {
  K8s = 'K8s',
  BareMetal = 'BareMetal',
  Windows = 'Windows',
  Kvm = 'Kvm',
  External = 'External',
}

export enum ArgStrategy {
  None = 'None',
  Append = 'Append',
}

export type ArgStrategyKwarg = {
  Kwarg: string;
};

export type ArgStrategyValue = ArgStrategy | ArgStrategyKwarg;

export type SpawnLimitsBasic = {
  Basic: number;
};

export type SpawnLimitsValue = 'Unlimited' | SpawnLimitsBasic;

export enum DependencyPassStrategy {
  Paths = 'Paths',
  Names = 'Names',
  Directory = 'Directory',
  Disabled = 'Disabled',
}

export enum FileNamingStrategy {
  Sha256 = 'Sha256',
  MostRecent = 'MostRecent',
}

export enum OutputHandler {
  Files = 'Files',
}

export enum AutoTagLogic {
  Exists = 'Exists',
}

export type AutoTagLogicValue =
  | AutoTagLogic
  | { Equal: unknown }
  | { Not: unknown }
  | { Greater: unknown }
  | { GreaterOrEqual: unknown }
  | { LesserOrEqual: unknown }
  | { Lesser: unknown }
  | { In: unknown[] }
  | { NotIn: unknown[] };

export enum VolumeTypes {
  HostPath = 'HostPath',
  ConfigMap = 'ConfigMap',
  Secret = 'Secret',
  NFS = 'NFS',
}

export enum HostPathTypes {
  DirectoryOrCreate = 'DirectoryOrCreate',
  Directory = 'Directory',
  FileOrCreate = 'FileOrCreate',
  File = 'File',
  Socket = 'Socket',
  CharDevice = 'CharDevice',
  BlockDevice = 'BlockDevice',
}

export type HostPath = {
  path: string;
  path_type?: HostPathTypes;
};

export type ConfigMap = {
  default_mode?: number;
  optional?: boolean;
};

export type Secret = {
  default_mode?: number;
  optional?: boolean;
};

export type NFS = {
  path: string;
  server: string;
};

export type Volume = {
  name: string;
  archetype: VolumeTypes;
  mount_path: string;
  sub_path?: string;
  read_only?: boolean;
  kustomize?: boolean;
  host_path?: HostPath;
  config_map?: ConfigMap;
  secret?: Secret;
  nfs?: NFS;
};

export type BurstableResourcesRequest = {
  cpu: number;
  memory: number;
};

export type ResourcesRequest = {
  cpu: number;
  memory: number;
  ephemeral_storage?: number;
  worker_slots?: number;
  nvidia_gpu?: number;
  amd_gpu?: number;
  burstable?: BurstableResourcesRequest;
};

export type Resources = {
  cpu: number;
  memory: number;
  ephemeral_storage: number;
  worker_slots: number;
  nvidia_gpu: number;
  amd_gpu: number;
  burstable: BurstableResourcesRequest;
};

export type ImageLifetime = {
  counter: string;
  amount: number;
};

export type SecurityContext = {
  user?: number;
  group?: number;
  allow_privilege_escalation?: boolean;
};

export type ImageArgs = {
  entrypoint?: string[];
  command?: string[];
  reaction?: string;
  repo?: string;
  commit?: string;
  output?: ArgStrategyValue;
  output_files?: ArgStrategyValue;
};

export type SampleDependencySettings = {
  location?: string;
  kwarg?: string;
  strategy?: DependencyPassStrategy;
  naming?: FileNamingStrategy;
};

export type RepoDependencySettings = {
  location?: string;
  kwarg?: string;
  strategy?: DependencyPassStrategy;
};

export type TagDependencySettings = {
  enabled?: boolean;
  location?: string;
  kwarg?: string;
  strategy?: DependencyPassStrategy;
};

export type ChildrenDependencySettings = {
  enabled?: boolean;
  images?: string[];
  location?: string;
  kwarg?: string;
  strategy?: DependencyPassStrategy;
};

export type EphemeralDependencySettings = {
  location?: string;
  kwarg?: string;
  strategy?: DependencyPassStrategy;
  names?: string[];
};

export type KwargDependencyList = {
  List: string;
};

export type KwargDependencyMap = {
  Map: { [image: string]: string };
};

export type KwargDependencyValue = 'None' | KwargDependencyList | KwargDependencyMap;

export type ResultDependencySettings = {
  images?: string[];
  location?: string;
  kwarg?: KwargDependencyValue;
  strategy?: DependencyPassStrategy;
  names?: string[];
};

export type GenericCacheDependencySettings = {
  kwarg?: string;
  strategy?: DependencyPassStrategy;
};

export type CacheDependencySettings = {
  location?: string;
  generic?: GenericCacheDependencySettings;
  use_parent_cache?: boolean;
  enabled?: boolean;
};

export type Dependencies = {
  samples?: SampleDependencySettings;
  ephemeral?: EphemeralDependencySettings;
  results?: ResultDependencySettings;
  repos?: RepoDependencySettings;
  tags?: TagDependencySettings;
  children?: ChildrenDependencySettings;
  cache?: CacheDependencySettings;
};

export type AutoTag = {
  logic: AutoTagLogicValue;
  key?: string;
};

export type FilesHandler = {
  results?: string;
  result_files?: string;
  tags?: string;
  names?: string[];
};

export type OutputCollection = {
  handler?: OutputHandler;
  files?: FilesHandler;
  as_filesystem?: boolean;
  children?: string;
  auto_tag?: { [name: string]: AutoTag };
  groups?: string[];
};

export type ChildFilters = {
  mime?: string[];
  file_name?: string[];
  file_extension?: string[];
  submit_non_matches?: boolean;
};

export type Cleanup = {
  job_id: ArgStrategyValue;
  results: ArgStrategyValue;
  result_files_dir: ArgStrategyValue;
  script: string;
};

export type Kvm = {
  xml: string;
  qcow2: string;
};

export type ImageRequest = {
  group: string;
  name: string;
  version?: ImageVersion;
  scaler?: ImageScaler;
  image?: string;
  lifetime?: ImageLifetime;
  timeout?: number;
  resources?: ResourcesRequest;
  spawn_limit?: SpawnLimitsValue;
  volumes?: Volume[];
  env?: { [key: string]: string | null };
  args?: ImageArgs;
  modifiers?: string;
  description?: string;
  security_context?: SecurityContext;
  collect_logs?: boolean;
  generator?: boolean;
  dependencies?: Dependencies;
  display_type?: OutputDisplayType;
  output_collection?: OutputCollection;
  child_filters?: ChildFilters;
  clean_up?: Cleanup;
  kvm?: Kvm;
  network_policies?: string[];
};

export type ImageBanKind =
  | { Generic: { msg: string } }
  | { InvalidImageUrl: { url: string } }
  | { InvalidHostPath: { volume_name: string; host_path: string } };

export type ImageBan = {
  id: string;
  time_banned: string;
  ban_kind: ImageBanKind;
};

export type Image = {
  group: string;
  name: string;
  creator: string;
  version?: ImageVersion;
  scaler: ImageScaler;
  image?: string;
  lifetime?: ImageLifetime;
  timeout?: number;
  resources: Resources;
  spawn_limit: SpawnLimitsValue;
  env: { [key: string]: string | null };
  runtime: number;
  volumes: Volume[];
  args: ImageArgs;
  modifiers?: string;
  description?: string;
  security_context: SecurityContext;
  used_by: string[];
  collect_logs: boolean;
  generator: boolean;
  dependencies: Dependencies;
  display_type: OutputDisplayType;
  output_collection: OutputCollection;
  child_filters: ChildFilters;
  clean_up?: Cleanup;
  kvm?: Kvm;
  bans: { [uuid: string]: ImageBan };
  network_policies: string[];
};

