// Your own account (scraper/account.ts): the League client's PUUID is not the Riot API's, so the API's is looked up
// from the Riot ID; the server comes from the client (a platform id or a region name), Riot, or Settings.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { toPlatform } from './riot/routing.ts';
import { isClientPuuid, resolveAccount } from './scraper/account.ts';
import { FakeRiot } from './test/fixtures.ts';

const CLIENT_PUUID = '5f3e2a1b-7c9d-4e8f-a1b2-c3d4e5f60718';
const API_PUUID = 'Xq3-ApiPuuid_78chars-encrypted-for-this-key';

function riot() {
  const r = new FakeRiot();
  r.accounts.set('irelia main#euw', API_PUUID);
  return r;
}

test("the Riot API PUUID comes from the Riot ID, never the client's, and the server from what the client says", async () => {
  const r = riot();
  const a = await resolveAccount(r, { gameName: 'Irelia Main', tagLine: 'EUW', platform: 'EUW1' }, null);
  assert.deepEqual(a, { ok: true, puuid: API_PUUID, platform: 'EUW1', riotId: 'Irelia Main#EUW' });
  assert.deepEqual(r.calls.find((c) => c.method === 'accountByRiotId')?.args, ['Irelia Main', 'EUW', 'europe.api.riotgames.com']);
  assert.ok(!r.calls.some((c) => c.args.includes(CLIENT_PUUID)));
  // The Riot client's region names work too.
  const eune = await resolveAccount(r, { gameName: 'Irelia Main', tagLine: 'EUW', platform: 'EUNE' }, null);
  assert.equal(eune.ok && eune.platform, 'EUN1');
});

test("without a server from the client, Riot's answer; then Settings; and a choice in Settings wins", async () => {
  const r = riot();
  const none = await resolveAccount(r, { gameName: 'Irelia Main', tagLine: 'EUW', platform: null }, null);
  assert.equal(none.ok, false);
  assert.match(!none.ok ? none.error : '', /Could not tell which server Irelia Main#EUW plays on: choose it in Settings/);
  r.regions.set(API_PUUID, 'euw1');
  const fromRiot = await resolveAccount(r, { gameName: 'Irelia Main', tagLine: 'EUW', platform: null }, null);
  assert.equal(fromRiot.ok && fromRiot.platform, 'EUW1');
  const manual = await resolveAccount(r, { gameName: 'Irelia Main', tagLine: 'EUW', platform: 'EUW1' }, 'EUN1');
  assert.equal(manual.ok && manual.platform, 'EUN1');
});

test('an unknown Riot ID, or none yet, says so', async () => {
  const r = riot();
  const unknown = await resolveAccount(r, { gameName: 'Nobody', tagLine: 'X', platform: 'KR' }, null);
  assert.match(!unknown.ok ? unknown.error : '', /Riot doesn't know the account Nobody#X/);
  const early = await resolveAccount(r, { gameName: null, tagLine: null, platform: 'KR' }, null);
  assert.match(!early.ok ? early.error : '', /did not give your Riot ID/);
});

test('server names and stored client PUUIDs are recognised', () => {
  assert.deepEqual(['EUW1', 'euw1', 'EUW', 'EUNE', 'LAN', 'oce', 'KR', 'PBE', ''].map(toPlatform), ['EUW1', 'EUW1', 'EUW1', 'EUN1', 'LA1', 'OC1', 'KR', null, null]);
  assert.equal(isClientPuuid(CLIENT_PUUID), true);
  assert.equal(isClientPuuid(API_PUUID), false);
});
