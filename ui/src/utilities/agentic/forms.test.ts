import { describe, expect, it } from 'vitest';

// project imports
import {
  buildAgentConfig,
  buildAgentCreateForm,
  buildDeprecateForm,
  buildRootUpdateForm,
  buildToolCreateForm,
  buildToolPublishForm,
  buildToolSetCreateForm,
  buildWorkflowCreateForm,
  buildWorkflowUpdateForm,
  kindWithArticle,
  parsePositiveInt,
  positiveIntError,
  selectorError,
  parseToolSpec,
  specKindMismatch,
  specToText,
} from './forms';
import { ToolKind } from '@models/tools';

/** Read every value of a form field. */
function all(form: FormData, field: string): string[] {
  return form.getAll(field).map(String);
}

describe('positiveIntError', () => {
  it('accepts a blank field and a positive whole number', () => {
    expect(positiveIntError('')).toBe('');
    expect(positiveIntError('  ')).toBe('');
    expect(positiveIntError('12')).toBe('');
  });
  it('explains a value that would be silently dropped', () => {
    for (const text of ['abc', '0', '-3', '1.5', '1e3']) expect(positiveIntError(text)).not.toBe('');
  });
});

describe('selectorError', () => {
  it('accepts a complete selector and a blank one', () => {
    expect(selectorError('', '')).toBe('');
    expect(selectorError(' ', ' ')).toBe('');
    expect(selectorError('team', 'blue')).toBe('');
  });
  it('names the missing half of a selector that would be dropped', () => {
    expect(selectorError('team', '')).toMatch(/value/);
    expect(selectorError('', 'blue')).toMatch(/key/);
    expect(selectorError('team', '  ')).not.toBe('');
  });
});

describe('parsePositiveInt', () => {
  it('accepts positive integers only', () => {
    expect(parsePositiveInt('30')).toBe(30);
    expect(parsePositiveInt('0')).toBeUndefined();
    expect(parsePositiveInt('-4')).toBeUndefined();
    expect(parsePositiveInt('x')).toBeUndefined();
    expect(parsePositiveInt('')).toBeUndefined();
  });
  it('refuses text that only starts with a number', () => {
    expect(parsePositiveInt('1e3')).toBeUndefined();
    expect(parsePositiveInt('1,000')).toBeUndefined();
    expect(parsePositiveInt('12.9')).toBeUndefined();
    expect(parsePositiveInt('300x')).toBeUndefined();
  });
  it('allows surrounding spaces', () => {
    expect(parsePositiveInt(' 42 ')).toBe(42);
  });
});

describe('parseToolSpec', () => {
  it('reads a YAML image spec and re-encodes it as JSON', () => {
    const parsed = parseToolSpec('Image:\n  environment:\n    External: null\n');
    expect(parsed.error).toBeNull();
    expect(parsed.kind).toBe(ToolKind.Image);
    expect(JSON.parse(parsed.json ?? '')).toEqual({ Image: { environment: { External: null } } });
  });
  it('reads JSON the same way', () => {
    const parsed = parseToolSpec('{"Agentic": {"function_name": "f"}}');
    expect(parsed.kind).toBe(ToolKind.Agentic);
  });
  it('refuses an empty document', () => {
    expect(parseToolSpec('  \n').error).toMatch(/empty/);
  });
  it('refuses text that is not YAML', () => {
    expect(parseToolSpec('a: [1, 2').error).toMatch(/not valid/);
  });
  it('refuses a scalar and a list', () => {
    expect(parseToolSpec('hello').error).toMatch(/mapping/);
    expect(parseToolSpec('- a\n- b').error).toMatch(/mapping/);
  });
  it('refuses a spec without exactly one known kind key', () => {
    expect(parseToolSpec('Other: {}').error).toMatch(/exactly one/);
    expect(parseToolSpec('Image: {}\nHarness: {}').error).toMatch(/exactly one/);
  });
});

describe('specToText', () => {
  it('prints indented JSON that parses back to the same spec', () => {
    const spec = { Image: { execution: { timeout: 30 } } };
    expect(JSON.parse(specToText(spec))).toEqual(spec);
    expect(specToText(spec)).toContain('\n  ');
  });
});

describe('tool forms', () => {
  it('builds the create form with trimmed fields and the spec as given', () => {
    const form = buildToolCreateForm({
      name: ' strings ',
      groups: ['a', ' ', 'b'],
      description: ' extracts ',
      version: ' 1.0.0 ',
      spec: '{"Image":{}}',
      kind: ToolKind.Image,
    });
    expect(form.get('kind')).toBe('Tool');
    expect(form.get('name')).toBe('strings');
    expect(all(form, 'groups[]')).toEqual(['a', 'b']);
    expect(form.get('description')).toBe('extracts');
    expect(form.get('metadata[tool_kind]')).toBe('Image');
    expect(form.get('metadata[tool_version]')).toBe('1.0.0');
    expect(form.get('metadata[tool_spec]')).toBe('{"Image":{}}');
  });
  it('leaves a blank description unset', () => {
    const form = buildToolCreateForm({ name: 'x', groups: ['a'], description: '  ', version: '1', spec: '{}', kind: ToolKind.Image });
    expect(form.has('description')).toBe(false);
  });
  it('builds the publish form with only the version and spec', () => {
    const form = buildToolPublishForm({ version: '2.0.0', spec: '{}' });
    expect([...form.keys()].sort()).toEqual(['metadata[tool_spec]', 'metadata[tool_version]']);
  });
  it('builds the deprecate form', () => {
    expect(buildDeprecateForm(true).get('metadata[tool_instance_deprecated]')).toBe('true');
    expect(buildDeprecateForm(false).get('metadata[tool_instance_deprecated]')).toBe('false');
  });
});

describe('buildRootUpdateForm', () => {
  it('sends only the changed fields', () => {
    const form = buildRootUpdateForm({ description: 'new', addGroups: ['g'] });
    expect(form.get('description')).toBe('new');
    expect(all(form, 'add_groups[]')).toEqual(['g']);
    expect(form.has('name')).toBe(false);
    expect(form.has('clear_description')).toBe(false);
  });
  it('sends the clear flag', () => {
    expect(buildRootUpdateForm({ clearDescription: true }).get('clear_description')).toBe('true');
  });
});

describe('buildToolSetCreateForm', () => {
  const base = {
    name: 'set',
    groups: ['g'],
    description: '',
    tools: ['t1', 't2'],
    selectorKey: '',
    selectorValue: '',
    readOnly: true,
    autoInstance: false,
  };
  it('lists the tools and flags', () => {
    const form = buildToolSetCreateForm(base);
    expect(form.get('kind')).toBe('ToolSet');
    expect(all(form, 'metadata[toolset_tools][]')).toEqual(['t1', 't2']);
    expect(form.get('metadata[toolset_read_only]')).toBe('true');
    expect(form.get('metadata[toolset_auto_instance]')).toBe('false');
    expect(form.has('metadata[toolset_selector_key]')).toBe(false);
  });
  it('sends a selector only when it has a key and a value', () => {
    expect(buildToolSetCreateForm({ ...base, selectorKey: 'area' }).has('metadata[toolset_selector_key]')).toBe(false);
    const form = buildToolSetCreateForm({ ...base, selectorKey: 'area', selectorValue: 'static' });
    expect(form.get('metadata[toolset_selector_key]')).toBe('area');
    expect(form.get('metadata[toolset_selector_value]')).toBe('static');
  });
});

describe('agent forms', () => {
  const base = {
    name: 'lead',
    groups: ['g'],
    description: '',
    systemPrompt: 'be brief',
    prompt: 'triage',
    models: ['m1', ' ', 'm2'],
    maxTurns: '40',
    harness: 'h1',
    tools: ['t'],
    toolSets: ['s'],
    subAgents: ['a'],
  };
  it('builds a config without unset settings', () => {
    expect(buildAgentConfig({ ...base, systemPrompt: '', prompt: '', models: [], maxTurns: '' })).toEqual({});
  });
  it('builds a config with prompts, models, and turns', () => {
    expect(buildAgentConfig(base)).toEqual({
      system_prompt: 'be brief',
      prompt: 'triage',
      models: [{ model_id: 'm1' }, { model_id: 'm2' }],
      max_turns: 40,
    });
  });
  it('adds the delegation function name and model description when set', () => {
    const config = buildAgentConfig({ ...base, functionName: ' scan_helper ', modelDescription: ' Scans a sample ' });
    expect(config).toHaveProperty('function_name', 'scan_helper');
    expect(config).toHaveProperty('model_description', 'Scans a sample');
  });
  it('leaves blank delegation fields out', () => {
    const config = buildAgentConfig({ ...base, functionName: ' ', modelDescription: '' });
    expect(config).not.toHaveProperty('function_name');
    expect(config).not.toHaveProperty('model_description');
  });
  it('ignores a turn count that is not a positive number', () => {
    expect(buildAgentConfig({ ...base, maxTurns: 'abc' })).not.toHaveProperty('max_turns');
    expect(buildAgentConfig({ ...base, maxTurns: '0' })).not.toHaveProperty('max_turns');
    expect(buildAgentConfig({ ...base, maxTurns: '1e2' })).not.toHaveProperty('max_turns');
  });
  it('builds the create form with every link', () => {
    const form = buildAgentCreateForm(base);
    expect(form.get('kind')).toBe('Agent');
    expect(JSON.parse(form.get('metadata[agent_config]') as string)).toHaveProperty('max_turns', 40);
    expect(form.get('metadata[agent_harness]')).toBe('h1');
    expect(all(form, 'metadata[agent_tools][]')).toEqual(['t']);
    expect(all(form, 'metadata[agent_tool_sets][]')).toEqual(['s']);
    expect(all(form, 'metadata[agent_sub_agents][]')).toEqual(['a']);
  });
  it('leaves the harness unset when blank', () => {
    expect(buildAgentCreateForm({ ...base, harness: '' }).has('metadata[agent_harness]')).toBe(false);
  });
});

describe('workflow forms', () => {
  const base = {
    name: 'wf',
    groups: ['g'],
    description: 'd',
    agents: ['a1', 'a2'],
    entryAgent: 'a1',
    defaultModels: ['m1'],
    maxAliveSubAgents: '4',
    autoInstance: true,
  };
  it('builds the create form', () => {
    const form = buildWorkflowCreateForm(base);
    expect(form.get('kind')).toBe('WorkflowCatalog');
    expect(all(form, 'metadata[workflow_agents][]')).toEqual(['a1', 'a2']);
    expect(form.get('metadata[workflow_entry_agent]')).toBe('a1');
    expect(JSON.parse(form.get('metadata[workflow_default_models]') as string)).toEqual([{ model_id: 'm1' }]);
    expect(JSON.parse(form.get('metadata[workflow_limits]') as string)).toEqual({ max_alive_sub_agents: 4 });
    expect(form.get('metadata[workflow_auto_instance]')).toBe('true');
  });
  it('omits unset limits, models, and entry agent', () => {
    const form = buildWorkflowCreateForm({ ...base, entryAgent: '', defaultModels: [], maxAliveSubAgents: '' });
    expect(form.has('metadata[workflow_entry_agent]')).toBe(false);
    expect(form.has('metadata[workflow_default_models]')).toBe(false);
    expect(form.has('metadata[workflow_limits]')).toBe(false);
  });
  it('omits a sub-agent limit that is not a whole number', () => {
    expect(buildWorkflowCreateForm({ ...base, maxAliveSubAgents: '4.5' }).has('metadata[workflow_limits]')).toBe(false);
  });
  it('builds the update form', () => {
    const form = buildWorkflowUpdateForm({ addAgents: ['a3'], removeAgents: ['a2'], clearEntryAgent: true });
    expect(all(form, 'metadata[add_workflow_agents][]')).toEqual(['a3']);
    expect(all(form, 'metadata[remove_workflow_agents][]')).toEqual(['a2']);
    expect(form.get('metadata[clear_workflow_entry_agent]')).toBe('true');
    expect(form.has('metadata[workflow_entry_agent]')).toBe(false);
  });
  it('sends only the settings that were edited', () => {
    const form = buildWorkflowUpdateForm({ autoInstance: false });
    expect(form.get('metadata[workflow_auto_instance]')).toBe('false');
    for (const field of ['workflow_limits', 'workflow_default_models', 'workflow_model_auth']) {
      expect(form.has(`metadata[${field}]`)).toBe(false);
    }
  });
  it('sends limits, default models, and model auth as whole replacement values', () => {
    const form = buildWorkflowUpdateForm({
      maxAliveSubAgents: 4,
      defaultModels: [{ model_id: 'a', provider: 'p' }],
      modelAuthLevels: ['Group'],
    });
    expect(JSON.parse(all(form, 'metadata[workflow_limits]')[0])).toEqual({ max_alive_sub_agents: 4 });
    expect(JSON.parse(all(form, 'metadata[workflow_default_models]')[0])).toEqual([{ model_id: 'a', provider: 'p' }]);
    expect(JSON.parse(all(form, 'metadata[workflow_model_auth]')[0])).toEqual({ levels: ['Group'] });
  });
  it('clears a limit, the default models, and the auth levels with empty values', () => {
    const form = buildWorkflowUpdateForm({ maxAliveSubAgents: null, defaultModels: [], modelAuthLevels: [] });
    expect(JSON.parse(all(form, 'metadata[workflow_limits]')[0])).toEqual({});
    expect(JSON.parse(all(form, 'metadata[workflow_default_models]')[0])).toEqual([]);
    expect(JSON.parse(all(form, 'metadata[workflow_model_auth]')[0])).toEqual({ levels: [] });
  });
});

describe('specKindMismatch', () => {
  it('uses "an" before kinds that start with a vowel and "a" before the rest', () => {
    expect(kindWithArticle(ToolKind.Image)).toBe('an Image');
    expect(kindWithArticle(ToolKind.Agentic)).toBe('an Agentic');
    expect(kindWithArticle(ToolKind.Harness)).toBe('a Harness');
  });
  it('names the pasted spec kind and what it was expected to match', () => {
    expect(specKindMismatch(ToolKind.Image, 'the kind chosen is Harness')).toBe('The spec is an Image spec but the kind chosen is Harness');
    expect(specKindMismatch(ToolKind.Agentic, `this is ${kindWithArticle(ToolKind.Image)} tool`)).toBe(
      'The spec is an Agentic spec but this is an Image tool',
    );
  });
});
