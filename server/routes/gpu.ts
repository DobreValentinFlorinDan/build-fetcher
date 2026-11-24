import express from 'express';
import type { GpuState } from '../../shared/types.ts';
import { readAppConfig, updateAppConfig } from '../appConfig.ts';
import { clearGpuLog, gpuLog, gpuLogFile, readGpuLog } from '../gpu/log.ts';
import { gpuTesting, testGpuNow } from '../gpu/service.ts';

const state = (): GpuState => ({ useGpu: readAppConfig().useGpu === true, testing: gpuTesting(), log: readGpuLog(), logFile: gpuLogFile });

/**
 * /api/gpu: Settings → GPU (0.13.0). Use GPU on or off (read by the next evaluation), Test GPU, and its log.
 * A test waits while Evaluate builds runs (they would share the card's memory) and in champ select or a game (the card
 * is the game's).
 */
export function gpuRoutes(o: { evaluating: () => boolean; busy: () => boolean }) {
  const router = express.Router();
  router.get('/', (_req, res) => { res.json(state()); });
  router.put('/', (req, res) => {
    if (typeof req.body?.useGpu !== 'boolean') { res.status(400).json({ error: 'useGpu (boolean) is required' }); return; }
    updateAppConfig({ useGpu: req.body.useGpu || undefined });
    gpuLog(`Use GPU ${req.body.useGpu ? 'on: the next evaluation uses the GPU' : 'off: Evaluate builds runs on the processor'}.`);
    res.json(state());
  });
  router.post('/test', async (_req, res) => {
    if (o.evaluating()) { res.status(409).json({ error: 'Evaluate builds is running: test the GPU once it ends.' }); return; }
    if (o.busy()) { res.status(409).json({ error: 'Champ select or a game is on: test the GPU after it.' }); return; }
    const ok = await testGpuNow();
    res.json({ ...state(), ok });
  });
  router.delete('/log', (_req, res) => { clearGpuLog(); res.json(state()); });
  return router;
}
