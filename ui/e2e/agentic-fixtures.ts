import fs from 'fs';
import { test } from '@playwright/test';

/** The names of the objects the live fixture stack seeds (the shape of its FIXTURES.json). */
export interface LiveFixtures {
  /** The group the fixtures were seeded in */
  group: string;
  /** A second group the test user belongs to */
  second_group: string;
  /** The SHA256 of a sample uploaded to `group` */
  sample_sha256: string;
  /** The name of a seeded toolset */
  toolset: string;
  /** The name of a seeded agent */
  agent: string;
  /** The name of a seeded workflow */
  workflow: string;
  /** The ID of the seeded workflow */
  workflow_id: string;
  /** The ID of a built instance of the seeded workflow */
  workflow_instance: string;
  /** The names of the seeded tools */
  tools: string[];
  /** The published versions of the seeded scan tool */
  scan_versions: string[];
  /** The ID of the seeded scan tool */
  tool_scan_id: string;
  /** The names of the seeded model endpoints */
  model_endpoints: string[];
  /** A model endpoint that carries a ban */
  banned_endpoint: string;
  /** A pipeline in `group` that carries a ban */
  banned_pipeline: string;
}

/**
 * Read the live fixture names, which the stack's run script writes next to itself.
 *
 * Without `THORIUM_FIXTURES` the calling spec file is marked skipped instead of throwing, because a
 * throw while Playwright loads a spec file aborts the whole run, mock-based specs included.
 *
 * @returns The fixture names; the path comes from `THORIUM_FIXTURES`. When it is not set the
 *   result is empty, and no test of the calling file runs to read it.
 */
export function loadFixtures(): LiveFixtures {
  const path = process.env.THORIUM_FIXTURES;
  if (!path) {
    // skip every test of the spec file that is loading the fixtures
    test.skip(true, 'set THORIUM_FIXTURES to the live stack FIXTURES.json');
    return {} as LiveFixtures;
  }
  return JSON.parse(fs.readFileSync(path, 'utf8')) as LiveFixtures;
}

/** Make a unique name for data a test creates. */
export function uniqueName(area: string): string {
  return `e2e-${area}-${Math.random().toString(36).slice(2, 8)}`;
}
