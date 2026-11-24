import { useEffect, useState } from 'react';
import type { GameReview, Lane, PersonalReport } from '../../../shared/types.ts';
import { api, img, pts, type Meta } from '../api.ts';
import { LANE_LABEL, LANES } from '../lane.ts';
import { Card, Empty, ItemIcon } from './common.tsx';
import { pickLabel } from './DraftAdjust.tsx';

// Personal layer (Stage B4): your own Irelia games against the high-elo ones, and a review of each game: what the app
// showed, what you bought and how it went. Everything here is computed on this machine from your stored games.

type Games = Awaited<ReturnType<typeof api.personal>>['games'];

const minute = (m: number | null) => (m == null ? '—' : `${Math.floor(m)}:${String(Math.round((m % 1) * 60)).padStart(2, '0')}`);
const signed = (n: number) => `${n > 0 ? '+' : ''}${Math.round(n).toLocaleString('en-US')}`;
const shortDate = (t: number) => new Date(t).toLocaleDateString(undefined, { month: 'short', day: 'numeric' });

/** Item, when you finished it, the high-elo median, and the difference (faster in green). */
function TimingTable({ meta, rows }: { meta: Meta; rows: { itemId: number; you: number; highElo: number | null; note?: string }[] }) {
  return (
    <table className="w-full table-fixed text-[11px] tabular-nums">
      <colgroup><col /><col className="w-10" /><col className="w-14" /><col className="w-11" /></colgroup>
      <thead>
        <tr className="text-ash-600"><th className="text-left font-normal">Item</th><th className="pl-2 text-right font-normal">You</th>
          <th className="pl-2 text-right font-normal whitespace-nowrap">High elo</th><th className="pl-2 text-right font-normal">Δ</th></tr>
      </thead>
      <tbody>
        {rows.map((r) => {
          const d = r.highElo === null ? null : r.you - r.highElo;
          return (
            <tr key={r.itemId} title={r.note}>
              <td className="py-0.5"><span className="flex min-w-0 items-center gap-1.5"><ItemIcon meta={meta} id={r.itemId} size={20} />
                <span className="truncate text-ash-200">{meta.items[r.itemId]?.name}</span></span></td>
              <td className="pl-2 text-right text-ash-50">{minute(r.you)}</td>
              <td className="pl-2 text-right text-ash-400">{minute(r.highElo)}</td>
              <td className={`pl-2 text-right whitespace-nowrap ${d === null ? 'text-ash-600' : d <= -0.5 ? 'text-[#2dbf8d]' : d >= 0.5 ? 'text-blood-300' : 'text-ash-400'}`}>
                {d === null ? '—' : `${d > 0 ? '+' : d < 0 ? '−' : ''}${minute(Math.abs(d))}`}
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}

function ReviewDetail({ meta, r }: { meta: Meta; r: GameReview }) {
  const draft = r.shown.find((s) => s.phase === 'draft');
  const live = r.shown.find((s) => s.phase === 'live');
  const opp = meta.champions.find((c) => c.id === r.opponentId);
  return (
    <div className="mt-1.5 space-y-2 rounded bg-ink-900 p-2.5">
      <div className="flex items-center gap-2">
        {opp && <img src={img.champ(meta.version, opp.key)} alt="" className="size-7 rounded" />}
        <b className={r.win ? 'text-[#2dbf8d]' : 'text-blood-400'}>{r.win ? 'Win' : 'Loss'}</b>
        <span className="text-ash-400">{r.kda.join(' / ')} · {r.minutes} min{opp ? ` vs ${opp.name}` : ''}</span>
      </div>
      {(r.lane10 || r.lane15) && (
        <div className="flex flex-wrap gap-x-4 text-[11px] text-ash-400 [&>span]:whitespace-nowrap">
          {r.lane10 && <span>At 10:00 <b className={r.lane10.gold >= 0 ? 'text-[#2dbf8d]' : 'text-blood-300'}>{signed(r.lane10.gold)}g</b> · {signed(r.lane10.cs)} CS</span>}
          {r.lane15 && <span>At 15:00 <b className={r.lane15.gold >= 0 ? 'text-[#2dbf8d]' : 'text-blood-300'}>{signed(r.lane15.gold)}g</b> · {signed(r.lane15.cs)} CS</span>}
        </div>
      )}
      {r.items.length
        ? <TimingTable meta={meta} rows={r.items.map((i) => ({ itemId: i.itemId, you: i.minute, highElo: i.highEloMinute }))} />
        : <div className="text-[11px] text-ash-600">No completed item.</div>}
      {draft ? (
        <div>
          <div className="text-[11px] text-ash-600">Shown in champ select ({draft.mode === 'draft' ? 'draft-adjusted' : 'lane build'})</div>
          <div className="mt-1 flex flex-wrap items-center gap-1">
            {draft.build.core.map((id) => <ItemIcon key={id} meta={meta} id={id} size={24} />)}
            {r.followed && <span className="ml-1.5 text-[11px] text-ash-400">
              {r.followed.keystone ? 'same keystone' : 'other keystone'} · {r.followed.core} of 3 core items followed
            </span>}
          </div>
        </div>
      ) : <div className="text-[11px] text-ash-600">Nothing was recorded in champ select for this game.</div>}
      {live?.build.next?.length ? (
        <div>
          <div className="text-[11px] text-ash-600">"Next" in game</div>
          <div className="mt-1 flex gap-1">{live.build.next.map((id, i) => <ItemIcon key={`${id}-${i}`} meta={meta} id={id} size={22} />)}</div>
        </div>
      ) : null}
    </div>
  );
}

/**
 * The Your games tab: your item timings and what works for you against the high-elo games, and a review of each game.
 * Read again when the model is rebuilt or one of your games arrives; `refresh` stands for both.
 */
export function YourGames({ meta, lane: matchupLane, refresh }: { meta: Meta; lane: Lane; refresh: string }) {
  const [lane, setLane] = useState<Lane | null>(matchupLane);
  const [data, setData] = useState<{ report: PersonalReport | null; games: Games } | null>(null);
  const [open, setOpen] = useState<string | null>(null);
  const [review, setReview] = useState<GameReview | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let current = true;
    api.personal(lane).then((d) => { if (current) { setData(d); setError(null); } }).catch((e) => { if (current) setError(e instanceof Error ? e.message : String(e)); });
    return () => { current = false; };
  }, [lane, refresh]);
  useEffect(() => {
    setReview(null);
    if (open) api.review(open).then(setReview).catch((e) => setError(e instanceof Error ? e.message : String(e)));
  }, [open, refresh]);

  const report = data?.report ?? null;
  const games = data?.games ?? [];
  const laneSwitch = (
    <div className="lane-switch" role="group" aria-label="Lane">
      {[...LANES, null].map((l) => (
        <button key={l ?? 'all'} type="button" aria-pressed={lane === l} className={lane === l ? 'active' : ''} onClick={() => setLane(l)}>
          {l ? LANE_LABEL[l] : 'Both'}
        </button>
      ))}
    </div>
  );
  if (!data) return <Card title="Your games"><Empty>{error ?? 'Loading…'}</Empty></Card>;
  if (!games.length) {
    return <Card title="Your games"><Empty>Your ranked Irelia games (solo and flex, this patch and the last) appear here once the League client has been open with the app running and a Riot API key is set: they are fetched after each game. Settings → Your account shows which account and server are used.</Empty></Card>;
  }
  return (
    <div className="grid gap-4 lg:grid-cols-2">
      <Card title="Your item timings" aside={laneSwitch}>
        {error && <div className="error">{error}</div>}
        {report && report.games > 0 && (
          <p className="mb-2 text-[11px] text-ash-600">{lane ? LANE_LABEL[lane] : 'Top and Mid'}: {report.wins} of {report.games} won. When you finished each item, against the high-elo median.</p>
        )}
        {!report
          ? <Empty>The model is still loading: your timings appear in a moment.</Empty>
          : report.timings.length === 0 ? <Empty>No finished items in these games yet.</Empty>
          : <TimingTable meta={meta} rows={report.timings.slice(0, 8).map((t) => ({
            itemId: t.itemId, you: t.you, highElo: t.highElo, note: `${t.games} of your games, ${t.highEloGames} high-elo games`,
          }))} />}
        {report && report.options.some((o) => o.games >= 2) && (
          <>
            <h3 className="mt-4 text-[11px] uppercase tracking-wider text-ash-600">What works for you <span className="normal-case tracking-normal">(estimates)</span></h3>
            <ul className="mt-1 space-y-0.5 text-[12px]">
              {report.options.filter((o) => o.games >= 2).map((o) => (
                <li key={`${o.key}-${o.value}-${o.lane}`} className="flex justify-between gap-2"
                  title={`Your games: ${pts(o.yourLift)} over ${o.games}; high-elo: ${pts(o.highEloLift)}. Blended: your games count more as they add up.`}>
                  <span className="truncate">{o.label}: <span className="text-ash-200">{pickLabel(meta, o)}</span> <span className="text-ash-600">({o.games})</span></span>
                  <span className={`tabular-nums ${o.blended > 0 ? 'text-[#2dbf8d]' : 'text-ash-400'}`}>{pts(o.blended)}</span>
                </li>
              ))}
            </ul>
          </>
        )}
      </Card>
      <Card title="Post-game review" aside={<span className="text-[11px] text-ash-600">What the app showed, what you bought, how it went</span>}>
        <ul className="space-y-1">
          {games.slice(0, 12).map((g) => {
            const opp = meta.champions.find((c) => c.id === g.opponentId);
            return (
              <li key={g.matchId}>
                <button type="button" onClick={() => setOpen(open === g.matchId ? null : g.matchId)} aria-expanded={open === g.matchId}
                  className={`flex w-full items-center gap-2 rounded px-1 py-0.5 text-left hover:bg-ink-900 ${open === g.matchId ? 'bg-ink-900' : ''}`}>
                  <span className={`w-7 text-[11px] font-bold ${g.win ? 'text-[#2dbf8d]' : 'text-blood-400'}`}>{g.win ? 'Win' : 'Loss'}</span>
                  {opp ? <img src={img.champ(meta.version, opp.key)} alt="" className="size-5 rounded" /> : <span className="size-5" />}
                  <span className="text-ash-200">{opp ? `vs ${opp.name}` : 'Unknown opponent'}</span>
                  <span className="ml-auto text-[11px] text-ash-600">{g.lane === 'MIDDLE' ? 'Mid' : g.lane === 'TOP' ? 'Top' : ''} · {shortDate(g.created)}</span>
                  <span className="text-ash-600" aria-hidden="true">{open === g.matchId ? '▴' : '▾'}</span>
                </button>
                {open === g.matchId && (review ? <ReviewDetail meta={meta} r={review} /> : <div className="p-2 text-ash-600">Loading…</div>)}
              </li>
            );
          })}
        </ul>
      </Card>
    </div>
  );
}
