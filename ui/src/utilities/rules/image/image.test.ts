import { describe, test, expect } from 'vitest';
import { ImageChecker, PipelineChecker } from './index';
import { removeLine, removeBlock, replaceLine } from '../test-helpers';

const VALID_IMAGE = `group: analysis
name: yara-scanner
image: thorium/yara-scanner:latest
scaler: K8s
timeout: 300
lifetime:
    counter: jobs
    amount: 32
resources:
    cpu: 1000
    memory: 512
display_type: JSON
collect_logs: true
generator: false
dependencies:
    samples:
        location: /tmp/thorium/samples
        strategy: Paths
        naming: Sha256
output_collection:
    handler: Files
    files:
        results: /tmp/thorium/results
description: Scans files with YARA rules`;

const VALID_PIPELINE = `group: analysis
name: triage
order:
    - file-info
    - - yara-scanner
      - clamav
    - report-generator
sla: 604800
description: Standard triage pipeline`;

const imageChecker = new ImageChecker();
const pipelineChecker = new PipelineChecker();

function imageErrors(text: string) {
  return imageChecker.check(text).diagnostics.filter((d) => d.severity === 'error');
}

function imageWarnings(text: string) {
  return imageChecker.check(text).diagnostics.filter((d) => d.severity === 'warning');
}

function imageSuggestions(text: string) {
  return imageChecker.check(text).suggestions;
}

function pipelineErrors(text: string) {
  return pipelineChecker.check(text).diagnostics.filter((d) => d.severity === 'error');
}

function pipelineWarnings(text: string) {
  return pipelineChecker.check(text).diagnostics.filter((d) => d.severity === 'warning');
}

function pipelineSuggestions(text: string) {
  return pipelineChecker.check(text).suggestions;
}



describe('ImageChecker', () => {
  describe('valid image', () => {
    test('produces no errors for valid image request', () => {
      const errs = imageErrors(VALID_IMAGE);
      expect(errs).toHaveLength(0);
    });

    test('produces no warnings for valid image request', () => {
      const warns = imageWarnings(VALID_IMAGE);
      expect(warns).toHaveLength(0);
    });

    test('fully specified image produces no suggestions', () => {
      const s = imageSuggestions(VALID_IMAGE);
      expect(s).toHaveLength(0);
    });
  });

  describe('empty and invalid input', () => {
    test('empty string returns no diagnostics', () => {
      const result = imageChecker.check('');
      expect(result.diagnostics).toHaveLength(0);
      expect(result.suggestions).toHaveLength(0);
    });

    test('invalid YAML syntax returns error', () => {
      const result = imageChecker.check('group: [unclosed');
      const errs = result.diagnostics.filter((d) => d.severity === 'error');
      expect(errs.length).toBeGreaterThan(0);
    });

    test('non-mapping YAML returns error', () => {
      const result = imageChecker.check('- item1\n- item2');
      const errs = result.diagnostics.filter((d) => d.severity === 'error');
      expect(errs.some((e) => e.message.includes('mapping'))).toBe(true);
    });
  });

  describe('required field errors', () => {
    test('missing group', () => {
      const text = removeLine(VALID_IMAGE, 'group:');
      const errs = imageErrors(text);
      expect(errs.some((e) => e.message.includes("Missing required field: 'group'"))).toBe(true);
    });

    test('missing name', () => {
      const text = removeLine(VALID_IMAGE, 'name:');
      const errs = imageErrors(text);
      expect(errs.some((e) => e.message.includes("Missing required field: 'name'"))).toBe(true);
    });
  });

  describe('enum validation', () => {
    test('invalid scaler value', () => {
      const text = replaceLine(VALID_IMAGE, 'scaler:', 'scaler: Docker');
      const errs = imageErrors(text);
      expect(errs.some((e) => e.message.includes("Invalid scaler value: 'Docker'"))).toBe(true);
    });

    test('invalid display_type value', () => {
      const text = replaceLine(VALID_IMAGE, 'display_type:', 'display_type: Binary');
      const errs = imageErrors(text);
      expect(errs.some((e) => e.message.includes("Invalid display_type value: 'Binary'"))).toBe(true);
    });

    test('valid scaler values pass', () => {
      for (const val of ['K8s', 'BareMetal', 'Windows', 'Kvm', 'External']) {
        const text = replaceLine(VALID_IMAGE, 'scaler:', `scaler: ${val}`);
        const errs = imageErrors(text);
        expect(errs.filter((e) => e.message.includes('scaler'))).toHaveLength(0);
      }
    });
  });

  describe('type validation', () => {
    test('timeout must be a number', () => {
      const text = replaceLine(VALID_IMAGE, 'timeout:', 'timeout: fast');
      const errs = imageErrors(text);
      expect(errs.some((e) => e.message.includes("'timeout' must be a number"))).toBe(true);
    });

    test('collect_logs must be a boolean', () => {
      const text = replaceLine(VALID_IMAGE, 'collect_logs:', 'collect_logs: yes_please');
      const errs = imageErrors(text);
      expect(errs.some((e) => e.message.includes("'collect_logs' must be a boolean"))).toBe(true);
    });
  });

  describe('unknown field warnings', () => {
    test('unknown top-level field', () => {
      const text = VALID_IMAGE + '\nfoobar: baz';
      const warns = imageWarnings(text);
      expect(warns.some((w) => w.message.includes("Unknown image field: 'foobar'"))).toBe(true);
    });

    test('unknown resources sub-field', () => {
      const text = VALID_IMAGE.replace('cpu: 1000', 'cpu: 1000\n    gpus: 4');
      const warns = imageWarnings(text);
      expect(warns.some((w) => w.message.includes("Unknown resources field: 'gpus'"))).toBe(true);
    });
  });

  describe('nested validation', () => {
    test('invalid dependency strategy', () => {
      const text = replaceLine(VALID_IMAGE, 'strategy: Paths', '        strategy: Invalid');
      const errs = imageErrors(text);
      expect(errs.some((e) => e.message.includes("Invalid strategy value: 'Invalid'"))).toBe(true);
    });

    test('invalid lifetime counter', () => {
      const text = replaceLine(VALID_IMAGE, 'counter: jobs', '    counter: infinite');
      const errs = imageErrors(text);
      expect(errs.some((e) => e.message.includes("Invalid counter value: 'infinite'"))).toBe(true);
    });
  });

  describe('suggestions', () => {
    test('missing optional fields are suggested', () => {
      const minimal = `group: test\nname: minimal`;
      const s = imageSuggestions(minimal);
      const fields = s.map((sg) => sg.field);
      expect(fields).toContain('description');
      expect(fields).toContain('image');
      expect(fields).toContain('timeout');
      expect(fields).toContain('scaler');
      expect(fields).toContain('display_type');
      expect(fields).toContain('resources');
    });

    test('scaler suggestion includes valid values', () => {
      const minimal = `group: test\nname: minimal`;
      const s = imageSuggestions(minimal);
      const scalerSugg = s.find((sg) => sg.field === 'scaler');
      expect(scalerSugg).toBeDefined();
      expect(scalerSugg!.values).toContain('K8s');
      expect(scalerSugg!.values).toContain('BareMetal');
    });

    test('empty scaler suggests valid values', () => {
      const text = replaceLine(VALID_IMAGE, 'scaler:', 'scaler:');
      const s = imageSuggestions(text);
      const scalerSugg = s.find((sg) => sg.field === 'scaler');
      expect(scalerSugg).toBeDefined();
      expect(scalerSugg!.values).toContain('K8s');
    });
  });
});

describe('PipelineChecker', () => {
  describe('valid pipeline', () => {
    test('produces no errors for valid pipeline request', () => {
      const errs = pipelineErrors(VALID_PIPELINE);
      expect(errs).toHaveLength(0);
    });

    test('produces no warnings for valid pipeline request', () => {
      const warns = pipelineWarnings(VALID_PIPELINE);
      expect(warns).toHaveLength(0);
    });
  });

  describe('required field errors', () => {
    test('missing group', () => {
      const text = removeLine(VALID_PIPELINE, 'group:');
      const errs = pipelineErrors(text);
      expect(errs.some((e) => e.message.includes("Missing required field: 'group'"))).toBe(true);
    });

    test('missing name', () => {
      const text = removeLine(VALID_PIPELINE, 'name:');
      const errs = pipelineErrors(text);
      expect(errs.some((e) => e.message.includes("Missing required field: 'name'"))).toBe(true);
    });

    test('missing order', () => {
      const text = removeBlock(VALID_PIPELINE, 'order');
      const errs = pipelineErrors(text);
      expect(errs.some((e) => e.message.includes("Missing required field: 'order'"))).toBe(true);
    });
  });

  describe('type validation', () => {
    test('sla must be a number', () => {
      const text = replaceLine(VALID_PIPELINE, 'sla:', 'sla: fast');
      const errs = pipelineErrors(text);
      expect(errs.some((e) => e.message.includes("'sla' must be a number"))).toBe(true);
    });

    test('order must be an array', () => {
      const text = replaceLine(VALID_PIPELINE, 'order:', 'order: not-an-array');
      const errs = pipelineErrors(text);
      expect(errs.some((e) => e.message.includes("'order' must be an array"))).toBe(true);
    });
  });

  describe('unknown field warnings', () => {
    test('unknown top-level field', () => {
      const text = VALID_PIPELINE + '\nfoobar: baz';
      const warns = pipelineWarnings(text);
      expect(warns.some((w) => w.message.includes("Unknown pipeline field: 'foobar'"))).toBe(true);
    });
  });

  describe('suggestions', () => {
    test('missing optional fields are suggested', () => {
      const minimal = `group: test\nname: minimal\norder:\n    - tool1`;
      const s = pipelineSuggestions(minimal);
      const fields = s.map((sg) => sg.field);
      expect(fields).toContain('description');
      expect(fields).toContain('sla');
      expect(fields).toContain('triggers');
    });
  });
});
