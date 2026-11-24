// Patch memory (model/weights.ts): older patches count, fading per patch, less when Irelia or the item changed since
// (Data Dragon fingerprints), not at all past the window; off, only today's and the previous patch count, as before.
// The data check lets today's games overrule older ones that clearly disagree.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import type { Fingerprints } from '../ddragon.ts';
import { loadTestStaticData } from '../test/staticData.ts';
import type { PreparedGame } from './dataset.ts';
import type { Observation } from './estimate.ts';
import { consistent, MEMORY_PATCHES, patchAge, patchAger, patchOrder, patchWeigher, PREVIOUS_PATCH, WEIGHT_DEFAULTS } from './weights.ts';

const data = loadTestStaticData();
const pg = (patch: string, platform = 'KR') => ({ patch, platform, created: 0 }) as PreparedGame;
const versions = ['16.19.1', '16.18.1', '16.17.1', '16.16.1', '16.15.1', '16.14.1', '16.13.1', '16.12.1'];
const fp = data.fingerprints;
const same = (): Fingerprints => ({ items: new Map(fp.items), champion: fp.champion });

/** Data Dragon as of 16.19 with the fingerprints of the patches before it; `edit` changes one patch's. */
function withHistory(edit: (patch: string, f: Fingerprints) => void = () => {}) {
  const history = new Map<string, Fingerprints>();
  for (const v of versions.slice(1)) { const p = v.split('.').slice(0, 2).join('.'); const f = same(); edit(p, f); history.set(p, f); }
  return { ...data, versions, previous: { version: '16.18.1', ...history.get('16.18')! }, history };
}

test('patch ages count patches back on the version list, across seasons too', () => {
  const order = patchOrder({ version: '17.1.1', versions: ['17.1.1', '16.24.1', '16.23.1'] });
  assert.deepEqual(order, ['17.1', '16.24', '16.23']);
  assert.equal(patchAge(order, '16.23', '17.1'), 2);
  assert.equal(patchAge(order, '17.1', '17.1'), 0);
  assert.equal(patchAge(order, '16.20', '16.22'), 2, 'not on the list: the same season counts by number');
  assert.equal(patchAge(order, '15.20', '16.22'), null);
  const ageOf = patchAger({ ...data, versions }, () => undefined);
  assert.deepEqual(['16.19', '16.18', '16.16', '15.1'].map((p) => ageOf(pg(p))), [0, 1, 3, Number.POSITIVE_INFINITY]);
});

test("without patch memory: today's and the previous patch only, exactly as before", () => {
  const w = patchWeigher(withHistory(), () => undefined, WEIGHT_DEFAULTS);
  assert.equal(w(pg('16.19')), 1);
  assert.equal(w(pg('16.18'), [3153]), PREVIOUS_PATCH.unchanged);
  assert.equal(w(pg('16.17'), [3153]), 0);
  assert.equal(w(pg('16.14')), 0);
});

test('with patch memory: older patches fade per patch, count less once something involved changed, and stop at the window', () => {
  const memory = { decay: 0.75, patches: MEMORY_PATCHES };
  // Trinity Force (3078) was changed in 16.17: games before it played a different item.
  const d = withHistory((patch, f) => { if (patch === '16.16' || patch === '16.15') f.items.set(3078, 'before the change'); });
  const w = patchWeigher(d, () => undefined, { ...WEIGHT_DEFAULTS, memory });
  assert.equal(w(pg('16.19')), 1);
  assert.equal(w(pg('16.18'), [3153]), PREVIOUS_PATCH.unchanged, 'the previous patch as without memory');
  assert.equal(w(pg('16.17'), [3153]), 0.75 * PREVIOUS_PATCH.unchanged);
  assert.equal(w(pg('16.16'), [3153]), 0.75 ** 2 * PREVIOUS_PATCH.unchanged, 'BotRK did not change');
  assert.equal(w(pg('16.16'), [3078]), 0.75 ** 2 * PREVIOUS_PATCH.changed, 'Trinity Force did');
  assert.equal(w(pg('16.14'), [3153]), 0.75 ** 4 * PREVIOUS_PATCH.unchanged, 'the oldest patch in the window');
  assert.equal(w(pg('16.13'), [3153]), 0, 'outside the window');

  // An Irelia change demotes every older game; a patch without Data Dragon files is unknown.
  const ireliaChanged = patchWeigher(withHistory((patch, f) => { if (patch === '16.16') f.champion = 'old kit'; }), () => undefined, { ...WEIGHT_DEFAULTS, memory });
  assert.equal(ireliaChanged(pg('16.16'), [3153]), 0.75 ** 2 * PREVIOUS_PATCH.changed);
  assert.equal(ireliaChanged(pg('16.17'), [3153]), 0.75 * PREVIOUS_PATCH.unchanged);
  const missing = withHistory();
  missing.history.delete('16.16');
  assert.equal(patchWeigher(missing, () => undefined, { ...WEIGHT_DEFAULTS, memory })(pg('16.16'), [3153]), 0.75 ** 2 * PREVIOUS_PATCH.unknown);
  // A hand-entered patch weight still wins, and fades with age too.
  const manual = patchWeigher(d, () => undefined, { ...WEIGHT_DEFAULTS, memory, patchWeights: { '16.16': 0.5 } });
  assert.equal(manual(pg('16.16'), [3078]), 0.5 * 0.75 ** 2);
});

test("the data check: older games of an option today's games clearly contradict count less; agreeing ones keep their weight", () => {
  const obs: Observation[] = [];
  const add = (option: string, age: number, n: number, wins: number) => {
    for (let i = 0; i < n; i++) obs.push({ option, age, wpa: (i < wins ? 1 : 0) - 0.5, win: i < wins ? 1 : 0, w: 1, lane: 'TOP', opponent: null, pilot: `p${i}`, z: [] });
  };
  add('3078', 0, 120, 40);   // nerfed (hidden): 33 % today...
  add('3078', 2, 400, 240);  // ...60 % on older patches
  add('3153', 0, 120, 66);   // 55 % today, 56 % before: agrees
  add('3153', 2, 400, 224);
  add('6333', 0, 10, 2);     // too few games today to judge
  add('6333', 2, 400, 280);
  const out = consistent(obs);
  const weight = (option: string, age: number) => out.find((o) => o.option === option && o.age === age)!.w;
  assert.ok(weight('3078', 2) < 0.2, `contradicted: ${weight('3078', 2)}`);
  assert.equal(weight('3078', 0), 1, "today's games keep their weight");
  assert.equal(weight('3153', 2), 1);
  assert.equal(weight('6333', 2), 1);
  const agreeing = obs.filter((o) => o.option === '3153');
  assert.equal(consistent(agreeing), agreeing, 'nothing contradicted: the same observations back');
});
