import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';
import { describe, expect, it } from 'vitest';

/** The `src` directory, found from this file's location. */
const SRC = join(__dirname, '..', '..', '..');

/** The directories whose every source file is checked: the agentic pages and components. */
const CHECKED_DIRS = [
  'components/agentic',
  'pages/agents',
  'pages/models',
  'pages/reactions',
  'pages/runs',
  'pages/toolsets',
  'pages/tools',
  'pages/workflows',
];

/** The files outside those directories that were moved onto the shared Badge and must stay on it. */
const CHECKED_FILES = [
  'components/entities/details/override_pages/FileInfo.tsx',
  'components/pages/NavBanner.tsx',
  'components/pages/files/reactions/reactions.tsx',
  'components/pages/pipelines/PipelineInfo.styled.tsx',
  'components/pages/pipelines/PipelineInfo.tsx',
  'components/pages/users/RunTokenTable.tsx',
  'components/pages/users/ScopedTokens.styled.tsx',
  'components/shared/tabs/Tabs.tsx',
  'dashboards/Dashboard/AnalysisStatusPanel.tsx',
  'dashboards/Dashboard/styles.ts',
  'pages/images/ImageBrowsing.tsx',
  'pages/pipelines/PipelineBrowsing.styled.tsx',
  'pages/pipelines/PipelineBrowsing.tsx',
  'pages/users/UserProfile.tsx',
];

/** Matches a styled badge or pill declaration and its template, so a radius is only flagged where a badge is drawn. */
const BADGE_STYLE = /const\s+\w*(?:Badge|Pill)\w*\s*=\s*styled[^`]*`([^`]*)`/g;

/** Matches a fully round radius (`999px` or `9999px`). */
const ROUND_RADIUS = /border-radius:\s*9{3,4}px/;

/** Matches a named import of `Badge` from react-bootstrap. */
const BOOTSTRAP_BADGE = /import\s*\{[^}]*\bBadge\b[^}]*\}\s*from\s*'react-bootstrap'/;

/**
 * List the source files under a directory.
 *
 * @param dir - The absolute directory.
 * @returns The `.ts` and `.tsx` files in it, tests excluded.
 */
function sourceFiles(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return sourceFiles(path);
    return /\.tsx?$/.test(name) && !/\.test\.tsx?$/.test(name) ? [path] : [];
  });
}

/** The checked files, as paths under `src`. */
const FILES = [...CHECKED_DIRS.flatMap((dir) => sourceFiles(join(SRC, dir)).map((path) => relative(SRC, path))), ...CHECKED_FILES];

describe('badge consistency', () => {
  it('checks files that exist', () => {
    expect(FILES.length).toBeGreaterThan(CHECKED_FILES.length);
    expect(FILES.filter((file) => !existsSync(join(SRC, file)))).toEqual([]);
  });

  it('uses no react-bootstrap Badge in the migrated files', () => {
    const offenders = FILES.filter((file) => BOOTSTRAP_BADGE.test(readFileSync(join(SRC, file), 'utf8')));
    expect(offenders).toEqual([]);
  });

  it('draws no fully round (999px) badge or pill', () => {
    const offenders = FILES.filter((file) =>
      [...readFileSync(join(SRC, file), 'utf8').matchAll(BADGE_STYLE)].some((match) => ROUND_RADIUS.test(match[1])),
    );
    expect(offenders).toEqual([]);
  });
});
