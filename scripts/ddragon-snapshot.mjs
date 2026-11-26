// Writes the Data Dragon snapshot the tests load (fixtures/ddragon-snapshot.json), so the tests run offline and a
// new patch can't change their results. Only the fields bf_core::ddragon reads are kept: since 1.3.0 that includes
// the full item descriptions (their <stats> block and effects) and every champion's base stats.
//   node scripts/ddragon-snapshot.mjs            newest version
//   node scripts/ddragon-snapshot.mjs 16.19.1    a given version
// Behind a proxy on Node 22+, prefix NODE_USE_ENV_PROXY=1. Re-run it when the tests should move to a newer patch,
// then fix whatever the new items or runes break.
import fs from 'node:fs/promises';

const CDN = 'https://ddragon.leagueoflegends.com';
const CHAMPION_KEY = 'Irelia';
const OUT = 'fixtures/ddragon-snapshot.json';

const get = async (url) => {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${res.status} ${url}`);
  return res.json();
};
const toPatch = (v) => v.split('.').slice(0, 2).join('.');

const versions = await get(`${CDN}/api/versions.json`);
const version = process.argv[2] ?? versions[0];
if (!versions.includes(version)) throw new Error(`Unknown Data Dragon version ${version}`);
const previousPatch = versions.map(toPatch).find((p) => p !== toPatch(version)) ?? toPatch(version);
const base = `${CDN}/cdn/${version}/data/en_US`;
const [item, champion, runesReforged, summoner, champ] = await Promise.all(
  ['item.json', 'champion.json', 'runesReforged.json', 'summoner.json', `champion/${CHAMPION_KEY}.json`].map((f) => get(`${base}/${f}`)));

const pick = (o, keys) => Object.fromEntries(keys.filter((k) => o[k] !== undefined).map((k) => [k, o[k]]));
const mapValues = (o, f) => Object.fromEntries(Object.entries(o).map(([k, v]) => [k, f(v)]));

const snapshot = {
  $comment: `Trimmed Data Dragon ${version} for the tests; regenerate with node scripts/ddragon-snapshot.mjs`,
  version,
  previousPatch,
  raw: {
    item: {
      data: mapValues(item.data, (it) => ({
        ...pick(it, ['name', 'tags', 'into', 'from', 'specialRecipe', 'depth', 'inStore']),
        description: it.description,
        maps: pick(it.maps, ['11']),
        gold: pick(it.gold, ['base', 'total', 'purchasable']),
        ...(it.stats && Object.keys(it.stats).length ? { stats: it.stats } : {}),
      })),
    },
    champion: { data: mapValues(champion.data, (c) => ({ ...pick(c, ['key', 'id', 'name', 'tags', 'info']), stats: c.stats })) },
    runesReforged: runesReforged.map((tree) => ({
      ...pick(tree, ['id', 'key', 'name', 'icon']),
      slots: tree.slots.map((s) => ({ runes: s.runes.map((r) => ({ ...pick(r, ['id', 'key', 'name', 'icon']), shortDesc: '' })) })),
    })),
    summoner: { data: mapValues(summoner.data, (s) => ({ key: s.key, name: s.name, image: { full: s.image.full } })) },
    champ: { data: mapValues(champ.data, (c) => ({ spells: c.spells.map((s) => ({ id: s.id, name: s.name, image: { full: s.image.full } })) })) },
  },
};

await fs.writeFile(OUT, `${JSON.stringify(snapshot)}\n`);
console.log(`${OUT}: Data Dragon ${version} (previous patch ${previousPatch}), ${Object.keys(snapshot.raw.item.data).length} items, ${(JSON.stringify(snapshot).length / 1024).toFixed(0)} KB`);
