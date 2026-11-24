# Late-game swaps (0.17.0)

The Items view's "Sixth Item" card showed the item games finished sixth: usually only after selling boots or another
item, so it read as a seventh slot. What helps late in a game is which item to sell for which. This replaces that card
with **late-game swap advice** for the build's items, measured the way the app measures every other decision, and
shown only once Evaluate builds finds following it better than keeping every item (or the Data tab says so).

## What a swap is (server/model/swaps.ts)

From a stored game's timeline (Riot's item events: buys, sells, undos, with their times), seen from Irelia:

- **A shop visit:** item events at most `SWAP_DEFAULTS.visitGapMs` (30 s) apart.
- **A decision point:** a visit from `SWAP_DEFAULTS.fromMinute` (20:00) on while every slot is full: five legendary
  items and tier-2 boots, or six legendaries (inventory just before the visit, undos applied).
- **At a decision point, for each item held** (legendaries and tier-2 boots): **kept** (not sold at this visit),
  **swapped for Y** (sold, and Y the next legendary finished within `SWAP_DEFAULTS.replaceWithinMs`, 10 minutes, at
  this visit or a later one: boots are often sold for components first), or **sold** (nothing finished in time). An
  item swapped is no longer held at the game's later decision points.
- **Weight:** a game's decision points share one game's weight (1 / its decision points), so long games, which have
  more shop visits, count no more than short ones.

## How it is scored

Each decision is scored like an item purchase: the game's result minus its win chance just before the visit (the
state model, from the gold, levels, kills and objectives at that moment). Comparing at the visit, against players in
the same spot who kept the item, leaves out what raw win rates mix in: swaps happen in long games and in games going
badly (on the user's games, games with a late swap won 42.5 % against 53 % overall).

The draft model estimates each held item's options (`swap:<item>`, the options `keep`, an item id, `sell`) from these
observations as it does every decision: shrunk from the lane towards drafts like this one (the similarity of the draft
and, where known, of the game at that moment), towards the matchup's own. Keeping is the reference: each option's
WPA counts from keeping's mean for that item, so an option with few games shrinks towards keeping, not towards a lead
the data doesn't show (late in a game keeping's mean isn't 0). Its pick is the best option whose lead over keeping is
clear (its 95 % range above zero), else keep: the best of many options with few games each beats keeping by chance.
With too few similar situations (`MIN_EFFECTIVE_GAMES`) it keeps the item and says so (`thin`).

Measured on the user's games (82,069, before the clear-lead rule): 68,215 situations in 5,203 games, the item sold in
3.3 % of them; keeping did 7.2 pts better than selling (state-adjusted), and advice that sold in 49 % of them did 7.21
pts worse than keeping every item (−9.29 to −4.90): the gate failed.

## The gate (Evaluate builds)

On the held-out games' decision points: the outcomes of those that did what the model advises for the item (from the
game's draft, as the Items view shows it before the game) against those that didn't, minus the same for always
keeping; each game's points together count once (the weights above). A pilot bootstrap gives its 95 % interval. It
passes when that interval is above zero. Reported as "Late-game swaps" in the report; a quick update (docs/
EVAL-UPDATE.md) adds the new games' points as for the other gates.

## In the app

- `DraftRecommendation.swaps` (shared/types.ts `SwapAdvice[]`): for each item of the draft-adjusted build (core,
  boots, items 4–5), computed with every draft.
- `DraftModelInfo.swaps`: shown (`active`) when the Data tab's switch says On, or, following the evaluation (no
  choice of yours), once the last evaluation passed it. Off: the Items view keeps the Sixth Item card.
- The card: per item, keep it or sell it for Y, the lift over keeping and its range, how many situations, the usual
  minute, the other ways players went.
