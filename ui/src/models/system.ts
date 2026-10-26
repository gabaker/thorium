/// Stats for a specific scaler type
export type ScalerStats = {
  /// The number of outstanding deadlines
  deadlines: number;
  /// The number of currently running deadlines
  running: number;
};

/// Per-user stage stats for a pipeline stage
export type UserStageStats = {
  /// Created but not running deadlines
  created: number;
  /// Running deadlines
  running: number;
  /// Number of finished stages (completed or failed)
  completed: number;
  /// Number of already failed stages of this type
  failed: number;
  /// Sleeping deadlines for this stage type
  sleeping: number;
  /// Total of all user deadlines of this stage type
  total: number;
};

/// Per-group stats
export type GroupsStats = {
  [group: string]: {
    /// Status summaries for each tool instance with jobs on a shared workload queue in this group, by user
    workloads: {
      [toolInstance: string]: {
        [user: string]: UserStageStats;
      };
    };
  };
};

/// One system setting's value: a scalar, or a nested structure such as the models settings
export type SystemSettingValue = string | number | boolean | null | SystemSettingValue[] | { [key: string]: SystemSettingValue };

/// System settings key-value map returned by the settings API
export type SystemSettings = Record<string, SystemSettingValue>;

/// System-wide stats
export type Stats = {
  /// The total number of deadlines currently in the system across all scalers
  deadlines: number;
  /// The total number of running jobs currently in the system across all scalers
  running: number;
  /// The number of users currently in the system
  users: number;
  /// The stats for jobs under the k8s scaler
  k8s: ScalerStats;
  /// The stats for jobs under the baremetal scaler
  baremetal: ScalerStats;
  /// The stats for jobs under the external scaler
  external: ScalerStats;
  /// Detailed stats reports for each group
  groups: GroupsStats;
};
