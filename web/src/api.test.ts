import { test } from 'node:test';
import assert from 'node:assert/strict';
import { apply, type ServerState } from './api.ts';

const empty: ServerState = { connected: true, status: null, lcu: null, draft: null, live: null, staticVersion: 0 };

test('the /ws feed folds into the dashboard state', () => {
  let s = apply(empty, { type: 'lcu', state: { status: 'connected', phase: 'ChampSelect' } });
  assert.deepEqual(s.lcu, { status: 'connected', phase: 'ChampSelect' });
  s = apply(s, { type: 'live', state: { active: true, context: null, advice: null } });
  assert.equal(s.live?.active, true);
  s = apply(s, { type: 'live', state: { active: false } });
  assert.equal(s.live, null, 'a game that ended is gone');
  s = apply(s, { type: 'static', ready: true });
  assert.equal(s.staticVersion, 1);
  s = apply(s, { type: 'activity', activity: 'inGame' });
  assert.equal(s.status, null, 'no status yet: nothing to update');
  s = apply(s, { type: 'status', status: null });
  assert.equal(s.status, null);
});
