export const REQUIRED_IMAGE_FIELDS = ['group', 'name'] as const;

export const REQUIRED_PIPELINE_FIELDS = ['group', 'name', 'order'] as const;

export const KNOWN_IMAGE_FIELDS = [
  'group',
  'name',
  'version',
  'scaler',
  'image',
  'lifetime',
  'timeout',
  'resources',
  'spawn_limit',
  'volumes',
  'env',
  'args',
  'modifiers',
  'description',
  'security_context',
  'collect_logs',
  'generator',
  'dependencies',
  'display_type',
  'output_collection',
  'child_filters',
  'clean_up',
  'kvm',
  'network_policies',
] as const;

export const KNOWN_PIPELINE_FIELDS = [
  'group',
  'name',
  'order',
  'sla',
  'triggers',
  'description',
] as const;

export const IMAGE_SCALER_VALUES = ['K8s', 'BareMetal', 'Windows', 'Kvm', 'External'] as const;

export const ARG_STRATEGY_VALUES = ['None', 'Append'] as const;

export const DEPENDENCY_PASS_STRATEGY_VALUES = ['Paths', 'Names', 'Directory', 'Disabled'] as const;

export const FILE_NAMING_STRATEGY_VALUES = ['Sha256', 'MostRecent'] as const;

export const OUTPUT_DISPLAY_TYPE_VALUES = [
  'JSON',
  'String',
  'Table',
  'Image',
  'Custom',
  'Disassembly',
  'HTML',
  'Markdown',
  'Hidden',
  'XML',
] as const;

export const OUTPUT_HANDLER_VALUES = ['Files'] as const;

export const VOLUME_TYPE_VALUES = ['HostPath', 'ConfigMap', 'Secret', 'NFS'] as const;

export const HOST_PATH_TYPE_VALUES = [
  'DirectoryOrCreate',
  'Directory',
  'FileOrCreate',
  'File',
  'Socket',
  'CharDevice',
  'BlockDevice',
] as const;

export const LIFETIME_COUNTER_VALUES = ['jobs', 'time'] as const;

export const KNOWN_RESOURCES_FIELDS = [
  'cpu',
  'memory',
  'ephemeral_storage',
  'worker_slots',
  'nvidia_gpu',
  'amd_gpu',
  'burstable',
] as const;

export const KNOWN_ARGS_FIELDS = [
  'entrypoint',
  'command',
  'reaction',
  'repo',
  'commit',
  'output',
  'output_files',
] as const;

export const KNOWN_DEPENDENCIES_FIELDS = [
  'samples',
  'ephemeral',
  'results',
  'repos',
  'tags',
  'children',
  'cache',
] as const;

export const KNOWN_SAMPLE_DEP_FIELDS = ['location', 'kwarg', 'strategy', 'naming'] as const;

export const KNOWN_REPO_DEP_FIELDS = ['location', 'kwarg', 'strategy'] as const;

export const KNOWN_TAG_DEP_FIELDS = ['enabled', 'location', 'kwarg', 'strategy'] as const;

export const KNOWN_CHILDREN_DEP_FIELDS = ['enabled', 'images', 'location', 'kwarg', 'strategy'] as const;

export const KNOWN_EPHEMERAL_DEP_FIELDS = ['location', 'kwarg', 'strategy', 'names'] as const;

export const KNOWN_RESULT_DEP_FIELDS = ['images', 'location', 'kwarg', 'strategy', 'names'] as const;

export const KNOWN_CACHE_DEP_FIELDS = ['location', 'generic', 'use_parent_cache', 'enabled'] as const;

export const KNOWN_OUTPUT_COLLECTION_FIELDS = [
  'handler',
  'files',
  'as_filesystem',
  'children',
  'auto_tag',
  'groups',
] as const;

export const KNOWN_FILES_HANDLER_FIELDS = ['results', 'result_files', 'tags', 'names'] as const;

export const KNOWN_CHILD_FILTERS_FIELDS = ['mime', 'file_name', 'file_extension', 'submit_non_matches'] as const;

export const KNOWN_CLEANUP_FIELDS = ['job_id', 'results', 'result_files_dir', 'script'] as const;

export const KNOWN_SECURITY_CONTEXT_FIELDS = ['user', 'group', 'allow_privilege_escalation'] as const;

export const KNOWN_LIFETIME_FIELDS = ['counter', 'amount'] as const;

export const KNOWN_VOLUME_FIELDS = [
  'name',
  'archetype',
  'mount_path',
  'sub_path',
  'read_only',
  'kustomize',
  'host_path',
  'config_map',
  'secret',
  'nfs',
] as const;

export const KNOWN_KVM_FIELDS = ['xml', 'qcow2'] as const;

export const KNOWN_BURSTABLE_FIELDS = ['cpu', 'memory'] as const;
