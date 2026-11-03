/** Options that pin how {@link describeTime} formats, so tests and callers get stable text. */
export interface TimeFormatOptions {
  /** The instant to compare against, in milliseconds since the epoch; the current time when left out */
  now?: number;
  /** The BCP 47 locale to format in; the reader's locale when left out */
  locale?: string;
  /** The IANA time zone to show the absolute time in; the reader's zone when left out */
  timeZone?: string;
}

/** A timestamp described for a reader. */
export interface DescribedTime {
  /** Whether the text was a readable date */
  valid: boolean;
  /** The time relative to now, such as `in 3 hours` or `2 days ago`; the original text when invalid */
  relative: string;
  /** The local date and time, such as `Oct 30, 2026, 14:05`; the year is left out for the current year */
  absolute: string;
  /** The full ISO 8601 timestamp for a tooltip; the original text when invalid */
  iso: string;
  /** Whether the instant is before now */
  past: boolean;
}

/** The units a relative time is counted in, each with its length in seconds, largest first. */
const RELATIVE_UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
  ['year', 365 * 24 * 3600],
  ['month', 30 * 24 * 3600],
  ['day', 24 * 3600],
  ['hour', 3600],
  ['minute', 60],
  ['second', 1],
];

/**
 * Parse an API timestamp into milliseconds since the epoch.
 *
 * The API writes RFC 3339 text (`2026-10-29T10:26:06.176140017Z`) and, in some status update fields,
 * a space separated form ending in ` UTC` (`2026-11-05 20:26:06.175074104 UTC`); both are accepted.
 *
 * @param text - The timestamp text.
 * @returns The instant, or `NaN` when the text is not a date.
 */
export function parseTimestamp(text: string): number {
  const direct = Date.parse(text);
  if (!Number.isNaN(direct)) return direct;
  const match = /^(\d{4}-\d{2}-\d{2})[ T](\d{2}:\d{2}:\d{2}(?:\.\d+)?)\s*UTC$/.exec(text.trim());
  return match ? Date.parse(`${match[1]}T${match[2]}Z`) : Number.NaN;
}

/**
 * Describe a timestamp as a relative time, a local date and time, and an ISO timestamp.
 *
 * @param text - The timestamp text from the API.
 * @param options - The instant to compare against, the locale, and the time zone.
 * @returns The described time; an unreadable date keeps its text in every field and is marked invalid.
 */
export function describeTime(text: string, options: TimeFormatOptions = {}): DescribedTime {
  const instant = parseTimestamp(text);
  if (Number.isNaN(instant)) return { valid: false, relative: text, absolute: text, iso: text, past: false };
  const now = options.now ?? Date.now();
  const date = new Date(instant);
  // the year only adds noise for a date in the current year
  const sameYear = date.getUTCFullYear() === new Date(now).getUTCFullYear();
  const absolute = new Intl.DateTimeFormat(options.locale, {
    year: sameYear ? undefined : 'numeric',
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
    hourCycle: 'h23',
    timeZone: options.timeZone,
  }).format(date);
  const seconds = Math.round((instant - now) / 1000);
  const [unit, size] = RELATIVE_UNITS.find(([, length]) => Math.abs(seconds) >= length) ?? RELATIVE_UNITS[RELATIVE_UNITS.length - 1];
  const relative = new Intl.RelativeTimeFormat(options.locale, { numeric: 'auto' }).format(Math.round(seconds / size), unit);
  return { valid: true, relative, absolute, iso: date.toISOString(), past: instant < now };
}

/**
 * Format a timestamp as a relative time with the local date and time after it.
 *
 * @param text - The timestamp text from the API.
 * @param options - The instant to compare against, the locale, and the time zone.
 * @returns For example `in 3 hours (Oct 30, 2026, 14:05)`, or the text unchanged when it is not a date.
 */
export function formatRelativeTimestamp(text: string, options: TimeFormatOptions = {}): string {
  const described = describeTime(text, options);
  return described.valid ? `${described.relative} (${described.absolute})` : text;
}
