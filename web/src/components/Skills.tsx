import type { Option, SkillPlan } from '../../../shared/types.ts';
import { img, type Meta } from '../api.ts';
import { Card, Empty, WinStat } from './common.tsx';

export function SkillPriority({ meta, skills }: { meta: Meta; skills: Option<SkillPlan> | null }) {
  const spell = (k: string) => meta.spells.find((s) => s.key === k)!;
  return (
    <Card title="Skill Priority" aside={skills && <WinStat winRate={skills.winRate} games={skills.games} />}>
      {!skills ? <Empty>No skill data yet.</Empty> : (
        <div className="skill-priority">
          {skills.value.priority.map((k, i) => (
            <div key={k} className="skill-prio-item">
              {i > 0 && <span className="chev">›</span>}
              <div className="skill-badge">
                <img src={img.spell(meta.version, spell(k).image)} width={40} height={40} alt={spell(k).name} title={spell(k).name} />
                <span className={`skill-key key-${k}`}>{k}</span>
              </div>
            </div>
          ))}
        </div>
      )}
    </Card>
  );
}

export function SkillPath({ meta, skills }: { meta: Meta; skills: Option<SkillPlan> | null }) {
  if (!skills) return null;
  const { path } = skills.value;
  return (
    <Card title="Skill Path" className="skill-path-card" aside={<span className="muted">Most frequent order, levels 1–18</span>}>
      <div className="skill-grid">
        {(['Q', 'W', 'E', 'R'] as const).map((k) => {
          const s = meta.spells.find((x) => x.key === k)!;
          return (
            <div className="skill-grid-row" key={k}>
              <div className="skill-grid-label">
                <img src={img.spell(meta.version, s.image)} width={26} height={26} alt="" />
                <span>{k}</span>
              </div>
              {path.map((p, lvl) => (
                <div key={lvl} className={`skill-cell ${p === k ? `on key-${k}` : ''}`}>{p === k ? lvl + 1 : ''}</div>
              ))}
            </div>
          );
        })}
      </div>
    </Card>
  );
}
