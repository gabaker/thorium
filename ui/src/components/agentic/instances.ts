import { Severity } from '@components/shared/alerts/AlertBanner';

/** The parts of a workflow or toolset instance the request notices read. */
export interface NamedInstance {
  /** The instance's id */
  id: string;
  /** The instance's name (`<root>#r<revision>`) */
  name: string;
  /** The errors that made the instance Invalid; absent when the instance is Ready or its status is unknown */
  invalid?: string[];
}

/** A page notice and how it is shown. */
export interface Notice {
  /** The notice text */
  text: string;
  /** The banner tone */
  severity: Severity;
}

/** How long and how often to look for the instance a request should produce. */
export interface WatchOptions {
  /** How many times to read the instances */
  tries: number;
  /** The wait before each read, in milliseconds */
  delayMs: number;
  /** Waits for a number of milliseconds; replaced in tests */
  sleep?: (ms: number) => Promise<void>;
  /** Tells whether to stop watching, such as when the page was left; checked before each read */
  cancelled?: () => boolean;
}

/**
 * How long the pages watch for a requested build: the event handler took 12 to 13 seconds to build
 * in trials, so the window covers that with room to spare.
 */
export const REQUEST_WATCH: Pick<WatchOptions, 'tries' | 'delayMs'> = { tries: 15, delayMs: 2000 };

/**
 * Word the notice shown as soon as an instance request is accepted.
 *
 * The backend queues a build but skips it when the newest instance already has the same digest,
 * so the notice never promises a new instance.
 *
 * @param newest - The newest existing instance, if any.
 * @returns The notice text.
 */
export function requestedNotice(newest: NamedInstance | undefined): string {
  const since = newest ? ` since ${newest.name}` : '';
  return `Instance requested: the build was queued. A new revision is built only if the configuration changed${since}.`;
}

/**
 * Watch for the instance an accepted request builds, and word the outcome.
 *
 * @param read - Reads the current instances, newest first.
 * @param before - The instances that existed when the request was made, newest first.
 * @param options - How many times to read and how long to wait before each read.
 * @returns A success notice naming the instance that was built, a warning notice when that instance is
 * Invalid and so cannot be used, or an informational notice that none has appeared yet and which one is newest.
 */
export async function watchRequestedInstance(
  read: () => Promise<NamedInstance[]>,
  before: NamedInstance[],
  options: WatchOptions,
): Promise<Notice> {
  const sleep = options.sleep ?? ((ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms)));
  const known = new Set(before.map((item) => item.id));
  for (let attempt = 0; attempt < options.tries; attempt++) {
    // give the event handler time to build before each read
    await sleep(options.delayMs);
    // stop reading once the page that asked is gone
    if (options.cancelled?.()) return { text: '', severity: Severity.Info };
    const current = await read();
    const built = current.find((item) => !known.has(item.id));
    if (built) return builtNotice(built);
  }
  // nothing showed up in the window: either the configuration did not change, so no build was made,
  // or a slow build is still running; the client can't tell which, so the notice names both
  const text =
    before.length > 0
      ? `No new instance has appeared: ${before[0].name} is the newest. Either the configuration has not changed since it, or the build is still running. Reload to check.`
      : 'No instance has appeared yet. The build may still be running. Reload to check.';
  return { text, severity: Severity.Info };
}

/**
 * Word the notice for a newly built instance, which is a warning when the build left it Invalid.
 *
 * @param built - The instance that appeared.
 * @returns The notice for the instance.
 */
function builtNotice(built: NamedInstance): Notice {
  // a Ready instance, or one whose status is unknown, was simply built
  if (built.invalid === undefined) return { text: `Instance ${built.name} was built.`, severity: Severity.Success };
  // an Invalid instance exists but cannot run; name why so the notice does not read as a success
  const why = built.invalid.length > 0 ? `: ${built.invalid.join('; ')}` : '';
  return { text: `Instance ${built.name} was built but is Invalid${why}.`, severity: Severity.Warning };
}

/**
 * Tell whether the newest instance was built after the root entity was last read.
 *
 * The root records its highest revision, so an instance with a higher revision means the page's copy
 * of the root is stale.
 *
 * @param revisions - The revisions of the instances just read.
 * @param highest - The highest revision recorded on the root as the page last read it.
 * @returns Whether the root should be read again.
 */
export function rootIsStale(revisions: number[], highest: number): boolean {
  return Math.max(0, ...revisions) > highest;
}
