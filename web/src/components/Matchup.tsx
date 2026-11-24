import { useState } from 'react';
import type { Lane, LcuState, Matchup as MatchupState } from '../../../shared/types.ts';
import { api, img, type Meta } from '../api.ts';
import { LANE_LABEL, LANES, laneSource } from '../lane.ts';

/** How the opponent was chosen, for the chip's tooltip and the "Follow" link. */
const FROM_TEXT: Record<NonNullable<MatchupState['opponentFrom']>, string> = {
  game: 'same position in game', draft: 'likely lane opponent from champ select', guess: 'the enemy Irelia faces most in this lane', manual: 'picked by you',
};

/** Top / Mid switch. Says where the lane comes from: the game, champ select, or your own pick. */
function LaneSwitch({ m }: { m: MatchupState }) {
  const pick = (lane: Lane) => { void api.setMatchup({ lane }).catch(() => {}); };
  return (
    <div className="lane-switch" role="group" aria-label="Lane">
      {LANES.map((l) => (
        <button key={l} type="button" aria-pressed={m.lane === l} className={m.lane === l ? 'active' : ''} onClick={() => pick(l)}>
          {LANE_LABEL[l]}
        </button>
      ))}
      <span className="lane-source">{laneSource(m)}</span>
    </div>
  );
}

/**
 * The matchup bar: lane, the enemy picks (from champ select or the game) and opponent search. Every change goes to the
 * server, which pushes the new matchup and its build to every window (the overlay included).
 */
export function Matchup({ meta, lcu, m }: { meta: Meta; lcu: LcuState | null; m: MatchupState }) {
  const [query, setQuery] = useState('');
  const champ = (id: number) => meta.champions.find((c) => c.id === id);
  const choose = (opponentId: number | null) => { void api.setMatchup({ opponentId }).catch(() => {}); };
  const draft = lcu?.champSelect;
  const hovering = new Set(m.phase === 'draft' ? (draft?.enemies ?? []).filter((e) => e.championId && !e.locked).map((e) => e.championId) : []);
  const matches = query.length >= 1
    ? meta.champions.filter((c) => c.name.toLowerCase().startsWith(query.toLowerCase()) && c.id !== 39).slice(0, 8)
    : [];

  return (
    <div className="matchup-bar">
      <div className="matchup-draft">
        <LaneSwitch m={m} />
        <span className="label">{m.phase === 'game' ? 'Enemy team' : m.phase === 'draft' ? 'Enemy draft' : 'Matchup'}</span>
        {m.enemies.map((id) => {
          const c = champ(id);
          if (!c) return null;
          const active = m.opponentId === id;
          return (
            <button key={id} type="button" className={`champ-chip ${active ? 'active' : ''} ${hovering.has(id) ? 'hover' : ''}`}
              onClick={() => choose(id)} aria-pressed={active}
              title={`${c.name}${hovering.has(id) ? ' (hovering)' : ''}${active && m.opponentFrom ? ` — ${FROM_TEXT[m.opponentFrom]}` : ''}`}>
              <img src={img.champ(meta.version, c.key)} width={34} height={34} alt={c.name} />
              {active && m.opponentFrom !== 'manual' && <span className="lane-dot" />}
            </button>
          );
        })}
        {m.phase === 'draft' && !m.enemies.length && <span className="muted">Waiting for enemy picks…</span>}
        {m.phase !== 'idle' && m.opponentFrom === 'manual' && (
          <button type="button" className="ghost !px-2 !py-0.5 text-[11px]" onClick={() => void api.setMatchup({ follow: true }).catch(() => {})}
            title={`Go back to the opponent ${m.phase === 'game' ? 'the game' : 'champ select'} suggests`}>
            Follow {m.phase === 'game' ? 'the game' : 'the draft'}
          </button>
        )}
        {draft && !draft.isIrelia && draft.myChampionId > 0 && m.phase === 'draft' && <span className="warn">You are not on Irelia — showing Irelia data anyway</span>}
      </div>
      <div className="matchup-search">
        <input value={query} onChange={(e) => setQuery(e.target.value)} placeholder="Search opponent…" aria-label="Search opponent champion" />
        {matches.length > 0 && (
          <ul className="search-results">
            {matches.map((c) => (
              <li key={c.id}>
                <button type="button" onClick={() => { choose(c.id); setQuery(''); }}>
                  <img src={img.champ(meta.version, c.key)} width={22} height={22} alt="" /> {c.name}
                </button>
              </li>
            ))}
          </ul>
        )}
        {m.opponentId !== null && <button type="button" className="ghost" onClick={() => choose(null)}>All matchups</button>}
      </div>
    </div>
  );
}
