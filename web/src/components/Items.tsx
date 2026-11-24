import type { BuildRecommendation, DraftModelInfo, Option, SwapAdvice } from '../../../shared/types.ts';
import { img, pts, type Meta } from '../api.ts';
import { Card, Empty, fmtGames, ItemIcon, WinStat } from './common.tsx';

/** Items in order; a starting set's repeats (two potions) show once with a count. */
function ItemSet({ meta, opt, arrows, stack }: { meta: Meta; opt: Option<number[]>; arrows?: boolean; stack?: boolean }) {
  const ids = stack ? [...new Set(opt.value)] : opt.value;
  const count = (id: number) => (stack ? opt.value.filter((x) => x === id).length : 1);
  return (
    <div className="item-set">
      <div className="item-row">
        {ids.map((id, i) => (
          <span key={`${id}-${i}`} className="item-step">
            {arrows && i > 0 && <span className="arrow">›</span>}
            <span className="relative">
              <ItemIcon meta={meta} id={id} />
              {count(id) > 1 && <span className="absolute -right-1 -bottom-1 rounded bg-ink-950 px-1 text-[10px] font-bold text-ash-50">×{count(id)}</span>}
            </span>
          </span>
        ))}
      </div>
      <WinStat winRate={opt.winRate} games={opt.games} />
    </div>
  );
}

function SlotOptions({ meta, title, options }: { meta: Meta; title: string; options: Option<number>[] }) {
  return (
    <div className="slot-col">
      <h3>{title}</h3>
      {options.length === 0 ? <Empty>Not enough data</Empty> : options.map((o) => (
        <div key={o.value} className="slot-option">
          <ItemIcon meta={meta} id={o.value} size={36} />
          <div className="slot-meta">
            <span className="slot-name">{meta.items[o.value]?.name}</span>
            <WinStat winRate={o.winRate} games={o.games} />
          </div>
        </div>
      ))}
    </div>
  );
}

/** Late-game swaps as the Items view shows them (0.17.0): the advice, and whose choice showing it is. */
export interface SwapsShown { advice: SwapAdvice[]; choice: NonNullable<DraftModelInfo['swaps']>['choice']; gate: NonNullable<DraftModelInfo['swaps']>['gate'] }

/** A share in points, signed, without the unit: "+1.2", "−0.4". */
const signed = (x: number) => `${x >= 0 ? '+' : '−'}${Math.abs(x * 100).toFixed(1)}`;
/** A range's end in points: "0.4", "−1.3". */
const end = (x: number) => `${x < 0 ? '−' : ''}${Math.abs(x * 100).toFixed(1)}`;
const whole = (n: number) => Math.round(n).toLocaleString('en-US');

/**
 * Late-game swaps (docs/SWAPS.md) in place of the Sixth Item: for each item of the draft-adjusted build, keep it or sell
 * it for another, its lift over keeping with the 95 % range, the situations behind it, the usual minute, and the other
 * ways players went (estimates).
 */
function SwapOptions({ meta, advice }: { meta: Meta; advice: SwapAdvice[] }) {
  const name = (id: number) => meta.items[id]?.name ?? `#${id}`;
  return (
    <div className="slot-col">
      <h3>Late-game swaps</h3>
      {advice.map((a, i) => {
        const pick = a.pick;
        return (
          <div key={`${a.held}-${i}`} className="slot-option !items-start">
            <ItemIcon meta={meta} id={a.held} size={36} />
            <div className="slot-meta">
              <span className="slot-name flex items-center gap-1">
                {a.thin ? <span className="text-gold-400" title="Too few similar situations to tell: keep it">Too few games: keep</span>
                  : pick.kind === 'swap'
                    ? <>Sell <span className="text-ash-600">→</span> <ItemIcon meta={meta} id={pick.item} size={20} /> <span className="truncate">{name(pick.item)}</span></>
                    : pick.kind === 'sell' ? 'Sell' : 'Keep'}
              </span>
              <div className="winstat" title={`Shop visits from 20:00 with every slot full where games held ${name(a.held)}, each game's together counting once; their similarity to this draft adds up to ${whole(a.nEff)}.`}>
                {a.lift !== null && a.low !== null && a.high !== null && (
                  <span className={`wr ${a.low > 0 ? '!text-gold-400' : ''}`}>
                    {pts(a.lift)}<span className="font-normal text-ash-600">, {end(a.low)} to {end(a.high)}</span>
                  </span>
                )}
                <span className="games">
                  {fmtGames(a.games)} situations · {whole(a.nEff)} like this draft{a.minute !== null ? ` · around minute ${Math.round(a.minute)}` : ''}
                </span>
              </div>
              {a.alternatives.length > 0 && (
                <span className="mt-0.5 flex flex-wrap items-center gap-x-2 gap-y-0.5 text-[10px] text-ash-600">
                  Others
                  {a.alternatives.slice(0, 3).map((o) => (
                    <span key={o.item ?? 'sell'} className="inline-flex items-center gap-0.5"
                      title={`${o.item !== null ? `Sell for ${name(o.item)}` : 'Sell'}: ${pts(o.lift)} against keeping it, ${fmtGames(o.games)} situations`}>
                      {o.item !== null ? <ItemIcon meta={meta} id={o.item} size={16} /> : 'Sell'} {signed(o.lift)}
                    </span>
                  ))}
                </span>
              )}
            </div>
          </div>
        );
      })}
    </div>
  );
}

/** What the swaps are and why they are shown, under the Situational Items. */
function swapsNote(s: SwapsShown): string {
  const why = s.choice === 'auto' ? 'Evaluate builds found following them better than keeping every item.'
    : `On by your choice (Data tab)${s.gate === 'passed' ? '; Evaluate builds found them better too'
      : s.gate === 'failed' ? ': Evaluate builds found no clear gain over keeping every item' : ': not evaluated yet'}.`;
  return `Late-game swaps: at shop visits from 20:00 with every slot full, each item of the draft-adjusted build kept or sold for another, against keeping it (adjusted for the game state). ${why}`;
}

export function Summoners({ meta, rec }: { meta: Meta; rec: BuildRecommendation }) {
  const s = rec.summoners;
  return (
    <Card title="Summoner Spells" aside={s && <WinStat winRate={s.winRate} games={s.games} />}>
      {!s ? <Empty>No data yet.</Empty> : (
        <div className="item-row">
          {s.value.map((id) => meta.summoners[id] && (
            <img key={id} className="icon" src={img.spell(meta.version, meta.summoners[id].image)} width={40} height={40}
              alt={meta.summoners[id].name} title={meta.summoners[id].name} />
          ))}
        </div>
      )}
    </Card>
  );
}

/** `swaps`: shown in place of the Sixth Item (null: the Sixth Item, as before 0.17.0). */
export function Items({ meta, rec, swaps = null }: { meta: Meta; rec: BuildRecommendation; swaps?: SwapsShown | null }) {
  return (
    <>
      <div className="items-top">
        <Card title="Starting Items">
          {rec.startingItems.length ? rec.startingItems.map((o) => <ItemSet key={o.value.join()} meta={meta} opt={o} stack />) : <Empty>No data yet.</Empty>}
        </Card>
        <Card title="Core Items" className="core-card">
          {rec.coreBuild.length ? rec.coreBuild.map((o, i) => (
            <div key={o.value.join()} className={i === 0 ? 'core-main' : 'core-alt'}>
              <ItemSet meta={meta} opt={o} arrows />
            </div>
          )) : <Empty>No completed 3-item builds on this patch yet.</Empty>}
        </Card>
        <Card title="Boots" aside={rec.bootsAfter != null && rec.boots.length > 0 && (
          <span className="muted" title="When the games finished their tier-2 boots (median)">
            {rec.bootsAfter === 0 ? 'before the first item' : `after item ${rec.bootsAfter}`}
          </span>
        )}>
          {rec.boots.length ? rec.boots.map((o) => (
            <div key={o.value} className="slot-option">
              <ItemIcon meta={meta} id={o.value} size={36} />
              <div className="slot-meta">
                <span className="slot-name">{meta.items[o.value]?.name}</span>
                <WinStat winRate={o.winRate} games={o.games} />
              </div>
            </div>
          )) : <Empty>No data yet.</Empty>}
        </Card>
      </div>
      <Card title="Situational Items" aside={<span className="muted">Filtered against core build — no shared unique passives</span>}>
        <div className="slots">
          <SlotOptions meta={meta} title="Fourth Item" options={rec.situational.slot4} />
          <SlotOptions meta={meta} title="Fifth Item" options={rec.situational.slot5} />
          {swaps ? <SwapOptions meta={meta} advice={swaps.advice} /> : <SlotOptions meta={meta} title="Sixth Item" options={rec.situational.slot6} />}
        </div>
        {swaps && <p className="mt-2 text-[11px] text-ash-600">{swapsNote(swaps)}</p>}
      </Card>
    </>
  );
}
