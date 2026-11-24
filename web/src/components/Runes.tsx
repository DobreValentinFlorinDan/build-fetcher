import type { Option, RunePage } from '../../../shared/types.ts';
import { img, type Meta, type RuneTree } from '../api.ts';
import { Card, Empty, WinStat } from './common.tsx';

export const SHARDS: Record<number, { name: string; icon: string }> = {
  5008: { name: 'Adaptive Force', icon: 'StatModsAdaptiveForceIcon.png' },
  5005: { name: 'Attack Speed', icon: 'StatModsAttackSpeedIcon.png' },
  5007: { name: 'Ability Haste', icon: 'StatModsCDRScalingIcon.png' },
  5010: { name: 'Move Speed', icon: 'StatModsMovementSpeedIcon.png' },
  5001: { name: 'Health Scaling', icon: 'StatModsHealthScalingIcon.png' },
  5011: { name: 'Health', icon: 'StatModsHealthPlusIcon.png' },
  5013: { name: 'Tenacity & Slow Resist', icon: 'StatModsTenacityIcon.png' },
};
const SHARD_ROWS = [[5008, 5005, 5007], [5008, 5010, 5001], [5011, 5013, 5001]];

function Tree({ tree, selected, skipKeystone }: { tree: RuneTree; selected: number[]; skipKeystone?: boolean }) {
  return (
    <div className="rune-tree">
      <div className="tree-title">
        <img src={img.rune(tree.icon)} width={28} height={28} alt="" />
        <span>{tree.name}</span>
      </div>
      {tree.slots.map((slot, i) =>
        skipKeystone && i === 0 ? null : (
          <div key={i} className={`rune-row ${i === 0 ? 'keystones' : ''}`}>
            {slot.runes.map((r) => (
              <img
                key={r.id}
                className={`rune ${selected.includes(r.id) ? 'on' : 'off'}`}
                src={img.rune(r.icon)}
                width={i === 0 ? 46 : 32}
                height={i === 0 ? 46 : 32}
                alt={r.name}
                title={`${r.name}\n${r.shortDesc.replace(/<[^>]+>/g, '')}`}
              />
            ))}
          </div>
        ),
      )}
    </div>
  );
}

export function Runes({ meta, runes }: { meta: Meta; runes: Option<RunePage> | null }) {
  if (!runes) return <Card title="Runes"><Empty>No rune data on this patch yet — start the fetcher.</Empty></Card>;
  const page = runes.value;
  const primary = meta.runes.find((t) => t.id === page.primaryStyle);
  const secondary = meta.runes.find((t) => t.id === page.subStyle);
  const keystone = primary?.slots[0].runes.find((r) => page.perks.includes(r.id));

  return (
    <Card
      title="Runes"
      className="runes-card"
      aside={<>{keystone && <span className="pill">{keystone.name}</span>}<WinStat winRate={runes.winRate} games={runes.games} /></>}
    >
      <div className="rune-layout">
        {primary && <Tree tree={primary} selected={page.perks.slice(0, 4)} />}
        <div className="rune-col">
          {secondary && <Tree tree={secondary} selected={page.perks.slice(4)} skipKeystone />}
          <div className="shards">
            {SHARD_ROWS.map((row, i) => (
              <div key={i} className="rune-row shard-row">
                {row.map((id, j) => (
                  <img
                    key={j}
                    className={`rune shard ${page.statPerks[i] === id ? 'on' : 'off'}`}
                    src={img.rune(`perk-images/StatMods/${SHARDS[id].icon}`)}
                    width={24}
                    height={24}
                    alt={SHARDS[id].name}
                    title={SHARDS[id].name}
                  />
                ))}
              </div>
            ))}
          </div>
        </div>
      </div>
    </Card>
  );
}
