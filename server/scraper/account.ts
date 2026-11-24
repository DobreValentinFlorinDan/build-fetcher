// Your own account, for fetching your games (source "me"). The League client knows who you are, but its PUUID is not
// the one the Riot API takes (the API's is encrypted for each application), so the API's is looked up from your Riot
// ID. Your server comes from the League client (chat, the Riot client's region, the login data), else from Riot, else
// from Settings, where you can also set it by hand (that choice wins).
import type { RiotApi } from '../riot/client.ts';
import { accountHost, toPlatform, type Platform } from '../riot/routing.ts';

/** What the League client says about the signed-in account. */
export interface ClientAccount { gameName: string | null; tagLine: string | null; platform: string | null }

export type Resolved =
  | { ok: true; puuid: string; platform: Platform; riotId: string }
  | { ok: false; error: string };

/** The League client's own PUUID format (a UUID): never valid for the Riot API. Older versions stored it. */
export const isClientPuuid = (puuid: string) => /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(puuid);

export async function resolveAccount(riot: Pick<RiotApi, 'accountByRiotId' | 'accountRegion'>, a: ClientAccount,
  manualPlatform: string | null, signal?: AbortSignal): Promise<Resolved> {
  if (!a.gameName || !a.tagLine) return { ok: false, error: 'The League client did not give your Riot ID (Name#TAG) yet.' };
  const riotId = `${a.gameName}#${a.tagLine}`;
  const fromClient = toPlatform(manualPlatform) ?? toPlatform(a.platform);
  const account = await riot.accountByRiotId(a.gameName, a.tagLine, accountHost(fromClient), signal);
  if (!account?.puuid) return { ok: false, error: `Riot doesn't know the account ${riotId}.` };
  let platform = fromClient;
  if (!platform) {
    try { platform = toPlatform(await riot.accountRegion(account.puuid, signal)); } catch { platform = null; }
  }
  if (!platform) return { ok: false, error: `Could not tell which server ${riotId} plays on: choose it in Settings → Your account.` };
  return { ok: true, puuid: account.puuid, platform, riotId };
}
