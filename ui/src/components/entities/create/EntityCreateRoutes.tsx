import { JSX, lazy } from 'react';

// project imports
import { buildPathByType } from '@entities/shared/routePaths';
import { Entities } from '@models/entities';

const FileUpload = lazy(async () => await import('./override_pages/FileUpload'));
const HuntCreate = lazy(async () => await import('./override_pages/HuntCreate'));

export const EntityCreateRoutes: Record<string, { type: Entities; override_page?: React.LazyExoticComponent<() => JSX.Element> }> = {
  '/create/collection': { type: Entities.Collection },
  '/create/collections': { type: Entities.Collection },
  '/create/device': { type: Entities.Device },
  '/create/devices': { type: Entities.Device },
  '/upload': { type: Entities.File, override_page: FileUpload },
  '/upload/*': { type: Entities.File, override_page: FileUpload },
  '/uploads': { type: Entities.File, override_page: FileUpload },
  '/analyze': { type: Entities.File, override_page: FileUpload },
  '/analyze/*': { type: Entities.File, override_page: FileUpload },
  '/analysis': { type: Entities.File, override_page: FileUpload },
  '/create/file': { type: Entities.File, override_page: FileUpload },
  '/create/files': { type: Entities.File, override_page: FileUpload },
  '/create/flag': { type: Entities.Flag },
  '/create/flags': { type: Entities.Flag },
  // hunts create through their dedicated JSON route, so an override page replaces the
  // generic multipart create factory
  '/create/hunt': { type: Entities.Hunt, override_page: HuntCreate },
  '/create/hunts': { type: Entities.Hunt, override_page: HuntCreate },
  '/create/incident': { type: Entities.Incident },
  '/create/incidents': { type: Entities.Incident },
  '/create/function/compiled': { type: Entities.CompiledFunction },
  '/create/functions/compiled': { type: Entities.CompiledFunction },
  '/create/function/decompiled': { type: Entities.DecompiledFunction },
  '/create/functions/decompiled': { type: Entities.DecompiledFunction },
  '/create/pe/section': { type: Entities.PeSection },
  '/create/pe/sections': { type: Entities.PeSection },
  '/create/pe/import': { type: Entities.PeImport },
  '/create/pe/imports': { type: Entities.PeImport },
  '/create/rule/sigma': { type: Entities.SigmaRule },
  '/create/rules/sigma': { type: Entities.SigmaRule },
  '/create/rule/yara': { type: Entities.YaraRule },
  '/create/rules/yara': { type: Entities.YaraRule },
  '/create/ruleset': { type: Entities.RuleSet },
  '/create/rulesets': { type: Entities.RuleSet },
  '/create/vendor': { type: Entities.Vendor },
  '/create/vendors': { type: Entities.Vendor },
};

export const EntityCreatePathByType: Partial<Record<Entities, string>> = buildPathByType(EntityCreateRoutes, (config) => config.type);

export function getCreatePathByEntity(entity: Entities): string | undefined {
  return EntityCreatePathByType[entity];
}
