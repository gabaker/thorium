import type { IconType } from 'react-icons';
import {
  FaCheckCircle,
  FaMinusCircle,
  FaMoon,
  FaPlayCircle,
  FaRegCircle,
  FaRegClock,
  FaSpinner,
  FaStopCircle,
  FaTimesCircle,
} from 'react-icons/fa';

// project imports
import { badgeColor, BadgeTone, statusTone } from '@components/shared/badges/Badge';
import { JobStatus, ReactionStatus } from '@models/reactions';

/**
 * The states a stage of a run can be in when none of its jobs says otherwise; the API has no status for
 * these, so the page derives them from the run's status and current stage.
 */
export enum DerivedStageState {
  /** The run has not reached this stage yet */
  Waiting = 'Waiting',
  /** The run finished before it reached this stage */
  Skipped = 'Skipped',
  /** The run finished (failed or was cancelled) while this stage's jobs were still queued or running */
  Stopped = 'Stopped',
}

/** Every status the run pages draw: the API's reaction and job statuses and the derived stage states. */
export type IndicatorStatus = `${ReactionStatus}` | `${JobStatus}` | `${DerivedStageState}`;

/** How one status is drawn: never by colour alone, always with an icon shape and a text label. */
export interface StatusIndicatorStyle {
  /** The text label, read by screen readers and shown in pills */
  label: string;
  /** The shared status tone, which the status pill is drawn with */
  tone: BadgeTone;
  /** The colour of the icon and of a diagram node's accent: the badge colour of the tone */
  color: string;
  /** The icon, whose shape differs for every status */
  icon: IconType;
  /** Whether the icon spins, for work in progress */
  spin: boolean;
  /** A one line description for tooltips */
  description: string;
}

/**
 * Build an indicator style.
 *
 * @param label - The text label, whose shared status tone sets the badge tone and the colour.
 * @param icon - The icon.
 * @param description - The tooltip text.
 * @param spin - Whether the icon spins.
 * @returns The style.
 */
function indicator(label: string, icon: IconType, description: string, spin = false): StatusIndicatorStyle {
  // the shared status tone colours the pill and the icon, so a status has one colour on every page
  const tone = statusTone(label);
  return { label, tone, color: badgeColor(tone), icon, spin, description };
}

/**
 * The one indicator of every status. The record type makes a status the API adds without an entry here a
 * type error, and the tests check every enum value at run time.
 */
export const STATUS_INDICATORS: Record<IndicatorStatus, StatusIndicatorStyle> = {
  Created: indicator('Created', FaRegClock, 'Created and queued, not running yet'),
  Started: indicator('Started', FaPlayCircle, 'At least one stage has started'),
  Running: indicator('Running', FaSpinner, 'Claimed by a worker and running', true),
  Completed: indicator('Completed', FaCheckCircle, 'Completed'),
  Failed: indicator('Failed', FaTimesCircle, 'Failed with an error, or cancelled'),
  Sleeping: indicator('Sleeping', FaMoon, 'A generator waiting to be respawned'),
  Waiting: indicator('Waiting', FaRegCircle, 'The run has not reached this stage yet'),
  Skipped: indicator('Skipped', FaMinusCircle, 'The run ended before this stage'),
  Stopped: indicator('Stopped', FaStopCircle, 'The run ended while this stage was queued or running'),
};

/** The style drawn for a status the page does not know, so a new API status still shows its own text. */
const UNKNOWN_INDICATOR = indicator('Unknown', FaRegCircle, 'A status this page does not know');

/**
 * Check whether a value is a status the run pages know.
 *
 * @param status - The value to check.
 * @returns Whether it has an indicator.
 */
export function isIndicatorStatus(status: string): status is IndicatorStatus {
  return Object.prototype.hasOwnProperty.call(STATUS_INDICATORS, status);
}

/**
 * Normalise a status as the API writes it in different places: JSON quoted (`"Created"`) in a job's
 * creation update, and lowercase (`running`) when a worker claims a job.
 *
 * @param raw - The status text.
 * @returns The status with quotes removed and the first letter upper case.
 */
export function normaliseStatus(raw: string): string {
  const unquoted = raw.replace(/^"(.*)"$/, '$1').trim();
  return unquoted.charAt(0).toUpperCase() + unquoted.slice(1);
}

/**
 * Get the indicator of a status.
 *
 * @param status - A reaction status, job status, or derived stage state, in any of the API's spellings.
 * @returns Its indicator; an unknown status keeps its own text as the label.
 */
export function statusIndicator(status: string): StatusIndicatorStyle {
  const normalised = normaliseStatus(status);
  if (isIndicatorStatus(normalised)) return STATUS_INDICATORS[normalised];
  return { ...UNKNOWN_INDICATOR, label: normalised || UNKNOWN_INDICATOR.label };
}
