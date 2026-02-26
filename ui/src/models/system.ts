export type ScalerStats = {
  deadlines: number; // number of outstanding deadlines
  running: number; // number of currently running deadlines
};

export type UserStageStats = {
  created: number; // created but not running deadlines
  running: number; // running deadlines
  completed: number; // number of finished stats (completed or failed)
  failed: number; // number of already failed stages of this type
  sleeping: number; // sleeping deadlines for this stage type
  total: number; // total of all user deadlines of this stage type
};

export type PipelineStats = {
  // stats for a pipeline
  stages: {
    // pipeline stages
    [stage: string]: {
      // per stage
      [user: string]: UserStageStats; // per user stats
    };
  };
};

export type GroupsStats = {
  // per group stats
  [group: string]: {
    pipelines: {
      [pipeline: string]: PipelineStats; // per pipeline stats
    };
  };
};

export type Stats = {
  deadlines: number; // number of outstanding deadlines
  running: number; // number of running deadlines
  users: number; // number of users with Thorium accounts
  k8s: ScalerStats; // per scaler type stats
  baremetal: ScalerStats;
  external: ScalerStats;
  groups: GroupsStats;
};

export type SystemSettings = {
  reserved_cpu: number;
  reserved_memory: number;
  reserved_storage: number;
  fairshare_cpu: number;
  fairshare_memory: number;
  fairshare_storage: number;
  host_path_whitelist: string[];
  allow_unrestricted_host_paths: boolean;
};

export type HostPathWhitelistUpdate = {
  add_paths: string[];
  remove_paths: string[];
};

export type SystemSettingsUpdate = {
  reserved_cpu?: string;
  reserved_memory?: string;
  reserved_storage?: string;
  fairshare_cpu?: string;
  fairshare_memory?: string;
  fairshare_storage?: string;
  host_path_whitelist?: HostPathWhitelistUpdate;
  clear_host_path_whitelist?: boolean;
  allow_unrestricted_host_paths?: boolean;
};
