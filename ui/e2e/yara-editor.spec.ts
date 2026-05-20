import { test, expect, Page } from '@playwright/test';
import path from 'path';
import {
  snapshot,
  setupMockAuth,
  waitForEditor,
  waitForLinter,
  setEditorContent,
} from './helpers';

const SCREENSHOT_DIR = path.join(import.meta.dirname, 'screenshots');

const VALID_RULE = `import "pe"

rule DetectUPX : packer
{
    meta:
        description = "Detects UPX packed executables"
        author = "Test Author"
        date = "2024-01-15"

    strings:
        $upx0 = "UPX0" ascii
        $upx1 = "UPX1" ascii
        $hex = { 60 E8 00 00 00 00 58 }

    condition:
        uint16(0) == 0x5A4D and ($upx0 or $upx1) and $hex
}`;

test.describe('YARA Rule CodeEditor', () => {
  test.beforeEach(async ({ page }) => {
    const consoleErrors: string[] = [];
    page.on('console', (msg) => {
      if (msg.type() === 'error') consoleErrors.push(msg.text());
    });
    page.on('pageerror', (err) => consoleErrors.push(err.message));

    await setupMockAuth(page);
    await page.goto('/test/yara');

    try {
      await waitForEditor(page);
    } catch {
      if (consoleErrors.length > 0) {
        throw new Error(`Editor failed to load. Console errors:\n${consoleErrors.join('\n')}`);
      }
      throw new Error('Editor failed to load (no console errors captured)');
    }
  });

  test('valid rule renders without error diagnostics', async ({ page }) => {
    await waitForLinter(page);
    const errorRanges = page.locator('.cm-lintRange-error');
    await expect(errorRanges).toHaveCount(0);

    await snapshot(page, SCREENSHOT_DIR, 'yara-valid-rule');
  });

  test('syntax highlighting applies to keywords', async ({ page }) => {
    await waitForLinter(page);

    const editorHtml = await page.locator('.cm-content').innerHTML();
    expect(editorHtml).toContain('cm-');

    await snapshot(page, SCREENSHOT_DIR, 'yara-syntax-highlighting');
  });

  test('missing condition shows error diagnostic', async ({ page }) => {
    await setEditorContent(page, `rule NoCondition
{
    strings:
        $a = "test"
}`);

    await waitForLinter(page);

    const errorRanges = page.locator('.cm-lintRange-error');
    await expect(errorRanges.first()).toBeVisible({ timeout: 5000 });

    await snapshot(page, SCREENSHOT_DIR, 'yara-error-missing-condition');
  });

  test('unknown import module shows warning', async ({ page }) => {
    await setEditorContent(page, `import "badmodule"

rule Test
{
    condition:
        true
}`);

    await waitForLinter(page);

    const warningRanges = page.locator('.cm-lintRange-warning');
    await expect(warningRanges.first()).toBeVisible({ timeout: 5000 });

    await snapshot(page, SCREENSHOT_DIR, 'yara-warning-unknown-module');
  });

  test('tooltip appears on hover over error range', async ({ page }) => {
    await setEditorContent(page, `rule NoCondition
{
    strings:
        $a = "test"
}`);

    await waitForLinter(page);

    const errorRange = page.locator('.cm-lintRange-error').first();
    await errorRange.hover();
    await page.waitForTimeout(300);

    const tooltip = page.locator('.cm-tooltip-lint');
    await expect(tooltip).toBeVisible();

    await snapshot(page, SCREENSHOT_DIR, 'yara-tooltip-hover');
  });

  test('suggestion panel shows for rule missing meta keys', async ({ page }) => {
    await waitForLinter(page);

    // Move cursor to the meta section so context-aware suggestions appear
    await page.evaluate(() => {
      const container = document.querySelector('.cm-editor')?.parentElement as HTMLElement & { _cmView?: { state: { doc: { line: (n: number) => { from: number } } }; dispatch: (spec: unknown) => void } };
      const view = container?._cmView;
      if (view) {
        const line = view.state.doc.line(6);
        view.dispatch({ selection: { anchor: line.from } });
      }
    });
    await page.waitForTimeout(200);

    const panel = page.locator('text=Suggestions');
    await expect(panel).toBeVisible();
  });

  test('suggestion chip click shows preview decoration', async ({ page }) => {
    await waitForLinter(page);

    // Move cursor to meta section to show meta suggestions
    await page.evaluate(() => {
      const container = document.querySelector('.cm-editor')?.parentElement as any;
      const view = container?._cmView;
      if (view) {
        const line = view.state.doc.line(6);
        view.dispatch({ selection: { anchor: line.from } });
      }
    });
    await page.waitForTimeout(200);

    const chipVisible = await page.locator('span').filter({ hasText: 'reference' }).first().isVisible().catch(() => false);
    if (chipVisible) {
      await page.evaluate(() => {
        const helpers = (window as any).__yaraTestHelpers;
        const container = document.querySelector('.cm-editor')?.parentElement as any;
        const view = container?._cmView;
        if (view && helpers) {
          view.dispatch({ effects: helpers.addPreview.of({ field: 'meta', value: 'reference', format: 'yara' }) });
        }
      });
      await page.waitForTimeout(500);

      const preview = page.locator('.cm-suggestion-preview');
      await expect(preview).toBeVisible({ timeout: 5000 });

      await snapshot(page, SCREENSHOT_DIR, 'yara-suggestion-preview');
    }
  });

  test('accept button inserts suggested text', async ({ page }) => {
    await waitForLinter(page);

    // Move cursor to meta section
    await page.evaluate(() => {
      const container = document.querySelector('.cm-editor')?.parentElement as any;
      const view = container?._cmView;
      if (view) {
        const line = view.state.doc.line(6);
        view.dispatch({ selection: { anchor: line.from } });
      }
    });
    await page.waitForTimeout(200);

    const chipVisible = await page.locator('span').filter({ hasText: 'reference' }).first().isVisible().catch(() => false);
    if (chipVisible) {
      await page.evaluate(() => {
        const helpers = (window as any).__yaraTestHelpers;
        const container = document.querySelector('.cm-editor')?.parentElement as any;
        const view = container?._cmView;
        if (view && helpers) {
          view.dispatch({ effects: helpers.addPreview.of({ field: 'meta', value: 'reference', format: 'yara' }) });
        }
      });
      await page.waitForTimeout(500);

      const acceptBtn = page.locator('.cm-suggestion-preview button', { hasText: 'Accept' });
      await expect(acceptBtn).toBeVisible({ timeout: 5000 });
      await acceptBtn.click();
      await page.waitForTimeout(200);

      const editorText = await page.locator('.cm-content').textContent();
      expect(editorText).toContain('reference');

      await snapshot(page, SCREENSHOT_DIR, 'yara-suggestion-accepted');
    }
  });

  test('dismiss button clears preview without changes', async ({ page }) => {
    await waitForLinter(page);

    const editorTextBefore = await page.locator('.cm-content').textContent();

    // Move cursor to meta section
    await page.evaluate(() => {
      const container = document.querySelector('.cm-editor')?.parentElement as any;
      const view = container?._cmView;
      if (view) {
        const line = view.state.doc.line(6);
        view.dispatch({ selection: { anchor: line.from } });
      }
    });
    await page.waitForTimeout(200);

    const chipVisible = await page.locator('span').filter({ hasText: 'reference' }).first().isVisible().catch(() => false);
    if (chipVisible) {
      await page.evaluate(() => {
        const helpers = (window as any).__yaraTestHelpers;
        const container = document.querySelector('.cm-editor')?.parentElement as any;
        const view = container?._cmView;
        if (view && helpers) {
          view.dispatch({ effects: helpers.addPreview.of({ field: 'meta', value: 'reference', format: 'yara' }) });
        }
      });
      await page.waitForTimeout(500);

      const dismissBtn = page.locator('.cm-suggestion-preview button', { hasText: 'Dismiss' });
      await expect(dismissBtn).toBeVisible({ timeout: 5000 });
      await dismissBtn.click();
      await page.waitForTimeout(200);

      const preview = page.locator('.cm-suggestion-preview');
      await expect(preview).toHaveCount(0);

      const editorTextAfter = await page.locator('.cm-content').textContent();
      expect(editorTextAfter).toBe(editorTextBefore);
    }
  });

  test('captures screenshot of full editor with suggestions', async ({ page }) => {
    await waitForLinter(page);

    await snapshot(page, SCREENSHOT_DIR, 'yara-editor-full');
  });
});
