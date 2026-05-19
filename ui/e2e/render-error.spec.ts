import { test, expect } from '@playwright/test';
import path from 'path';
import { snapshot, setupMockAuth } from './helpers';

const SCREENSHOT_DIR = path.join(import.meta.dirname, 'screenshots');
const ERR_TEXT = 'error occurred while rendering';

test.describe('RenderErrorAlert positioning', () => {
  test.beforeEach(async ({ page }) => {
    await setupMockAuth(page);
  });

  // page=true: the top-level ErrorBoundary in Thorium.tsx renders <RenderErrorAlert /> as a
  // sibling of the fixed nav bar. The alert must sit BELOW the nav, not under it.
  test('page=true: alert renders below the fixed nav bar', async ({ page }) => {
    await page.goto('/test/alerts?crash=1');
    const alert = page.locator('pre', { hasText: ERR_TEXT });
    await expect(alert.first()).toBeVisible({ timeout: 15000 });

    const m = await page.evaluate((needle) => {
      const nav = document.querySelector('nav') as HTMLElement;
      const pre = Array.from(document.querySelectorAll('pre')).find((p) => (p.textContent || '').includes(needle));
      const banner = pre!.parentElement as HTMLElement; // AlertBanner bordered box
      const wrapper = banner.parentElement as HTMLElement; // PageWrapper
      const navRect = nav.getBoundingClientRect();
      const bannerRect = banner.getBoundingClientRect();
      return {
        navBottom: Math.round(navRect.bottom),
        navHeight: Math.round(navRect.height),
        navPosition: getComputedStyle(nav).position,
        bannerTop: Math.round(bannerRect.top),
        wrapperMarginTop: getComputedStyle(wrapper).marginTop,
        overlapPx: Math.round(navRect.bottom - bannerRect.top), // >0 means alert is hidden under the nav
      };
    }, ERR_TEXT);

    console.log('PAGE=TRUE metrics:', JSON.stringify(m));
    await snapshot(page, SCREENSHOT_DIR, 'render-error-page-true');

    expect(m.navPosition).toBe('fixed');
    expect(m.wrapperMarginTop).toBe('60px'); // the fix: real margin, not inert `top`
    expect(m.bannerTop).toBeGreaterThanOrEqual(m.navBottom); // alert is fully below the nav
  });

  // page=false: nested boundaries (AssociationGraph/AssociationTree) render the inline variant,
  // which has no PageWrapper and therefore none of the nav/sidebar offsets.
  test('page=false: alert renders inline (no nav/sidebar offset)', async ({ page }) => {
    await page.goto('/test/alerts?inline=1');
    await expect(page.getByTestId('inline-marker')).toBeVisible({ timeout: 15000 });
    const alert = page.locator('pre', { hasText: ERR_TEXT });
    await expect(alert.first()).toBeVisible();

    const m = await page.evaluate((needle) => {
      const pre = Array.from(document.querySelectorAll('pre')).find((p) => (p.textContent || '').includes(needle));
      const banner = pre!.parentElement as HTMLElement; // AlertBanner (no PageWrapper around it)
      const parent = banner.parentElement as HTMLElement;
      return {
        bannerMarginTop: getComputedStyle(banner).marginTop,
        bannerMarginLeft: getComputedStyle(banner).marginLeft,
        parentMarginLeft: getComputedStyle(parent).marginLeft,
      };
    }, ERR_TEXT);

    console.log('PAGE=FALSE metrics:', JSON.stringify(m));
    await snapshot(page, SCREENSHOT_DIR, 'render-error-page-false');

    // inline path is not wrapped in PageWrapper → no 60px top / 160px (10rem) left offsets
    expect(m.bannerMarginTop).not.toBe('60px');
    expect(m.parentMarginLeft).not.toBe('160px');
  });

  // Root cause: `top` is inert on a static element; `margin-top` is not. Replays the exact CSS
  // mechanic the bug/fix turns on, in a collapse-safe flex column so the numbers are unambiguous.
  test('root cause: top is ignored on a static element, margin-top is not', async ({ page }) => {
    await page.goto('/test/alerts');
    const offsets = await page.evaluate(() => {
      function offsetWithin(style: string): number {
        const c = document.createElement('div');
        c.style.cssText = 'display:flex; flex-direction:column;'; // flex items do not margin-collapse
        const ref = document.createElement('div');
        ref.style.height = '0px';
        const probe = document.createElement('div');
        probe.style.cssText = style;
        probe.textContent = 'x';
        c.append(ref, probe);
        document.body.appendChild(c);
        const off = probe.getBoundingClientRect().top - ref.getBoundingClientRect().top;
        c.remove();
        return Math.round(off);
      }
      return {
        oldStyle: offsetWithin('top:60px;'), // the buggy declaration (static element + top)
        newStyle: offsetWithin('margin-top:60px;'), // the fix
      };
    });

    console.log('ROOT CAUSE offsets:', JSON.stringify(offsets));
    expect(offsets.oldStyle).toBeLessThan(2); // `top` had no effect → ~0
    expect(offsets.newStyle).toBeGreaterThanOrEqual(58); // `margin-top` applied → ~60
  });
});
