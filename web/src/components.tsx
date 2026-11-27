// Shared pieces: icons, a build's sections with their evidence, the champion picker, cards and meters.
import { useMemo, useState, type ReactNode } from 'react';
import { useDd } from './ddragon.ts';
import { count, KIND_ORDER, KIND_TITLES, keystoneOf, meter, optionParts, pageForKeystone, pct, pts, ROLE_NAMES } from './format.ts';
import type { BudgetStatus, BuildRecommendation, RankedOption, Role, Section } from './types.ts';
import { ROLES } from './types.ts';

export function Card({ title, right, children, className = '' }: { title?: ReactNode; right?: ReactNode; children: ReactNode; className?: string }) {
  return (
    <section className={`rounded-lg border border-line bg-panel p-4 ${className}`}>
      {(title || right) && (
        <header className="mb-3 flex items-center justify-between gap-3">
          <h2 className="text-sm font-semibold tracking-wide text-fg">{title}</h2>
          {right}
        </header>
      )}
      {children}
    </section>
  );
}

export function Img({ src, title, size = 32, round = false }: { src: string | null; title: string; size?: number; round?: boolean }) {
  const [failed, setFailed] = useState(false);
  const style = { width: size, height: size };
  if (!src || failed) {
    return <span title={title} style={style} className={`inline-grid shrink-0 place-items-center bg-raised text-[10px] text-muted ${round ? 'rounded-full' : 'rounded'}`}>{title.slice(0, 2)}</span>;
  }
  return <img src={src} alt={title} title={title} style={style} loading="lazy" onError={() => setFailed(true)} className={`shrink-0 ${round ? 'rounded-full' : 'rounded'}`} />;
}

export function ChampionIcon({ id, size = 32 }: { id: number; size?: number }) {
  const dd = useDd();
  return <Img src={dd.championIcon(id)} title={dd.championName(id)} size={size} />;
}

/** One option drawn from its key: items, runes, spells or skills. */
export function OptionIcons({ o, size = 28 }: { o: RankedOption; size?: number }) {
  const dd = useDd();
  const p = optionParts(o.option);
  switch (p.kind) {
    case 'items':
      return (
        <span className="flex items-center gap-1">
          {p.ids.map((id, i) => (
            <span key={i} className="flex items-center gap-1">
              {i > 0 && p.ordered && <span className="text-muted">›</span>}
              <Img src={dd.itemIcon(id)} title={dd.itemName(id)} size={size} />
            </span>
          ))}
        </span>
      );
    case 'runes':
      return (
        <span className="flex items-center gap-1">
          <Img src={dd.runeIcon(p.page.perks[0])} title={dd.runeName(p.page.perks[0])} size={size + 4} round />
          {p.page.perks.slice(1).map((id) => <Img key={id} src={dd.runeIcon(id)} title={dd.runeName(id)} size={size - 8} round />)}
          <span className="mx-1 h-4 w-px bg-line" />
          {p.page.subPerks.map((id) => <Img key={id} src={dd.runeIcon(id)} title={dd.runeName(id)} size={size - 8} round />)}
          <span className="mx-1 h-4 w-px bg-line" />
          {p.page.shards.map((id, i) => <Img key={i} src={dd.runeIcon(id)} title={dd.runeName(id)} size={size - 12} round />)}
        </span>
      );
    case 'keystone':
      return <Img src={dd.runeIcon(p.id)} title={dd.runeName(p.id)} size={size} round />;
    case 'summoners':
      return <span className="flex gap-1">{p.ids.map((id) => <Img key={id} src={dd.summonerIcon(id)} title={dd.summonerName(id)} size={size} />)}</span>;
    case 'skills':
      return (
        <span className="flex items-center gap-1 font-mono text-sm">
          {p.letters.map((l, i) => (
            <span key={i} className="flex items-center gap-1">
              {i > 0 && <span className="text-muted">{p.ordered ? '›' : ','}</span>}
              <span className="grid h-6 w-6 place-items-center rounded bg-raised text-accent">{l}</span>
            </span>
          ))}
        </span>
      );
    default:
      return <span className="font-mono text-xs">{p.text}</span>;
  }
}

const BASIS: Record<RankedOption['basis'], string> = {
  matchup: 'from games against this lane opponent',
  profile: "from games against this team's damage mix",
  lane: 'from all games of this champion in this role',
  champion: 'from all games of this champion',
};

/** The evidence beside an option: games, win rate, win probability added, the basis, and a thin-data flag. */
export function Evidence({ o, compact = false }: { o: RankedOption; compact?: boolean }) {
  return (
    <span className={`flex flex-wrap items-center gap-x-3 gap-y-1 ${compact ? 'text-[11px]' : 'text-xs'} text-muted`}>
      <span title={`${count(o.wins)} wins of ${count(o.games)} games (${o.weightedGames.toFixed(0)} after weighting by recency and rank)`}>
        <b className="text-fg">{count(o.games)}</b> {o.games === 1 ? 'game' : 'games'}
      </span>
      {/* What happened in those games; the estimate beside it is what the ranking uses. */}
      {o.games > 0 && <span title="Win rate in these games, as played">{pct(o.wins / o.games)} win</span>}
      <span title="Estimated win probability added: these games' result against the baseline, shrunk toward no effect when games are few (a few lucky games move it very little)" className={o.wpa > 0 ? 'text-good' : o.wpa < 0 ? 'text-bad' : ''}>{pts(o.wpa)} est.</span>
      {o.predictedWin != null && <span title="The model's predicted win chance in a context like yours">model {pct(o.predictedWin)}</span>}
      {!compact && <span title={BASIS[o.basis]}>basis: {o.basis}</span>}
      {o.thin && <span className="rounded bg-warn/15 px-1.5 text-warn" title="Too few games in this role to judge: listed after the options the data support">thin data</span>}
    </span>
  );
}

/**
 * One section of the build, best first. With `onPick` its rows can be picked (the rune page to import, or a keystone
 * that picks its best page): the picked row is highlighted, else the first.
 */
export function SectionView({ s, limit = 4, picked, onPick }: { s: Section; limit?: number; picked?: string | null; onPick?: (o: RankedOption) => void }) {
  const [all, setAll] = useState(false);
  const pickedAt = picked == null ? -1 : s.options.findIndex((o) => o.option.key === picked);
  // A picked row past the limit stays in view.
  const shown = all ? s.options : s.options.slice(0, Math.max(limit, pickedAt + 1));
  const highlighted = onPick ? pickedAt : 0;
  return (
    <Card
      title={KIND_TITLES[s.kind]}
      right={
        <span className="flex items-center gap-3">
          {onPick && s.options.length > 1 && <span className="text-[11px] text-muted">click one to import it instead</span>}
          {s.options.length > limit && <button className="text-xs text-accent hover:underline" onClick={() => setAll(!all)}>{all ? 'Fewer' : `All ${s.options.length}`}</button>}
        </span>
      }
    >
      {s.options.length === 0 ? (
        <p className="text-sm text-muted">No games yet.</p>
      ) : (
        <ol className="space-y-2">
          {shown.map((o, i) => {
            const body = (
              <>
                <div className="flex items-center gap-3">
                  <span className="w-4 text-xs text-muted">{i + 1}</span>
                  <OptionIcons o={o} />
                  <span className="truncate text-sm text-fg" title={o.label}>{o.label}</span>
                  {onPick && i === highlighted && <span className="ml-auto shrink-0 text-[11px] text-accent">will import</span>}
                </div>
                <div className="pl-7"><Evidence o={o} /></div>
              </>
            );
            const base = `flex w-full flex-col gap-1 rounded-md px-2 py-2 text-left ${i === highlighted ? 'bg-raised' : ''}`;
            return (
              <li key={o.option.key}>
                {onPick ? (
                  <button type="button" aria-pressed={i === highlighted} onClick={() => onPick(o)} className={`${base} ${i === highlighted ? 'ring-1 ring-accent' : 'hover:bg-raised/60'}`}>
                    {body}
                  </button>
                ) : (
                  <div className={base}>{body}</div>
                )}
              </li>
            );
          })}
        </ol>
      )}
    </Card>
  );
}

/**
 * A build, every section with its evidence. With `onRunePage` the rune page to import can be picked: a rune page row
 * picks that page, a keystone row picks that keystone's best listed page.
 */
export function BuildView({ rec, runePage, onRunePage }: { rec: BuildRecommendation; runePage?: string | null; onRunePage?: (key: string) => void }) {
  const dd = useDd();
  const sections = [...rec.sections].sort((a, b) => KIND_ORDER.indexOf(a.kind) - KIND_ORDER.indexOf(b.kind));
  const pick = (s: Section): Pick<Parameters<typeof SectionView>[0], 'picked' | 'onPick'> => {
    if (!onRunePage) return {};
    if (s.kind === 'runePage') return { picked: runePage ?? null, onPick: (o) => onRunePage(o.option.key) };
    if (s.kind === 'keystone') {
      const k = keystoneOf(runePage ?? null);
      return {
        picked: k == null ? null : String(k),
        onPick: (o) => {
          const page = pageForKeystone(rec, Number(o.option.key));
          if (page) onRunePage(page);
        },
      };
    }
    return {};
  };
  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-sm text-muted">
        <span><b className="text-fg">{count(rec.games)}</b> games behind this {ROLE_NAMES[rec.role]} build</span>
        <span title={rec.tier === 'a' ? 'Tier A: the learned model ranks the options' : 'Tier B: counts with expected wins rank the options'}>tier {rec.tier.toUpperCase()}</span>
        {rec.patch && <span>patch {rec.patch.major}.{rec.patch.minor}</span>}
        {rec.laneOpponent != null && <span className="flex items-center gap-1">vs <ChampionIcon id={rec.laneOpponent} size={20} /> {dd.championName(rec.laneOpponent)}</span>}
        {rec.enemyProfile && <span>enemy damage: {rec.enemyProfile}</span>}
        {rec.model && <span>model {rec.model}</span>}
      </div>
      {rec.notes.length > 0 && (
        <ul className="space-y-1 rounded-md border border-warn/30 bg-warn/10 p-3 text-sm text-warn">
          {rec.notes.map((n) => <li key={n}>{n}</li>)}
        </ul>
      )}
      <div className="grid gap-4 lg:grid-cols-2">
        {sections.map((s) => <SectionView key={s.kind} s={s} {...pick(s)} />)}
      </div>
    </div>
  );
}

export function RolePicker({ value, onChange, allowAuto = true }: { value: Role | null; onChange: (r: Role | null) => void; allowAuto?: boolean }) {
  const opts: (Role | null)[] = allowAuto ? [null, ...ROLES] : ROLES;
  return (
    <div className="flex overflow-hidden rounded-md border border-line">
      {opts.map((r) => (
        <button key={r ?? 'auto'} onClick={() => onChange(r)} className={`px-3 py-1.5 text-sm ${value === r ? 'bg-accent text-bg' : 'text-muted hover:bg-raised'}`}>
          {r ? ROLE_NAMES[r] : 'Most played'}
        </button>
      ))}
    </div>
  );
}

export function ChampionPicker({ value, onChange, placeholder = 'Search a champion…' }: { value: number | null; onChange: (id: number) => void; placeholder?: string }) {
  const dd = useDd();
  const [q, setQ] = useState('');
  const [open, setOpen] = useState(false);
  const list = useMemo(() => {
    const s = q.trim().toLowerCase();
    return dd.s.champions.filter((c) => !s || c.name.toLowerCase().includes(s) || c.key.toLowerCase().includes(s)).slice(0, 40);
  }, [q, dd]);
  return (
    <div className="relative w-72">
      <div className="flex items-center gap-2 rounded-md border border-line bg-bg px-2">
        {value != null && <ChampionIcon id={value} size={24} />}
        <input
          className="w-full bg-transparent py-1.5 text-sm text-fg outline-none placeholder:text-muted"
          placeholder={value != null ? dd.championName(value) : placeholder}
          value={q}
          onChange={(e) => { setQ(e.target.value); setOpen(true); }}
          onFocus={() => setOpen(true)}
          onBlur={() => setTimeout(() => setOpen(false), 150)}
          onKeyDown={(e) => { if (e.key === 'Enter' && list[0]) { onChange(list[0].id); setQ(''); setOpen(false); } }}
        />
      </div>
      {open && list.length > 0 && (
        <ul className="absolute z-20 mt-1 max-h-80 w-full overflow-auto rounded-md border border-line bg-panel shadow-xl">
          {list.map((c) => (
            <li key={c.id}>
              <button className="flex w-full items-center gap-2 px-2 py-1.5 text-left text-sm hover:bg-raised" onMouseDown={() => { onChange(c.id); setQ(''); setOpen(false); }}>
                <ChampionIcon id={c.id} size={24} /> {c.name}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** Requests Riot counted in the window for one routing value: ours, and another app's on the same key. */
export function BudgetMeter({ b }: { b: BudgetStatus }) {
  const m = meter(b);
  return (
    <div className="space-y-1">
      <div className="flex items-baseline justify-between text-sm">
        <span className="font-mono text-fg">{b.scope}</span>
        <span className="text-muted">
          {b.used} / {b.limit} in {b.windowS >= 60 ? `${b.windowS / 60} min` : `${b.windowS} s`}
          {b.shared && <span className="ml-2 rounded bg-warn/15 px-1.5 text-xs text-warn" title="Riot counted requests that weren't ours: another app (e.g. Irelia Companion) uses the same key">shared with another app</span>}
        </span>
      </div>
      <div className="flex h-2 overflow-hidden rounded bg-raised" role="meter" aria-valuenow={b.used} aria-valuemax={b.limit} aria-label={`${b.scope} requests`}>
        <div className="bg-accent" style={{ width: `${m.ours * 100}%` }} title={`ours: ${b.ours}`} />
        <div className="bg-warn" style={{ width: `${m.other * 100}%` }} title={`another app: ${b.used - b.ours}`} />
      </div>
      <div className="text-[11px] text-muted">limits {b.limits || '—'}</div>
    </div>
  );
}

export function ErrorText({ children }: { children: ReactNode }) {
  return <p className="rounded-md border border-bad/30 bg-bad/10 p-2 text-sm text-bad">{children}</p>;
}

export function Toggle({ on, onChange, label, hint }: { on: boolean; onChange: (v: boolean) => void; label: string; hint?: string }) {
  return (
    <label className="flex cursor-pointer items-start gap-3">
      <input type="checkbox" className="mt-1 accent-[var(--color-accent)]" checked={on} onChange={(e) => onChange(e.target.checked)} />
      <span>
        <span className="text-sm text-fg">{label}</span>
        {hint && <span className="block text-xs text-muted">{hint}</span>}
      </span>
    </label>
  );
}
