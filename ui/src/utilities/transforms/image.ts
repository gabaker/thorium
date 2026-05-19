const READ_ONLY_FIELDS = ['creator', 'runtime', 'used_by', 'bans'];

export function imageToEditorObject(image: Record<string, unknown>): Record<string, unknown> {
  const obj: Record<string, unknown> = {};
  for (const key of Object.keys(image)) {
    if (READ_ONLY_FIELDS.includes(key)) continue;
    obj[key] = image[key];
  }
  return obj;
}

export function editorObjectToImageCreate(obj: Record<string, unknown>): Record<string, unknown> | null {
  if (!obj['group'] || !obj['name']) return null;
  return { ...obj };
}

export function editorObjectToImageUpdate(
  obj: Record<string, unknown>,
  originalImage: Record<string, unknown>,
): { group: string; name: string; data: Record<string, unknown> } | null {
  const group = (originalImage['group'] as string) || '';
  const name = (originalImage['name'] as string) || '';
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

  return { group, name, data };
}
