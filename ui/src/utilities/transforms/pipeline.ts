export function pipelineToEditorObject(pipeline: Record<string, unknown>): Record<string, unknown> {
  const obj: Record<string, unknown> = {};
  for (const key of Object.keys(pipeline)) {
    if (key === 'creator' || key === 'bans') continue;
    obj[key] = pipeline[key];
  }
  return obj;
}

export function editorObjectToPipelineCreate(obj: Record<string, unknown>): Record<string, unknown> | null {
  if (!obj['group'] || !obj['name'] || !obj['order']) return null;
  return { ...obj };
}

export function editorObjectToPipelineUpdate(
  obj: Record<string, unknown>,
  originalPipeline: Record<string, unknown>,
): { group: string; name: string; data: Record<string, unknown> } | null {
  const group = (originalPipeline['group'] as string) || '';
  const name = (originalPipeline['name'] as string) || '';
  if (!group || !name) return null;

  const data: Record<string, unknown> = {};

  for (const key of Object.keys(obj)) {
    if (key === 'name' || key === 'group') continue;
    data[key] = obj[key];
  }

  if (!data['description'] || (typeof data['description'] === 'string' && data['description'].trim() === '')) {
    delete data['description'];
    data['clear_description'] = true;
  }

  const origTriggers = (originalPipeline['triggers'] as Record<string, unknown>) || {};
  const newTriggers = (obj['triggers'] as Record<string, unknown>) || {};
  const removedTriggers = Object.keys(origTriggers).filter((k) => !(k in newTriggers));
  if (removedTriggers.length > 0) {
    data['remove_triggers'] = removedTriggers;
  }

  return { group, name, data };
}
