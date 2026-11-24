// A helper thread of a model build (0.15.0, started by model/prepareTeam.ts): it prepares the stored games it is sent
// (model/dataset.ts prepareGames: replaying each timeline is most of a build) and sends them back; the build puts them
// back in the order it read them. Each timeline comes as stored (compressed or not) and is decoded here.
//   in  workerData: { dataDir, versions, profileRows, startingItemWindowMs, dicts }; messages: { seq, rows, participants }
//   out { ready: true } | { seq, prepared } | { seq, error }
import { parentPort, workerData } from 'node:worker_threads';
import { setDataDir } from '../config.ts';
import { loadStaticData } from '../ddragon.ts';
import { prepareGames, type ParticipantLite } from './dataset.ts';
import { decodeTimelines, type RawGameRow } from './prepareTeam.ts';
import { ChampionProfiles, type ChampionTotals } from './profiles.ts';

const boot = workerData as {
  dataDir: string; versions: string[]; profileRows: { current: ChampionTotals[]; older: ChampionTotals[] }; startingItemWindowMs: number;
  dicts: [number, Uint8Array][];
};
const dicts = new Map(boot.dicts.map(([id, bytes]) => [id, Buffer.from(bytes)]));
setDataDir(boot.dataDir);
const data = await loadStaticData(boot.versions);
const profiles = new ChampionProfiles(boot.profileRows.current, boot.profileRows.older, data);

parentPort!.on('message', (msg: { seq: number; rows: RawGameRow[]; participants: ParticipantLite[] }) => {
  try {
    decodeTimelines(msg.rows, (id) => dicts.get(id));
    const prepared = prepareGames(msg.rows, msg.participants, data, profiles, { startingItemWindowMs: boot.startingItemWindowMs });
    parentPort!.postMessage({ seq: msg.seq, prepared });
  } catch (err) {
    parentPort!.postMessage({ seq: msg.seq, error: err instanceof Error ? err.message : String(err) });
  }
});
parentPort!.postMessage({ ready: true });
