# Evaluate builds: quick updates between full evaluations (0.16.0)

A full evaluation (server/model/evaluate.ts) cross-validates the draft model by pilot on every stored game: about
12 minutes at 82,000 games on 4 threads, and its time grows with the square of the games. Most automatic evaluations
re-run it for a few hundred new games, re-measuring 82,000 games that haven't changed to add 300 that have.

A **quick update** keeps the last full evaluation's measurements and adds the new games' own:

1. A full evaluation saves its **base** next to its report: every held-out game's outcome per decision (followed the
   draft model's pick or not, the lane baseline's, the state-adjusted result), the Brier sums, the settings it chose
   (bandwidth, prior, state-model penalty, patch memory's fade, the expected win rate's penalty, fine-tuning's choice)
   and which games it held out (a hash of each match ID, nothing else).
2. A quick update reads the stored games as a full one does, puts every pilot in the same fold (the fold is a hash of
   the pilot), rebuilds each fold's models from all its training games, the new ones included, with the base's
   settings, and works out the **new held-out games only**. Their outcomes are exactly what a full evaluation with the
   same settings would give them.
3. The new outcomes are added to the base's, and every gate is decided again on all of them with the same rules and
   the same pilot bootstrap.

What it skips: the settings grids (bandwidth and prior, patch memory's fades, fine-tuning's settings), the state
model's penalty search, re-measuring games already measured, and the live replay (its numbers are kept).

## Quick or full: one button

`planEvaluation()` (server/model/evalBase.ts) decides, and Evaluate builds (the button and the automatic one) follows:

| Situation | Path | Why |
|---|---|---|
| No base yet, one an older app saved, or one cut short | full | nothing to add to |
| A patch the base didn't read, or a region on another current or previous patch than then | full | games' patch weights and ages change |
| The app's version, Data Dragon's, how games are captured, the data folder, or a setting the evaluation depends on (sources, weights, engine settings) changed | full | the base measured other models |
| Games since the last full evaluation over 20 % of the games it held out | full | its settings were chosen on far fewer games |
| The current patch's games grew over 30 % since the last full evaluation | full | the models lean on the current patch, so its new games move every measurement (measured below); early in a patch every evaluation stays full, as in 0.15, and the automatic one still waits for 3 % new games |
| Last full evaluation over 7 days ago | full | a fresh choice of settings now and then |
| A quick update moves a verdict clearly across the line (its interval clear of zero by a quarter of its half-width) | full, right after it | a verdict changes only on a full evaluation |
| A quick update moves a verdict only onto the line (or by its Brier score alone) | quick, the full evaluation's verdict kept | a verdict on the line would otherwise send every quick update on to a full one |
| Over 2 % of the base's held-out games no longer held out, or stored since in another form (backfilled) | full (found while it runs) | the base no longer describes the stored games |
| No new games since the last evaluation | none | nothing to add (a full one can still be asked for) |
| Otherwise | quick | |

"Full evaluation" stays available as its own action; `npm run eval:builds` is full unless given `--auto` or `--update`.

## Interfaces (the contract between the parts)

server/model/evalBase.ts

```ts
export const EVAL_LOGIC = 1;                       // bump when evaluate.ts measures differently: older bases go
export const UPDATE_RULES = { maxGrowth: 0.2, currentGrowth: 0.3, maxAgeDays: 7, maxMissing: 0.02, margin: 0.25 };
export type EvalPath = 'full' | 'update';
export interface EvalPlan {
  path: EvalPath | 'none';
  /** Why, for the Data tab: "new patch 25.21", "1,240 new games since the full evaluation of 21 Oct", "up to date". */
  reason: string;
  /** Stored games since the last evaluation (full or quick), and since the last full one. */
  newGames: number;
  sinceFull: number;
  /** The last full evaluation (null: none). */
  fullAt: number | null;
}
export function planEvaluation(db: Db, data: StaticData, now?: number): EvalPlan;
```

server/model/runEval.ts `evaluateDataFolder(db, data, o)` gains `o.path?: 'auto' | 'full' | 'update'` (default
`'full'`, as before) and returns `path: EvalPath` (what ran) and `reason: string` (why), besides what it returned.
`'auto'` follows planEvaluation, and goes on to a full evaluation when the quick update changes a verdict or finds too
many games gone (`reason` then says so). `'update'` with no usable base runs full.

`EvalSummary` (latest.json, server/model/service.ts) gains `path?: EvalPath` and `full?: { evaluatedAt: number; mark:
EvalMark }`: the full evaluation the verdicts rest on. `mark` stays the last evaluation's (full or quick).
`EvalReport` gains `update?: { fullAt: number; newGames: number; reason: string }`, set on a quick update's report.
Reports are saved as `eval-[auto-][update-]<stamp>.json|.txt`.

The job (server/jobs.ts, jobWorker.ts) carries `path: 'auto' | 'full'` to evaluateDataFolder and its result's `path`
and `reason`; quick updates' times are recorded apart from full ones (their own estimate). JobsStatus exposes the
plan for the Data tab. server/autoEval.ts starts a quick update after as many new games as 0.15 waited for (300, or
3 % of the games the last evaluation saw), at most every hour, and a full one when planEvaluation says full, after as
many games since the last full one, at most every 6 hours (a new patch as today). A full request while a quick update
runs stops it and starts the full one.

## Measured on the user's data (23–24 October 2026)

82,069 two-patch games, 10,689 pilots; patch 16.21 was young (1,194 games, against 83,189 of 16.20). Each quick update
was checked against a full evaluation of every game ("truth"): a copy without the newest N games (all of patch 16.21)
was evaluated in full (the base), then a quick update added them. Two threads (Irelia Companion was open for the first
pair, closed after); aggregates only.

| | +970 games | +286 | +118 | none new |
|---|---:|---:|---:|---:|
| Growth of the current patch over the base's | ×6 | 34 % | 11 % | 0 |
| Base's full evaluation | 1,931 s | 1,203 s | 1,039 s | |
| Quick update | 231 s | 173 s | 151 s | 68 s |
| Draft gate: base / quick update / truth (pts) | 1.10 / 1.11 / 1.30 | 1.27 / 1.28 / 1.30 | 1.35 / 1.36 / 1.30 | = truth |
| Fine-tuning: base / quick update / truth | passed / passed / passed | failed / failed / passed | failed / failed / passed | = truth |
| Other verdicts | the same | the same | the same | the same |

The truth's full evaluation in this branch is 0.15.2's to the last digit (+1.303 pts, 0.882 to 1.705). With nothing new,
a quick update is that full evaluation again, number for number, and with new games it adds exactly their outcomes.

What it shows:
- A quick update follows its base closely (within 0.015 pts on the draft gate) and is 7 to 8 times faster; with nothing
  new, 68 s, most of it reading the games.
- Its distance from a new full evaluation is the full evaluation's own movement: 118 games of a young patch (0.15 % of
  the games) move a full evaluation's draft gate by 0.05 pts and flip fine-tuning, whose interval starts at 0.00. The new
  current-patch games change every model's weights, so a full evaluation re-measures every game differently; a quick
  update keeps the base's measurements and settings. 970 games on 194 (×6) left it 0.19 pts away.
- So: while the current patch is young, every evaluation stays full (currentGrowth: over 30 % more current-patch games
  than the full evaluation had), as in 0.15. Once the patch has many games, new ones move the models little and quick
  updates carry the verdicts between full evaluations, which still come with every 30 % of current-patch growth, 20 %
  of all games, a week, a new patch or region move, or a verdict the new games move clearly across its line.
- Fine-tuning's verdict sits on its line on these games and flips between full evaluations a few hundred games apart.
  A quick update keeps the full evaluation's verdict meanwhile (UPDATE_RULES.margin), which flips less, not more.

What it saves (an estimate from these times, not measured over a patch): 0.15 runs a full evaluation for every 3 % of
new games; 16.20 gathered about 83,000 games, so about 30 full evaluations a patch at today's size. With quick updates,
the full ones come about a dozen times a patch (every 3 % while the patch is young, then every 30 % of its growth), and
the rest are quick updates of 1–3 minutes: roughly half the evaluation time a patch, more as the games grow (a full
evaluation's time grows with the square of the games, a quick update's with the games).

## An evaluation that travels with a backup (0.21.0)

A pool backup (server/transfer.ts) also carries the computer's last evaluation: its base (`base.bin`, `base.json`), its
summary (`latest.json`, the verdicts the app follows), and which games its database held at its full evaluation and at
its last one (each match ID hashed with the base's salt). Importing the backup leaves them aside (`evals/incoming/`), and
the app takes them as this computer's own (server/model/evalTravel.ts `adoptIncomingEvaluation`) when:

- the base is whole, of the same evaluation logic, and its settings key without the data folder (`portableKey`: app
  version, Data Dragon, capture version, options, engine settings, sources) is this computer's;
- every game it measured is in this pool now, give or take `UPDATE_RULES.maxMissing`;
- this computer has no base of its own whose full evaluation held out as many games.

Then its verdicts apply at once, its report is saved as `eval-imported-<stamp>.txt`, and its marks are set so the games
this pool holds that the other computer didn't have count as new: the next evaluation is a quick update of those, unless
the rules above call for a full one (over 20 % more, a new patch, a week old). Otherwise it is turned down with the
reason, and this computer's own evaluation stays. While an evaluation runs, the imported one waits until it ends.

Why the measurements can be kept: a held-out game's outcome was measured by models trained on the other computer's
pool, as a quick update keeps the outcomes of games measured before newer games came in. The rules that send a quick
update on to a full evaluation apply the same way.

