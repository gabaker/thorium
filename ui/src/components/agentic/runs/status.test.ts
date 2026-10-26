import { describe, expect, it } from 'vitest';

// project imports
import { statusTone } from './display';
import { DerivedStageState, isIndicatorStatus, normaliseStatus, STATUS_INDICATORS, statusIndicator } from './status';
import { BadgeTone } from '@components/agentic/shared';
import { JobStatus, ReactionStatus } from '@models/reactions';

/** Every status the API returns for reactions and jobs, and every state the page derives. */
const ALL_STATUSES: string[] = [...Object.values(ReactionStatus), ...Object.values(JobStatus), ...Object.values(DerivedStageState)];

describe('STATUS_INDICATORS', () => {
  it('covers every reaction status, job status, and derived stage state', () => {
    for (const status of ALL_STATUSES) {
      expect(isIndicatorStatus(status), status).toBe(true);
      expect(STATUS_INDICATORS[status as keyof typeof STATUS_INDICATORS].label).toBe(status);
    }
  });
  it('has no entry beyond those statuses, so the map and the enums stay in step', () => {
    expect(Object.keys(STATUS_INDICATORS).sort()).toEqual([...new Set(ALL_STATUSES)].sort());
  });
  it('gives every status its own icon, so no status rests on colour alone', () => {
    const icons = Object.values(STATUS_INDICATORS).map((style) => style.icon);
    expect(new Set(icons).size).toBe(icons.length);
  });
  it('maps statuses to the shared tone names', () => {
    const tones = Object.fromEntries(Object.entries(STATUS_INDICATORS).map(([status, style]) => [status, style.tone]));
    expect(tones).toEqual({
      Created: BadgeTone.Neutral,
      Started: BadgeTone.Active,
      Running: BadgeTone.Active,
      Completed: BadgeTone.Ok,
      Failed: BadgeTone.Error,
      Sleeping: BadgeTone.Neutral,
      Waiting: BadgeTone.Neutral,
      Skipped: BadgeTone.Neutral,
      Stopped: BadgeTone.Warn,
    });
  });
  it('spins only for work in progress', () => {
    const spinning = Object.entries(STATUS_INDICATORS)
      .filter(([, style]) => style.spin)
      .map(([status]) => status);
    expect(spinning).toEqual(['Running']);
  });
  it('colours through theme variables only', () => {
    for (const style of Object.values(STATUS_INDICATORS))
      expect(style.color).toMatch(/^var\(--thorium-[a-z-]+(, var\(--thorium-[a-z-]+\))?\)$/);
  });
});

describe('statusIndicator', () => {
  it('reads the quoted and lowercase spellings the API writes', () => {
    expect(normaliseStatus('"Created"')).toBe('Created');
    expect(normaliseStatus('running')).toBe('Running');
    expect(statusIndicator('running').label).toBe('Running');
    expect(statusIndicator('"Created"').label).toBe('Created');
  });
  it('keeps the text of a status it does not know', () => {
    const style = statusIndicator('Paused');
    expect(style.label).toBe('Paused');
    expect(style.tone).toBe(BadgeTone.Neutral);
  });
});

describe('statusTone', () => {
  it('draws reaction statuses with the same tone as the indicator', () => {
    for (const status of Object.values(ReactionStatus)) expect(statusTone(status)).toBe(statusIndicator(status).tone);
    expect(statusTone(ReactionStatus.Completed)).toBe(BadgeTone.Ok);
    expect(statusTone(ReactionStatus.Failed)).toBe(BadgeTone.Error);
  });
});
