// project imports
import { HuntLane, HuntSelector, HuntStatus } from '@models/entities/hunts';

/**
 * Map a hunt status to a react-bootstrap Badge background variant.
 *
 * Mirrors the reaction status badge vocabulary (Created/Running/Completed/Failed) with
 * Cancelled rendered as a warning.
 *
 * @param status - The hunt status to style.
 * @returns The Badge `bg` variant name.
 */
export function huntStatusVariant(status: HuntStatus): string {
  switch (status) {
    case HuntStatus.Created:
      return 'secondary';
    case HuntStatus.Running:
      return 'primary';
    case HuntStatus.Completed:
      return 'success';
    case HuntStatus.Cancelled:
      return 'warning';
    case HuntStatus.Failed:
      return 'danger';
  }
}

/**
 * Map a hunt status to the themed progress-bar class.
 *
 * Reuses the shared bar classes from upload.scss, which remap Bootstrap's progress vars
 * onto the per-theme `--thorium-*-bar-bg` tokens: active hunts (Created/Running) draw the
 * muted info blue, Completed green, Cancelled amber, Failed crimson.
 *
 * @param status - The hunt status to style.
 * @returns The ProgressBar className.
 */
export function huntBarClass(status: HuntStatus): string {
  switch (status) {
    case HuntStatus.Created:
    case HuntStatus.Running:
      return 'info-bar';
    case HuntStatus.Completed:
      return 'success-bar';
    case HuntStatus.Cancelled:
      return 'warning-bar';
    case HuntStatus.Failed:
      return 'danger-bar';
  }
}

/**
 * Map one lane to its themed progress-bar class.
 *
 * An exhausted lane is done and draws green no matter what the hunt's status is; every
 * other lane follows the hunt status.
 *
 * @param status - The hunt's status.
 * @param lane - The lane to style.
 * @returns The ProgressBar className.
 */
export function laneBarClass(status: HuntStatus, lane: HuntLane): string {
  return lane.exhausted ? 'success-bar' : huntBarClass(status);
}

/**
 * Describe a hunt selector for display.
 *
 * The selector is kept for display only — the frozen snapshot is what scanned — so this
 * renders the submitted shape, including the bare-string `All` variant.
 *
 * @param selector - The selector as stored on the hunt.
 * @returns A short human-readable description.
 */
export function describeSelector(selector: HuntSelector): string {
  // the All variant is a bare string on the wire
  if (typeof selector === 'string') {
    return 'All rules visible at creation';
  }
  if ('Rules' in selector) {
    return `Explicit rules (${selector.Rules.length})`;
  }
  if ('Tag' in selector) {
    return `Rules tagged ${selector.Tag.key} = ${selector.Tag.value}`;
  }
  return 'Rule set';
}

/**
 * The rule set id a selector names, when it is the RuleSet variant.
 *
 * @param selector - The selector as stored on the hunt.
 * @returns The rule set entity id, or null for every other variant.
 */
export function selectorRuleSetId(selector: HuntSelector): string | null {
  if (typeof selector !== 'string' && 'RuleSet' in selector) {
    return selector.RuleSet;
  }
  return null;
}
