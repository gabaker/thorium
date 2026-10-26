import { describe, expect, it } from 'vitest';

// project imports
import { NAV_ITEMS, NavSubItem } from './navConfig';

/** Every sub item in the nav, primary and secondary. */
const allSubItems = (): NavSubItem[] =>
  NAV_ITEMS.flatMap((category) => [...(category.children ?? []), ...(category.secondaryChildren ?? [])]);

describe('NAV_ITEMS legacy markers', () => {
  it('marks only the images screen as legacy with a flag', () => {
    const legacyPaths = allSubItems()
      .filter((item) => item.legacy)
      .map((item) => item.path)
      .sort();
    expect(legacyPaths).toEqual(['/images']);
  });

  it('keeps the legacy marker out of the label so the sidebar can render it as a badge', () => {
    for (const item of allSubItems()) {
      expect(item.label.toLowerCase()).not.toContain('legacy');
    }
  });

  it('keeps every Tools group label short enough to fit the sidebar beside a badge', () => {
    const tools = NAV_ITEMS.find((category) => category.label === 'Tools');
    for (const item of tools?.children ?? []) {
      expect(item.label.length).toBeLessThanOrEqual(10);
    }
  });
});
