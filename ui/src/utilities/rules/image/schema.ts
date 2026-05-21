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

export const KNOWN_PIPELINE_FIELDS = ['group', 'name', 'order', 'sla', 'triggers', 'description'] as const;

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

export const KNOWN_RESOURCES_FIELDS = ['cpu', 'memory', 'ephemeral_storage', 'worker_slots', 'nvidia_gpu', 'amd_gpu', 'burstable'] as const;

export const KNOWN_ARGS_FIELDS = ['entrypoint', 'command', 'reaction', 'repo', 'commit', 'output', 'output_files'] as const;

export const KNOWN_DEPENDENCIES_FIELDS = ['samples', 'ephemeral', 'results', 'repos', 'tags', 'children', 'cache'] as const;

export const KNOWN_SAMPLE_DEP_FIELDS = ['location', 'kwarg', 'strategy', 'naming'] as const;

export const KNOWN_REPO_DEP_FIELDS = ['location', 'kwarg', 'strategy'] as const;

export const KNOWN_TAG_DEP_FIELDS = ['enabled', 'location', 'kwarg', 'strategy'] as const;

export const KNOWN_CHILDREN_DEP_FIELDS = ['enabled', 'images', 'location', 'kwarg', 'strategy'] as const;

export const KNOWN_EPHEMERAL_DEP_FIELDS = ['location', 'kwarg', 'strategy', 'names'] as const;

export const KNOWN_RESULT_DEP_FIELDS = ['images', 'location', 'kwarg', 'strategy', 'names'] as const;

export const KNOWN_CACHE_DEP_FIELDS = ['location', 'generic', 'use_parent_cache', 'enabled'] as const;

export const KNOWN_OUTPUT_COLLECTION_FIELDS = ['handler', 'files', 'as_filesystem', 'children', 'auto_tag', 'groups'] as const;

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

import { FieldValueType, type FieldSchema } from '../types';

export const IMAGE_FIELD_SCHEMAS: Record<string, FieldSchema> = {
  group: { type: FieldValueType.String, required: true, placeholder: 'group-name' },
  name: { type: FieldValueType.String, required: true, placeholder: 'image-name' },
  version: { type: FieldValueType.String, placeholder: '1.0.0' },
  image: { type: FieldValueType.String, placeholder: 'registry/image:tag' },
  description: { type: FieldValueType.String, placeholder: 'Image description' },
  modifiers: { type: FieldValueType.String, placeholder: 'modifier string' },
  scaler: { type: FieldValueType.Enum, enumValues: IMAGE_SCALER_VALUES },
  display_type: { type: FieldValueType.Enum, enumValues: OUTPUT_DISPLAY_TYPE_VALUES },
  timeout: { type: FieldValueType.Number, placeholder: '300' },
  spawn_limit: { type: FieldValueType.Number, placeholder: '1' },
  collect_logs: { type: FieldValueType.Boolean },
  generator: { type: FieldValueType.Boolean },
  kvm: {
    type: FieldValueType.Object,
    fields: {
      xml: { type: FieldValueType.String, required: true, placeholder: '/path/to/vm.xml' },
      qcow2: { type: FieldValueType.String, required: true, placeholder: '/path/to/disk.qcow2' },
    },
  },
  lifetime: {
    type: FieldValueType.Object,
    fields: {
      counter: { type: FieldValueType.Enum, required: true, enumValues: LIFETIME_COUNTER_VALUES },
      amount: { type: FieldValueType.Number, required: true, placeholder: '32' },
    },
  },
  resources: {
    type: FieldValueType.Object,
    fields: {
      cpu: { type: FieldValueType.Number, placeholder: '1000' },
      memory: { type: FieldValueType.Number, placeholder: '512' },
      ephemeral_storage: { type: FieldValueType.Number, placeholder: '1024' },
      worker_slots: { type: FieldValueType.Number, placeholder: '1' },
      nvidia_gpu: { type: FieldValueType.Number, placeholder: '0' },
      amd_gpu: { type: FieldValueType.Number, placeholder: '0' },
    },
  },
  args: {
    type: FieldValueType.Object,
    fields: {
      entrypoint: { type: FieldValueType.String, placeholder: '/entrypoint.sh' },
      command: { type: FieldValueType.String, placeholder: 'run' },
      reaction: { type: FieldValueType.String, placeholder: 'reaction-kwarg' },
      repo: { type: FieldValueType.String, placeholder: 'repo-kwarg' },
      commit: { type: FieldValueType.String, placeholder: 'commit-kwarg' },
      output: { type: FieldValueType.Enum, enumValues: ARG_STRATEGY_VALUES },
      output_files: { type: FieldValueType.Enum, enumValues: ARG_STRATEGY_VALUES },
    },
  },
  dependencies: {
    type: FieldValueType.Object,
    fields: {
      samples: { type: FieldValueType.Object, placeholder: 'sample dependency config' },
      ephemeral: { type: FieldValueType.Object, placeholder: 'ephemeral dependency config' },
      results: { type: FieldValueType.Object, placeholder: 'result dependency config' },
      repos: { type: FieldValueType.Object, placeholder: 'repo dependency config' },
      tags: { type: FieldValueType.Object, placeholder: 'tag dependency config' },
      children: { type: FieldValueType.Object, placeholder: 'children dependency config' },
      cache: { type: FieldValueType.Object, placeholder: 'cache config' },
    },
  },
  security_context: {
    type: FieldValueType.Object,
    fields: {
      user: { type: FieldValueType.Number, placeholder: '1000' },
      group: { type: FieldValueType.Number, placeholder: '1000' },
      allow_privilege_escalation: { type: FieldValueType.Boolean },
    },
  },
  output_collection: {
    type: FieldValueType.Object,
    fields: {
      handler: { type: FieldValueType.Enum, required: true, enumValues: OUTPUT_HANDLER_VALUES },
      files: { type: FieldValueType.Object, placeholder: 'file handler config' },
      as_filesystem: { type: FieldValueType.Boolean },
      children: { type: FieldValueType.Boolean },
      auto_tag: { type: FieldValueType.Boolean },
      groups: { type: FieldValueType.StringArray, placeholder: 'group-name' },
    },
  },
  child_filters: {
    type: FieldValueType.Object,
    fields: {
      mime: { type: FieldValueType.StringArray, placeholder: 'application/pdf' },
      file_name: { type: FieldValueType.StringArray, placeholder: '*.exe' },
      file_extension: { type: FieldValueType.StringArray, placeholder: '.dll' },
      submit_non_matches: { type: FieldValueType.Boolean },
    },
  },
  clean_up: {
    type: FieldValueType.Object,
    fields: {
      job_id: { type: FieldValueType.String, placeholder: 'job-id-kwarg' },
      results: { type: FieldValueType.String, placeholder: '/tmp/thorium/results' },
      result_files_dir: { type: FieldValueType.String, placeholder: '/tmp/thorium/result-files' },
      script: { type: FieldValueType.String, required: true, placeholder: '/cleanup.sh' },
    },
  },
  env: { type: FieldValueType.Object, placeholder: 'KEY: value' },
  volumes: { type: FieldValueType.StringArray, placeholder: 'volume entry' },
  network_policies: { type: FieldValueType.StringArray, placeholder: 'policy-name' },
};

export const PIPELINE_FIELD_SCHEMAS: Record<string, FieldSchema> = {
  group: { type: FieldValueType.String, required: true, placeholder: 'group-name' },
  name: { type: FieldValueType.String, required: true, placeholder: 'pipeline-name' },
  description: { type: FieldValueType.String, placeholder: 'Pipeline description' },
  sla: { type: FieldValueType.Number, placeholder: '604800' },
  order: { type: FieldValueType.StringArray, placeholder: 'image-name' },
  triggers: {
    type: FieldValueType.Object,
    placeholder: 'event trigger config',
  },
};
