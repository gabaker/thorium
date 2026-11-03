// project imports
import { ToolKind } from '@models/tools';

/** A starting spec for an Image tool: a container run on K8s. */
const IMAGE_TEMPLATE = `Image:
  environment:
    K8s:
      image: registry.example.com/my-tool:1.0
      scheduling:
        resources: {cpu: 1000, memory: 512}
        spawn_limit: Unlimited
  execution:
    args:
      entrypoint: [/opt/my-tool/run]
    timeout: 300
`;

/** A starting spec for an Agentic tool: a function a model can call, backed by a Thorium MCP tool. */
const AGENTIC_TEMPLATE = `Agentic:
  function_name: get_sample_info
  model_description: Get a sample's metadata by SHA256
  input_schema:
    type: object
    properties:
      sha256: {type: string}
    required: [sha256]
  backing:
    ThoriumMcp:
      tool: get_sample
  timeout: 60
`;

/** A starting spec for a Harness tool: the program that runs an agent's model and tool loop. */
const HARNESS_TEMPLATE = `Harness:
  workload:
    environment:
      K8s:
        image: registry.example.com/my-harness:1.0
        scheduling:
          resources: {cpu: 500, memory: 512}
          spawn_limit: Unlimited
    execution:
      args:
        entrypoint: [/opt/my-harness/run]
      timeout: 1800
  capabilities:
    mechanisms: [Cli]
    file_io: false
    model_apis: [OpenAiChat]
    credential_kinds: [ApiKey]
    per_agent_models: false
  tool_policy:
    max_result_bytes: 65536
  orphaned_runs: Delete
`;

/** The starting spec of each kind of tool. */
export const SPEC_TEMPLATES: Record<ToolKind, string> = {
  [ToolKind.Image]: IMAGE_TEMPLATE,
  [ToolKind.Agentic]: AGENTIC_TEMPLATE,
  [ToolKind.Harness]: HARNESS_TEMPLATE,
};
