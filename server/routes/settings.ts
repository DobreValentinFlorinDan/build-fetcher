import express from 'express';
import type { SettingsState } from '../../shared/types.ts';
import { readAppConfig, updateAppConfig } from '../appConfig.ts';
import { config } from '../config.ts';
import { PLATFORMS, toPlatform } from '../riot/routing.ts';
import type { ScraperController } from '../scraper/controller.ts';

const KEY_RE = /^RGAPI-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const mask = (k: string) => (k ? `${k.slice(0, 10)}…${k.slice(-4)}` : null);

function state(): SettingsState {
  const app = readAppConfig();
  return {
    riotApiKey: mask(config.riotApiKey),
    keySource: app.riotApiKey ? 'settings' : config.riotApiKey ? 'env' : 'none',
    keySavedAt: app.riotApiKey ? app.riotApiKeySavedAt ?? null : null,
    myPlatform: toPlatform(app.myPlatform) ?? null,
    platforms: Object.entries(PLATFORMS).map(([id, p]) => ({ id, label: p.label })),
    pauseFetchingInGame: app.pauseFetchingInGame !== false,
  };
}

/**
 * /api/settings: the Riot API key, and your server when it can't be detected. The installed app has no .env, so the key is entered in Settings
 * and stored in %APPDATA%/irelia-companion/config.json. A saved key wins over RIOT_API_KEY in .env;
 * saving an empty key falls back to .env.
 */
export function settingsRoutes(scraper: ScraperController) {
  const router = express.Router();
  router.get('/', (_req, res) => { res.json(state()); });
  router.put('/', (req, res) => {
    // Pause fetching during champ select and games ({"pauseFetchingInGame": false}: fetch through them).
    if (typeof req.body?.pauseFetchingInGame === 'boolean') {
      updateAppConfig({ pauseFetchingInGame: req.body.pauseFetchingInGame });
      scraper.setPauseInGame(req.body.pauseFetchingInGame);
      res.json(state());
      return;
    }
    // Your server, by hand ({"myPlatform": "EUW1"}; null: detect it again).
    if (req.body?.myPlatform !== undefined) {
      const p = req.body.myPlatform === null ? null : toPlatform(req.body.myPlatform);
      if (req.body.myPlatform !== null && !p) { res.status(400).json({ error: `Unknown server "${String(req.body.myPlatform)}"` }); return; }
      updateAppConfig({ myPlatform: p ?? undefined });
      scraper.retryMe();
      res.json(state());
      return;
    }
    if (typeof req.body?.riotApiKey !== 'string') { res.status(400).json({ error: 'riotApiKey (string) is required' }); return; }
    const key = req.body.riotApiKey.trim();
    if (key && !KEY_RE.test(key)) { res.status(400).json({ error: 'That does not look like a Riot API key (RGAPI-xxxxxxxx-...).' }); return; }
    updateAppConfig({ riotApiKey: key || undefined, riotApiKeySavedAt: key ? Date.now() : undefined });
    config.riotApiKey = key || process.env.RIOT_API_KEY || '';
    scraper.setApiKey(config.riotApiKey);
    scraper.retryMe();   // your own games need the key too
    res.json(state());
  });
  return router;
}
