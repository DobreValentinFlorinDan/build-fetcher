import { useEffect, useState } from 'react';
import type {
  AutoEvalStatus, BuildMode, ContextChip as ContextChipType, DraftModelInfo, DraftResponse, EvaluationPlan, JobsStatus, ModelStatus, PickStats, ProgressInfo,
  SystemInfo,
} from '../../../shared/types.ts';
import { api, fmtBytes, img, pts, useEstimates, useSystem, type Meta } from '../api.ts';
import { desktop } from '../desktop.ts';
import { Card, Empty, ItemIcon, SwitchRow } from './common.tsx';
import { clock, fmtAbout, leftAt, leftText, ProgressBar, Takes, useNow } from './Progress.tsx';
import { SHARDS } from './Runes.tsx';

// Stage B on the draft dashboard: the Draft-adjusted / Lane baseline switch, what the draft changed and why, how much
// evidence stands behind each pick, and which regions feed it. Every lift is an estimate (win-probability-added).

const ITEM_KEYS = new Set(['core1', 'core2', 'core3', 'boots', 'item4', 'item5', 'item6']);

/** Platform ids as players know them. */
export const REGION_NAME: Record<string, string> = {
  KR: 'KR', EUW1: 'EUW', EUN1: 'EUNE', NA1: 'NA', JP1: 'JP', BR1: 'BR', LA1: 'LAN', LA2: 'LAS', OC1: 'OCE', TR1: 'TR', RU: 'RU',
  ME1: 'ME', SG2: 'SG', TW2: 'TW', VN2: 'VN',
};

const runeById = (meta: Meta, id: number) =>
  meta.runes.flatMap((t) => [{ id: t.id, icon: t.icon, name: t.name }, ...t.slots.flatMap((s) => s.runes)]).find((r) => r.id === id);

/** A pick's value in words: item, rune, tree, shard, summoner spells or skills. */
export function pickLabel(meta: Meta, p: { key: string; value: string }): string {
  if (!p.value) return '—';
  if (ITEM_KEYS.has(p.key)) return meta.items[Number(p.value)]?.name ?? `#${p.value}`;
  if (p.key === 'start') {
    const ids = p.value.split(',').map(Number);
    return [...new Set(ids)].map((id) => { const n = ids.filter((x) => x === id).length; return `${meta.items[id]?.name ?? `#${id}`}${n > 1 ? ` ×${n}` : ''}`; }).join(' + ');
  }
  if (p.key.startsWith('shard')) return SHARDS[Number(p.value)]?.name ?? p.value;
  if (p.key === 'summoners') return p.value.split(',').map((id) => meta.summoners[Number(id)]?.name ?? id).join(' + ');
  if (p.key === 'maxFirst') return `Max ${p.value} first`;
  if (p.key === 'early') return p.value.split('').join(' › ');
  return runeById(meta, Number(p.value))?.name ?? p.value;
}

/** The pick's own icon(s): item, rune or tree, shard, both summoner spells, starting items, or skill keys. */
export function PickIcon({ meta, pick, size = 32 }: { meta: Meta; pick: { key: string; value: string }; size?: number }) {
  const box = { width: size, height: size };
  if (!pick.value) return <span className="shrink-0 rounded bg-ink-900" style={box} />;
  if (ITEM_KEYS.has(pick.key)) return <ItemIcon meta={meta} id={Number(pick.value)} size={size} />;
  if (pick.key === 'start') {
    const ids = [...new Set(pick.value.split(',').map(Number))];
    return <span className="flex shrink-0 gap-0.5">{ids.map((id) => <ItemIcon key={id} meta={meta} id={id} size={size} />)}</span>;
  }
  if (pick.key === 'summoners') {
    return (
      <span className="flex shrink-0 gap-0.5">
        {pick.value.split(',').map(Number).filter((id) => meta.summoners[id]).map((id) => (
          <img key={id} src={img.spell(meta.version, meta.summoners[id].image)} alt="" className="rounded" style={box} />
        ))}
      </span>
    );
  }
  if (pick.key.startsWith('shard') && SHARDS[Number(pick.value)]) {
    return <img src={img.rune(`perk-images/StatMods/${SHARDS[Number(pick.value)].icon}`)} alt="" className="shrink-0 rounded-full bg-black p-1" style={box} />;
  }
  if (pick.key === 'maxFirst' || pick.key === 'early') {
    const keys = pick.value.split('');
    return (
      <span className="flex shrink-0 gap-0.5">
        {keys.map((k, i) => <span key={i} className={`skill-key key-${k} static inline-flex items-center justify-center rounded text-[12px]`} style={{ width: size * 0.75, height: size }}>{k}</span>)}
      </span>
    );
  }
  const rune = runeById(meta, Number(pick.value));
  return rune ? <img src={img.rune(rune.icon)} alt="" className="shrink-0 rounded-full bg-black" style={box} /> : <span className="shrink-0" style={box} />;
}

const fmtCount = (n: number) => Math.round(n).toLocaleString('en-US');

/** W̄ at or beyond this far from 50 % is flagged: the item was mostly bought when ahead / behind. */
const START_FLAG = 0.08;

/** "W̄ 62 % · mostly bought when ahead" (a flag only when well away from 50 %); null when the win chance is unknown. */
export function startWinNote(w: number | null | undefined): { text: string; flag: string | null } | null {
  if (w == null || !Number.isFinite(w)) return null;
  const flag = w >= 0.5 + START_FLAG ? 'mostly bought when ahead' : w <= 0.5 - START_FLAG ? 'mostly bought when behind' : null;
  return { text: `W̄ ${(w * 100).toFixed(0)} %`, flag };
}

/** How a pick stands, in one compact line: "+2.8 ± 8.5 pts · 120 games · 52 pilots · 31 like this draft". Estimates. */
export function Evidence({ s, vsBaseline }: { s: PickStats; vsBaseline?: { name: string; lift: number } }) {
  const half = (s.high - s.low) / 2;
  const delta = vsBaseline ? s.lift - vsBaseline.lift : null;
  const start = startWinNote(s.startWin);
  const title = `Win-probability-added in games like this one: how much more often than expected they were won (an estimate). Items are judged against the game state when they were bought; runes, summoners, the start and skills against the overall win rate, or with Runes against the draft (Data tab) against what the draft predicted.\n`
    + `This pick: ${pts(s.lift)} (95% interval ${pts(s.low)} to ${pts(s.high)})`
    + (vsBaseline ? `\nThe lane build's ${vsBaseline.name}: ${pts(vsBaseline.lift)}` : '')
    + `\n${fmtCount(s.games)} games by ${s.pilots} pilots in this lane; their similarity to this draft adds up to ${fmtCount(s.nEff)} games.`
    + (start ? `\nW̄: the average win chance (game-state model) just before the purchase, over the same weighted games. Far from 50 % means the item was mostly bought by a team that was ahead or behind, which its score already accounts for.` : '');
  return (
    <span className="text-[11px] text-ash-400" title={title}>
      {delta !== null
        ? Math.abs(delta) < 0.005
          ? <span className="text-ash-200">About the same as the lane pick</span>
          : <><b className={delta > 0 ? 'text-gold-400' : 'text-ash-200'}>{pts(delta)}</b> vs the lane pick</>
        : <><b className={s.low > 0 ? 'text-gold-400' : 'text-ash-200'}>{pts(s.lift)}</b> <span className="text-ash-600">± {(half * 100).toFixed(1)}</span></>}
      {' · '}{fmtCount(s.games)} games · {s.pilots} pilots · {fmtCount(s.nEff)} like this draft
      {s.matchupGames > 0 && <> · {s.matchupGames} in this matchup</>}
      {start && <> · <span title="Average win chance just before the purchase">{start.text}</span>{start.flag && <> <span className="text-gold-400">({start.flag})</span></>}</>}
    </span>
  );
}

const GATE_TEXT: Record<DraftModelInfo['gate'], string> = {
  'not evaluated': 'Not evaluated yet: the lane build is the default until it is. You can still choose Draft-adjusted.',
  passed: 'The last evaluation found draft-adjusted builds clearly better than the lane builds, so they are the default.',
  failed: 'The last evaluation found no clear gain over the lane builds, so the lane build stays the default. You can still choose Draft-adjusted.',
};
/** What "clearly better" means (model/evaluate.ts): shown under the evaluation status. */
const GATE_RULE = 'On your stored games, each held out in turn (no player in both training and test), games that followed the draft-adjusted picks must beat the game-state prediction by more than games that followed the lane picks, with 95% confidence, on at least 200 games, and predict results at least as well.';

/** What the draft model is doing, in a few words; null once it is ready. */
export function modelState(status: ModelStatus | null): string | null {
  if (!status || status.state === 'loading') return 'Loading the draft model…';
  if (status.state === 'building') return 'Updating the draft model with new games…';
  if (status.state === 'failed') return `The draft model could not be built${status.error ? `: ${status.error}` : ''}.`;
  return null;
}

/** The Draft-adjusted / Lane baseline switch (the choice is saved and the overlay follows it). */
export function ModeToggle({ mode, status, available, onMode }: {
  mode: BuildMode; status: ModelStatus | null; available: boolean; onMode: (m: BuildMode) => void;
}) {
  const model = status?.info ?? null;
  return (
    <div className="flex flex-wrap items-center gap-2">
      <div className="lane-switch" role="group" aria-label="Build">
        {(['draft', 'baseline'] as const).map((m) => (
          <button key={m} type="button" aria-pressed={mode === m} className={mode === m ? 'active' : ''} onClick={() => onMode(m)}
            disabled={m === 'draft' && !available}>
            {m === 'draft' ? 'Draft-adjusted' : 'Lane baseline'}
          </button>
        ))}
      </div>
      <span className="text-[11px] text-ash-600" title={model ? GATE_RULE : undefined}>
        {modelState(status) ?? (!available || !model ? 'Draft-adjusted builds need more stored games with timelines.' : GATE_TEXT[model.gate])}
      </span>
    </div>
  );
}

/** "21:05", with the day when it isn't today. */
const at = (ms: number) => {
  const d = new Date(ms);
  const time = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  return d.toDateString() === new Date().toDateString() ? time : `${d.toLocaleDateString([], { weekday: 'short' })} ${time}`;
};

/** Evaluate builds' two kinds of run (0.16.0, server/model/evalBase.ts). */
const PATH_NAME: Record<'full' | 'update', string> = { update: 'quick update', full: 'full evaluation' };
const capital = (s: string) => `${s.charAt(0).toUpperCase()}${s.slice(1)}`;
const hours = (h: number) => (h === 1 ? 'hour' : `${h} hours`);

/**
 * What Evaluate builds runs now (server/model/evalBase.ts planEvaluation, 0.16.0): "Next: quick update — 1,240 new games
 * since the last evaluation (full evaluation of 21 Oct)."
 */
function planLine(p: EvaluationPlan): string {
  if (p.path === 'none') return `${capital(p.reason)}. Full evaluation still evaluates every game again.`;
  return `Next: ${PATH_NAME[p.path]} — ${p.reason}.`;
}

/** When Evaluate builds runs by itself (server/autoEval.ts), and whether one is due. */
function autoLine(a: AutoEvalStatus | undefined, running: boolean, evaluated: boolean): string {
  if (!a?.on) return 'Follow the evaluation is off, so Evaluate builds runs only when you click it.';
  const rule = `With Follow the evaluation on, Evaluate builds also runs by itself, never during champ select or a game: a quick update after ${fmtCount(a.updateGames)} new games, at most every ${hours(a.updateSpacingHours)}, and a full evaluation after a new patch or once a quick update is no longer enough, at most every ${hours(a.spacingHours)}.`;
  if (running || a.path === 'none') return rule;
  // A quick update counts the games since the last evaluation; a full one, those since the last full one.
  const quick = a.path === 'update';
  const count = quick ? a.newGames : a.sinceFull;
  const since = quick || a.sinceFull === a.newGames ? 'new games since the last evaluation' : 'games since the last full evaluation';
  const why = !evaluated && count >= a.needed ? 'not evaluated yet'
    : a.newPatch ? `new patch ${a.newPatch}` : count >= a.needed ? `${fmtCount(count)} ${since}` : null;
  if (!why) return `${rule} ${fmtCount(count)} ${since}: a ${quick ? 'quick update' : 'full evaluation'} is due at ${fmtCount(a.needed)}.`;
  const when = a.waiting === 'spacing' && a.notBefore ? `at ${at(a.notBefore)}`
    : a.waiting === 'quiet' && a.notBefore ? `at ${at(a.notBefore)} unless champ select or a game starts first`
      : a.waiting === 'quiet' ? `once no champ select or game has been on for ${a.quietMinutes} minutes`
        : a.waiting === 'busy' ? 'once the data folder is ready' : 'now';
  return `${rule} A ${quick ? 'quick update' : 'full evaluation'} is due (${why}): it runs ${when}.`;
}


/**
 * This computer, for Evaluate builds (0.11.0): its processor, its threads, its memory, and how many threads an
 * evaluation started now would use (every core the available memory holds, a reserve left free).
 */
function computerLine(s: SystemInfo): string {
  const e = s.evaluation;
  const cpu = `${s.processor ?? 'Processor not reported'}, ${s.cores} thread${s.cores === 1 ? '' : 's'}`;
  const memory = `${fmtBytes(s.memory.total)} memory, ${fmtBytes(s.memory.available)} available now`;
  const per = `about ${fmtBytes(e.perThread)} (an estimate${e.measured ? ' from the last evaluation here' : ''})`;
  const uses = s.cores <= 1 ? 'Evaluate builds runs on its one thread'
    : e.threads <= 1 ? `Evaluate builds would run on one thread: each more would take ${per}, more than is free now`
      : `Evaluate builds would run on ${e.threads} threads${e.limit === 'memory' ? ` (the free memory holds ${e.threads} of ${s.cores})` : e.limit === 'heap' ? ` (the app's 4 GB of script memory, shared by all its threads, holds ${e.threads} of ${s.cores})` : ''}, each beyond the first taking ${per}`;
  return `This computer: ${cpu}; ${memory}. ${uses}, with ${fmtBytes(s.memory.reserve)} always left free so nothing spills to disk.`;
}

/**
 * Data tab: the draft model's state and npm run eval:builds in the app. The evaluation's progress and result come from
 * the server (Realtime.jobs), so they stay when you switch tabs; Cancel stops it. Evaluate builds runs what the plan
 * says (0.16.0: a quick update or a full evaluation, shown under it); Full evaluation evaluates every game again.
 */
export function ModelCard({ status, jobs }: { status: ModelStatus | null; jobs: JobsStatus | null }) {
  const model = status?.info ?? null;
  const job = jobs?.running.find((j) => j.job === 'evaluate') ?? null;
  const last = jobs?.last.evaluate ?? null;
  const plan = jobs?.plan ?? null;
  // The request itself failing (the app restarting, say); the job's own errors arrive in `last`.
  const [requestError, setRequestError] = useState<string | null>(null);
  const [cancelling, setCancelling] = useState(false);
  // How long Evaluate builds would take now: asked again after each evaluation and as the games grow.
  const estimates = useEstimates(`${last?.finishedAt ?? 0}|${model?.games ?? 0}`);
  const system = useSystem(`${last?.finishedAt ?? 0}|${model?.games ?? 0}`);
  useEffect(() => { if (!job) setCancelling(false); }, [job]);
  const run = (path: 'auto' | 'full') => {
    setRequestError(null);
    api.evaluate(path).catch((e) => setRequestError(e instanceof Error ? e.message : String(e)));
  };
  // What Evaluate builds would take: a quick update's own estimate, or a full evaluation's.
  const next = plan?.path === 'update' ? estimates['evaluate-update'] : plan?.path === 'none' ? null : estimates.evaluate;
  const fullTakes = estimates.evaluate !== null && estimates.evaluate !== undefined ? ` Takes ${fmtAbout(estimates.evaluate)} (estimate).` : '';
  const running = job?.path ? capital(PATH_NAME[job.path]) : 'Evaluation';
  const cancel = () => { setCancelling(true); void api.cancelJob('evaluate').catch(() => setCancelling(false)); };
  const extraError = !job && requestError && requestError !== 'Cancelled' && requestError !== last?.error ? requestError : null;
  return (
    <Card title="Draft model" aside={<span className={`pill ${status?.state === 'ready' ? '' : 'live'}`}>{status?.state === 'ready' ? 'Ready' : status?.state === 'failed' ? 'Failed' : 'Updating'}</span>}>
      {status?.state === 'building' && status.progress
        ? <ProgressBar title="Updating the draft model" progress={status.progress} label={modelState(status)} />
        : modelState(status) && <p className="text-ash-400">{modelState(status)}</p>}
      {status?.state === 'ready' && status.error && <div className="error">The last update failed ({status.error}): the previous model is still in use.</div>}
      {model && (
        <dl className="stat-grid">
          <div><dt>Games</dt><dd>{fmtCount(model.games)}</dd></div>
          <div><dt>State model</dt><dd>{model.stateModel ? `${fmtCount(model.stateModel.rows)} moments` : '—'}</dd></div>
          <div className="col-span-2"><dt>Evaluation</dt><dd className="!text-[13px] !font-normal text-ash-200">
            {GATE_TEXT[model.gate]}{model.evaluatedAt ? ` (${new Date(model.evaluatedAt).toLocaleDateString()})` : ''}</dd></div>
        </dl>
      )}
      {model && <p className="text-[11px] text-ash-600">{GATE_RULE}</p>}
      {model && (
        <p className="mt-1 text-[11px] text-ash-600">
          The draft model is rebuilt once new games reach 1 % of it (after half an hour at the soonest, six hours at the latest), at once for your own games, never in queue, champ select or a game, nor while Evaluate builds runs.
          {' '}Its bandwidth ({model.bandwidth}) and prior ({fmtCount(model.k)} games) {model.evaluatedAt ? 'come from the last evaluation' : 'are the defaults until the first evaluation'}.
          {model.tuning && model.tuning.gate !== 'not evaluated' && (model.tuning.gate === 'passed'
            ? ` Fine-tuning passed and is used: pilot standing ×${model.tuning.pilotStrength}, item prior ${model.tuning.itemK !== null ? `${fmtCount(model.tuning.itemK)} games` : 'as the rest'}, ${model.tuning.recencyDays !== null ? `newer games of a patch count more (half every ${model.tuning.recencyDays} days)` : 'no fade within a patch'}.`
            : ' Fine-tuning didn\'t beat it yet: today\'s settings stay.')}
        </p>
      )}
      {model && <ModelOptions memory={model.memory} scoring={model.scoring} swaps={model.swaps} building={status?.state === 'building' ? status.progress ?? null : undefined} />}
      {job ? (
        <div className="mt-2 space-y-1.5">
          <ProgressBar title="Evaluation progress"
            progress={job.progress ?? { fraction: null, etaMs: null, startedAt: job.startedAt, at: job.startedAt }}
            label={<>{job.auto ? <>Automatic {job.path ? PATH_NAME[job.path] : 'evaluation'} ({job.auto}) · </> : job.path && <>{running} · </>}{job.label ?? 'Starting…'}</>}
            aside={job.step && job.steps ? `step ${job.step} of ${job.steps}${job.threads ? ` · ${job.threads} thread${job.threads === 1 ? '' : 's'}` : ''}` : undefined} />
          <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
            <button type="button" className="ghost" onClick={cancel} disabled={cancelling}
              title={`Stops the ${job.path ? PATH_NAME[job.path] : 'evaluation'}. Nothing changes: the last finished evaluation still decides the default build.${job.auto ? ` The app tries again ${hours(job.path === 'update' ? jobs?.auto?.updateSpacingHours ?? 1 : jobs?.auto?.spacingHours ?? 6)} after this one started at the earliest.` : ''}`}>
              {cancelling ? 'Cancelling…' : 'Cancel'}
            </button>
            {job.path === 'update' && (
              <button type="button" className="ghost text-[11px]" onClick={() => run('full')} disabled={cancelling}
                title={`Stops this quick update and evaluates every stored game again, choosing the bandwidth, the prior and the other settings anew.${fullTakes}`}>
                Full evaluation instead
              </button>
            )}
          </div>
        </div>
      ) : (
        <div className="mt-2 flex flex-wrap items-baseline gap-x-3 gap-y-1">
          <button type="button" className="ghost" onClick={() => run('auto')} disabled={!model || plan?.path === 'none'}
            title="Tests the draft-adjusted builds against the lane baseline on your stored games, with no player in both training and test. A quick update while the last full evaluation still describes your games: it keeps that evaluation's settings and results and adds the new games' own. Else a full evaluation. No API calls; runs in its own thread, fetching carries on.">
            Evaluate builds
          </button>
          <Takes ms={next} />
          <button type="button" className="ghost text-[11px]" onClick={() => run('full')} disabled={!model}
            title={`Evaluates every stored game again and chooses the bandwidth, the prior and the other settings anew, as Evaluate builds does after a new patch. Its time grows faster than the stored games.${fullTakes}`}>
            Full evaluation
          </button>
        </div>
      )}
      {model && !job && plan && <p className="mt-1 text-[11px] text-ash-400">{planLine(plan)}</p>}
      {system && <p className="mt-1 text-[11px] text-ash-600">{computerLine(system)}{job ? ' Champ select or a game stops the other threads, so their memory goes back to the game; this evaluation then finishes on one.' : ''}</p>}
      {model && <p className="mt-1 text-[11px] text-ash-600">{autoLine(jobs?.auto, !!job, !!model.evaluatedAt)}</p>}
      {extraError && <div className="error">{extraError}</div>}
      {!job && last?.cancelled && (
        <p className="mt-1 text-[11px] text-ash-600">
          {last.paused
            ? `The automatic evaluation stopped after ${clock(last.finishedAt - last.startedAt)} because champ select or a game began: nothing changed. It starts over once neither has been on for ${jobs?.auto?.quietMinutes ?? 10} minutes.`
            : `Evaluation cancelled after ${clock(last.finishedAt - last.startedAt)}: nothing changed.`}
        </p>
      )}
      {!job && last?.error && <div className="error">The {last.auto ? 'automatic ' : ''}evaluation failed after {clock(last.finishedAt - last.startedAt)}: {last.error}</div>}
      {!job && last?.text && (
        <details className="mt-2" open>
          <summary className="cursor-pointer text-[11px] text-ash-400">
            {last.path ? `${capital(PATH_NAME[last.path])} report` : 'Evaluation report'} ({at(last.finishedAt)}{last.auto ? (last.reason ? ', automatic' : `, automatic: ${last.auto}`) : ''}, took {clock(last.finishedAt - last.startedAt)}{last.threads ? ` on ${last.threads} thread${last.threads === 1 ? '' : 's'}` : ''}){last.reason ? `: ${last.reason}` : ''}{last.file ? ` · saved as ${last.file}` : ''}
          </summary>
          <textarea readOnly value={last.text} rows={12} className="audit-text mt-1 w-full font-mono text-[11px]" onFocus={(e) => e.currentTarget.select()} />
          {desktop && last.file && <button className="ghost mt-1 text-[11px]" onClick={() => void desktop!.openFolder(last.file!.replace(/[\\/][^\\/]+$/, ''))}>Open folder</button>}
        </details>
      )}
    </Card>
  );
}

const MEMORY_VERDICT: Record<DraftModelInfo['memory']['gate'], string> = {
  'not evaluated': 'Not evaluated yet.',
  passed: "The last evaluation found it better than today's and the previous patch alone.",
  failed: "The last evaluation found no clear gain over today's and the previous patch alone.",
};

const SCORING_VERDICT: Record<DraftModelInfo['scoring']['gate'], string> = {
  'not evaluated': 'Not evaluated yet.',
  passed: 'The last evaluation found it better than judging them against the overall win rate.',
  failed: 'The last evaluation found no clear gain over judging them against the overall win rate.',
};

const SWAPS_VERDICT: Record<NonNullable<DraftModelInfo['swaps']>['gate'], string> = {
  'not evaluated': 'Not evaluated yet.',
  passed: 'The last evaluation found following the advice better than keeping every item.',
  failed: 'The last evaluation found no clear gain over keeping every item.',
};

/**
 * The model's options the evaluation can decide, as switches: patch memory (model/weights.ts: whether games of patches
 * before the previous one count), runes and summoners against the draft (model/expected.ts: what pre-game choices are
 * judged against), late-game swaps (docs/SWAPS.md: the Items view's swap advice in place of the Sixth Item, the model
 * built the same), and whether the evaluation decides them (moving any switch makes the choices yours). The first two
 * off is exactly the 0.8.x model; a change to either rebuilds the model, nothing is deleted.
 */
function ModelOptions({ memory, scoring, swaps, building }: {
  memory: DraftModelInfo['memory']; scoring: DraftModelInfo['scoring']; swaps: DraftModelInfo['swaps'];
  /** While the model rebuilds: its progress (null: no estimate yet); undefined when it isn't rebuilding. */
  building: ProgressInfo | null | undefined;
}) {
  const now = useNow(building !== undefined);
  // What was just asked for, shown until the model's status says so (it comes back over /ws).
  const [pending, setPending] = useState<{ memory?: boolean; scoring?: boolean; swaps?: boolean; auto?: boolean } | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => { setPending(null); }, [memory.choice, memory.active, scoring.choice, scoring.active, swaps?.choice, swaps?.active]);
  useEffect(() => {
    if (!pending) return;
    const t = setTimeout(() => setPending(null), 4000);
    return () => clearTimeout(t);
  }, [pending]);
  const failed = (e: unknown) => { setPending(null); setError(e instanceof Error ? e.message : String(e)); };
  const sendMemory = (choice: 'on' | 'off' | null, shown: { memory?: boolean; auto?: boolean }) => {
    setError(null); setPending(shown);
    api.setMemory(choice).catch(failed);
  };
  const sendScoring = (on: boolean) => {
    setError(null); setPending({ scoring: on, auto: false });
    api.setScoring(on ? 'on' : 'off').catch(failed);
  };
  const sendSwaps = (on: boolean) => {
    setError(null); setPending({ swaps: on, auto: false });
    api.setSwaps(on ? 'on' : 'off').catch(failed);
  };
  const memoryOn = pending?.memory ?? memory.active;
  const scoringOn = pending?.scoring ?? scoring.active;
  const swapsOn = pending?.swaps ?? swaps?.active ?? false;
  const auto = pending?.auto ?? memory.choice === 'auto';
  const fade = memory.decay !== null ? `each patch back at ${Math.round(memory.decay * 100)}% of the next` : 'fading with age';
  // A switch shows what is chosen at once; the model takes it with a rebuild (35-45 s at 54,000 games, after one already
  // running), never during champ select or a game.
  const catchingUp = (on: boolean, inModel: boolean) => on === inModel ? null
    : building !== undefined
      ? ` Updating the model…${building && leftAt(building, now) !== null ? ` (${leftText(building, now)}, an estimate)` : ''}`
      : ' The model takes this at its next update, never during champ select or a game.';
  return (
    <div className="mt-3 border-t border-ink-600 pt-2">
      <SwitchRow label="Patch memory" checked={memoryOn} disabled={pending !== null}
        onChange={(on) => sendMemory(on ? 'on' : 'off', { memory: on, auto: false })}
        hint={<>
          {memoryOn
            ? `Learns from games up to ${memory.patches - 1} patches back too, ${fade}, and less where Irelia or the item changed since; today's games overrule older ones that disagree.${memory.inModel ? ` ${fmtCount(memory.olderGames)} games from before the previous patch.` : ''}`
            : "Only today's and the previous patch's games count."}
          {' '}{MEMORY_VERDICT[memory.gate]}
          {catchingUp(memoryOn, memory.inModel)}
        </>} />
      <SwitchRow label="Runes against the draft" checked={scoringOn} disabled={pending !== null}
        onChange={sendScoring}
        hint={<>
          {scoringOn
            ? "Runes, summoners, the start and the skill order are judged by how much more often than expected their games were won, given the draft, the lane opponent, the patch and the player's rank. A choice isn't blamed for the hard matchups it's taken into, or credited for the strong players who favour it."
            : 'Runes, summoners, the start and the skill order are judged against the overall win rate.'}
          {' '}{SCORING_VERDICT[scoring.gate]}
          {catchingUp(scoringOn, scoring.inModel)}
        </>} />
      {swaps && (
        <SwitchRow label="Late-game swaps" checked={swapsOn} disabled={pending !== null}
          onChange={sendSwaps}
          hint={<>
            {swapsOn
              ? 'In place of the Sixth Item, the Items view shows for each item of the build whether players in drafts like this one did better keeping it or selling it for another at shop visits from 20:00 with every slot full, judged against their win chance just before the visit.'
              : 'The Items view shows the Sixth Item: what games finished sixth, usually after selling another.'}
            {' '}{SWAPS_VERDICT[swaps.gate]}
          </>} />
      )}
      <SwitchRow label="Follow the evaluation" checked={auto} disabled={pending !== null}
        onChange={(on) => sendMemory(on ? null : memory.active ? 'on' : 'off', { auto: on })}
        hint={auto
          ? 'Each switch above turns on only when Evaluate builds finds it better on your games with 95% confidence, and no worse otherwise. To keep those verdicts current, Evaluate builds also runs by itself (below).'
          : 'Your choices above hold whatever an evaluation finds.'} />
      {error && <div className="error">{error}</div>}
    </div>
  );
}

/** One unusual thing about this draft, as a fact: "Lane opponent magic damage 12% · low". */
function ContextChip({ c }: { c: ContextChipType }) {
  const binary = c.key === 'oRanged';
  const text = binary ? `${c.value === 'ranged' ? 'Ranged' : 'Melee'} lane opponent` : c.label.charAt(0).toUpperCase() + c.label.slice(1);
  return (
    <span className="inline-flex items-baseline gap-1 rounded-md border border-ember bg-ink-900 px-2 py-0.5 text-[11px] text-ash-400"
      title={`${c.z > 0 ? 'Above' : 'Below'} the stored games' average by ${Math.abs(c.z).toFixed(1)} standard deviations`}>
      {text}{!binary && <b className="text-ash-50">{c.value}</b>}
      {!binary && <span className={`text-[10px] font-semibold uppercase ${c.z > 0 ? 'text-blood-300' : 'text-[#5c8ef2]'}`}>{c.z > 0 ? 'high' : 'low'}</span>}
    </span>
  );
}

/** What the draft changed against the lane baseline, with the reason and the evidence; and the draft's context. */
export function DraftAdjustments({ meta, res, regions, status }: {
  meta: Meta; res: DraftResponse; regions: string[] | null; status: ModelStatus | null;
}) {
  const d = res.draft;
  if (!d) return <Card title="Draft adjustments"><Empty>{modelState(status) ?? 'Not enough stored games with timelines for the draft model yet.'}</Empty></Card>;
  const onRegions = (r: string[] | null) => { void api.setMatchup({ regions: r }).catch(() => {}); };
  const changed = d.picks.filter((p) => p.changed && !p.fallback);
  const thin = d.picks.filter((p) => p.fallback);
  const known = Object.keys(d.sample.regions).sort((a, b) => d.sample.regions[b] - d.sample.regions[a]);
  const active = regions ?? known;
  const toggle = (r: string) => {
    const next = active.includes(r) ? active.filter((x) => x !== r) : [...active, r];
    onRegions(next.length === known.length || !next.length ? null : next);
  };
  return (
    <Card title="Draft adjustments" aside={<span className="text-[11px] text-ash-600" title="Games weighted by pilot, patch and region; every number below is an estimate">
      {fmtCount(d.sample.nEff)} weighted games · {d.sample.pilots} pilots</span>}>
      {d.context.length > 0 && (
        <div className="mb-3 flex flex-wrap gap-1.5" aria-label="What stands out in this draft">
          {d.context.map((c) => <ContextChip key={c.key} c={c} />)}
        </div>
      )}
      {changed.length === 0
        ? <p className="text-ash-400">This draft doesn't change the lane build{thin.length ? ' (some choices have too few similar games)' : ''}.</p>
        : (
          <ul className="space-y-2.5">
            {changed.map((p) => {
              const base = p.baseline ? { name: pickLabel(meta, { key: p.key, value: p.baseline.value }), lift: p.baseline.lift } : null;
              return (
                <li key={p.key} className="flex items-start gap-2.5">
                  <PickIcon meta={meta} pick={p} />
                  <div className="min-w-0 leading-snug">
                    <div className="flex flex-wrap items-baseline gap-x-1.5">
                      <span className="text-ash-400">{p.label}</span>
                      <b className="text-ash-50">{pickLabel(meta, p)}</b>
                      {base && <span className="text-[11px] text-ash-600">instead of {base.name}</span>}
                      {p.reason && <span className="rounded-full bg-blood-900 px-2 py-px text-[11px] text-blood-200">{p.reason}</span>}
                    </div>
                    {p.stats && <Evidence s={p.stats} vsBaseline={base && base.lift !== null ? { name: base.name, lift: base.lift } : undefined} />}
                  </div>
                </li>
              );
            })}
          </ul>
        )}
      {thin.length > 0 && (
        <p className="mt-2 text-[11px] text-gold-400">
          Kept from the lane build (too few similar games): {thin.map((p) => p.label).join(', ')}
        </p>
      )}
      {known.length > 1 && (
        <div className="mt-3 flex flex-wrap items-center gap-1.5">
          <span className="text-[11px] text-ash-600" title="Which regions' games feed this draft. Click one to leave it out.">Regions</span>
          {known.map((r) => {
            const on = active.includes(r);
            return (
              <button key={r} type="button" aria-pressed={on} onClick={() => toggle(r)} title={`${d.sample.regions[r]} games in this lane · click to ${on ? 'leave out' : 'include'}`}
                className={`rounded border px-2 py-0.5 text-[11px] ${on ? 'border-blood-600 bg-blood-900 text-ash-50' : 'border-ink-600 text-ash-600 line-through'}`}>
                {REGION_NAME[r] ?? r} <span className={on ? 'text-ash-400' : ''}>{d.sample.regions[r].toLocaleString('en-US')}</span>
              </button>
            );
          })}
        </div>
      )}
    </Card>
  );
}
