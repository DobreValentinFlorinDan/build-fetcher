// Source runs only (npm run dev:api, npm test): loads a worker's TypeScript entry (server/workers.ts) through tsx inside
// the worker thread. Node 22.18+ strips types natively and would otherwise load it without tsx, rejecting
// TypeScript-only syntax (parameter properties).
import { workerData } from 'node:worker_threads';
import { register } from 'tsx/esm/api';

register();
await import(workerData.entry);
