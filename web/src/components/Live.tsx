import { LANE_LABEL, toLane } from '../../../shared/lanes.ts';
import type { BestBuy, BuildMode, LiveGameState, LiveSummary } from '../../../shared/types.ts';
import { img, type Meta } from '../api.ts';
import { Card, Empty, ItemIcon } from './common.tsx';
import { Evidence } from './DraftAdjust.tsx';
import { goldK } from './LivePerformance.tsx';
import { clock } from './LivePerformance.tsx';

const gold = (n: number) => `${Math.round(n).toLocaleString('en-US')}g`;

/** Stage C3: what to buy with the gold you have, toward the next item. Shop advice only. */
export function BuyAdvice({ meta, buy }: { meta: Meta; buy: BestBuy }) {
  const name = (id: number) => meta.items[id]?.name ?? `#${id}`;
  const title = `${buy.reason}${buy.lift !== null && buy.games ? `\nThose games did ${buy.lift >= 0 ? '+' : '−'}${Math.abs(buy.lift * 100).toFixed(1)} pts against the game state (estimate).` : ''}`;
  return (
    <div className="mt-2 flex items-center gap-2.5 rounded-md border border-ember bg-ink-900 p-2" title={title} aria-label="Best buy for your gold">
      <span className="flex shrink-0 gap-0.5">{buy.items.map((id, i) => <ItemIcon key={`${id}-${i}`} meta={meta} id={id} size={28} />)}</span>
      <div className="min-w-0 leading-snug">
        <div className="text-ash-50">
          <span className="text-[11px] uppercase tracking-wider text-gold-400">With {gold(buy.gold)}</span>{' '}
          <b>{buy.items.map(name).join(' + ')}</b> <span className="text-[11px] text-ash-400">{gold(buy.cost)}</span>
        </div>
        <div className="truncate text-[11px] text-ash-400">{buy.finishes ? `Finishes ${name(buy.target)}` : `Toward ${name(buy.target)} · ${buy.reason}`}</div>
      </div>
    </div>
  );
}

/** Who is ahead, from the scoreboard: team item value and kills, and the most fed enemy and ally. */
export function SummaryLine({ meta, s, className = 'threat' }: { meta: Meta; s: LiveSummary; className?: string }) {
  const champ = (id: number) => meta.champions.find((c) => c.id === id)?.name ?? '?';
  return (
    <div className={className} title="From the scoreboard: the item value everyone can see on Tab, and kills. Estimates.">
      <span>Team items <b className={s.teamItems >= 0 ? 'text-[#2dbf8d]' : 'text-blood-300'}>{goldK(s.teamItems)}</b></span>
      <span>Kills <b>{s.kills > 0 ? '+' : ''}{s.kills}</b></span>
      {s.fedEnemy && <span>Most fed enemy <b className="text-blood-300">{champ(s.fedEnemy.championId)}</b> ({s.fedEnemy.ap ? 'AP' : 'AD'}, {(s.fedEnemy.items / 1000).toFixed(1)}k)</span>}
      {s.fedAlly && <span>Most fed ally <b className="text-[#2dbf8d]">{s.fedAlly.isMe ? 'you' : champ(s.fedAlly.championId)}</b> ({(s.fedAlly.items / 1000).toFixed(1)}k)</span>}
    </div>
  );
}

/** How the lane opponent was found when the game shows no positions. */
export const OPPONENT_NOTE: Record<string, string> = {
  draft: 'from champ select', guess: 'likely (no positions in this game)', manual: 'picked by you',
};

/**
 * The live game next to the build: you and your lane opponent, and the item advice of the mode you chose: "Next"
 * re-ranked from the game (draft-adjusted) or counter items for the enemy team (lane baseline). The CS, vision and
 * gold differences are in the live panel above the build (LivePerformance.tsx).
 */
export function LiveGame({ meta, live, mode }: { meta: Meta; live: LiveGameState | null; mode: BuildMode }) {
  if (!live) {
    return <Card title="Live Game"><Empty>No game running. Your lane stats and item advice appear here once you load in.</Empty></Card>;
  }
  const me = live.players.find((p) => p.isMe);
  const opp = live.laneOpponent;
  const lane = toLane(me?.position);
  const note = live.laneOpponentFrom ? OPPONENT_NOTE[live.laneOpponentFrom] : undefined;
  const row = (p: NonNullable<typeof me>, label: string) => {
    const c = meta.champions.find((x) => x.id === p.championId);
    return (
      <div className="live-row">
        {c && <img className="icon" src={img.champ(meta.version, c.key)} width={36} height={36} alt="" />}
        <div className="live-who"><b>{label}</b><span>{p.championName} · Lv {p.level}</span></div>
        <span className="kda">{p.kills}/{p.deaths}/{p.assists}</span>
        <span className="cs">{p.creepScore} CS</span>
        <div className="item-row small">{p.items.filter(Boolean).map((id, i) => <ItemIcon key={i} meta={meta} id={id} size={26} />)}</div>
      </div>
    );
  };

  return (
    <Card title="Live Game" aside={<span className="pill live">● {clock(live.gameTime)} · {Math.floor(live.myGold)}g</span>}>
      {me && row(me, 'You')}
      {opp && row(opp, lane ? LANE_LABEL[lane] : 'Lane')}
      {opp && note && <div className="text-[11px] text-ash-600">Lane opponent {note}</div>}
      {mode === 'draft' && live.next && (
        <div className="mt-2 flex items-start gap-2.5 rounded-md border border-gold-400/25 bg-ink-900 p-2" aria-label="Next item, adjusted to this game">
          <ItemIcon meta={meta} id={live.next.itemId} size={34} />
          <div className="min-w-0 leading-snug">
            <div className="text-ash-50">
              <span className="text-[11px] uppercase tracking-wider text-gold-400">Next</span> <b>{meta.items[live.next.itemId]?.name}</b>
              {live.next.inProgress && <span className="ml-1 text-[11px] text-ash-400">· in progress</span>}
            </div>
            <div className="text-[11px] text-ash-400">{live.next.reason ? `Adjusted for: ${live.next.reason}` : `Item ${live.next.slot} of the draft-adjusted build`}</div>
            <Evidence s={live.next.stats} />
          </div>
        </div>
      )}
      {live.buy && <BuyAdvice meta={meta} buy={live.buy} />}
      {live.summary && <SummaryLine meta={meta} s={live.summary} />}
      <div className="threat">
        <span>Enemy damage <b>{Math.round(live.threat.apShare * 100)}% magic</b></span>
        <span>Tanks <b>{live.threat.tanks}</b></span>
        <span>Healers <b>{live.threat.healers}</b></span>
      </div>
      {(mode === 'baseline' || !live.next) && <div className="suggestions">
        {live.suggestions.length === 0 && <span className="muted">No counter items needed against this team.</span>}
        {live.suggestions.map((s) => (
          <div key={s.itemId} className={`suggestion ${s.blockedBy ? 'blocked' : ''}`}>
            <ItemIcon meta={meta} id={s.replacement ?? s.itemId} size={32} />
            <div>
              <b>{meta.items[s.replacement ?? s.itemId]?.name}</b>
              <span className="muted">{s.reason}</span>
              {s.blockedBy && (
                <span className="blocked-note">
                  {meta.items[s.itemId]?.name} blocked by {s.blockedBy}{s.replacement ? ' → alternative shown' : ''}
                </span>
              )}
            </div>
          </div>
        ))}
      </div>}
    </Card>
  );
}
