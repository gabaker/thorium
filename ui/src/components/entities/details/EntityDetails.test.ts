import { describe, it, expect } from 'vitest';

// project imports
import { isEntityGoneError, nextErrorAfterPollSuccess } from './EntityDetails';
import { getCreatePathByEntity } from '../create/EntityCreateRoutes';
import { Entities } from '@models/entities';

describe('isEntityGoneError', () => {
  it('treats the API JSON-body 404 shape as gone', () => {
    // `not_found!` on the server renders as `{"error": "Entity <id> not found"}`
    expect(isEntityGoneError('Get Entity: Entity 0a658ba7-78a9-4a2e-8df3-7fbf18960286 not found')).toBe(true);
  });

  it('treats the bodyless 404 shape as gone', () => {
    // a 404 with no parseable body falls through to the bare status-code format
    expect(isEntityGoneError('Failed to Get Entity: 404')).toBe(true);
  });

  it('keeps polling through transient server errors', () => {
    expect(isEntityGoneError('Failed to Get Entity: 500')).toBe(false);
  });

  it('keeps polling through network blips', () => {
    // an API pod roll surfaces as a no-response axios error, not a 404
    expect(isEntityGoneError('Failed to receive a Get Entity request response: "[object XMLHttpRequest]')).toBe(false);
  });
});

describe('nextErrorAfterPollSuccess', () => {
  it('clears the banner the poll itself raised', () => {
    expect(nextErrorAfterPollSuccess('Failed to Get Entity: 500', 'Failed to Get Entity: 500')).toBe('');
  });

  it('preserves an error a user action raised after the failed tick', () => {
    expect(nextErrorAfterPollSuccess('Cancel Hunt: Permission Denied', 'Failed to Get Entity: 500')).toBe('Cancel Hunt: Permission Denied');
  });

  it('leaves the page untouched when the poll never failed', () => {
    expect(nextErrorAfterPollSuccess('', null)).toBe('');
    expect(nextErrorAfterPollSuccess('Delete Entity: Permission Denied', null)).toBe('Delete Entity: Permission Denied');
  });
});

// the details page renders Create/Upload only when a create route exists and navigates to that
// route; pin the two kinds the gate most depends on so a route-map change resurfaces the old
// dead-link / phantom-button bug here instead of in production
describe('create/upload gating inputs', () => {
  it('machine-written hits have no create path, so the buttons stay suppressed', () => {
    expect(getCreatePathByEntity(Entities.YaraRuleHit)).toBeUndefined();
  });

  it('sigma rules create through their canonical nested route, not a hand-built /create/sigmarule', () => {
    expect(getCreatePathByEntity(Entities.SigmaRule)).toBe('/create/rule/sigma');
  });
});
