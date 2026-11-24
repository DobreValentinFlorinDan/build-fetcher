//! Port of `server/model/evalHistory.ts`: the Data tab's evaluation history, every saved report reduced to its
//! comparisons. Reads only the evals folder.

use serde_json::{json, Value};
use std::path::Path;

pub const HISTORY_LIMIT: usize = 30;

fn finite(v: &Value) -> Option<f64> {
    v.as_f64().filter(|x| x.is_finite())
}

fn point(delta: &Value, low: &Value, high: &Value, passed: bool, reason: &str) -> Value {
    let Some(d) = finite(delta) else { return Value::Null };
    let lo = finite(low).unwrap_or(f64::NAN);
    let hi = finite(high).unwrap_or(f64::NAN);
    json!({ "delta": irelia_num(d), "low": irelia_num(lo), "high": irelia_num(hi), "verdict": if passed { "better" } else if hi < 0.0 { "worse" } else { "unclear" }, "reason": reason })
}

fn irelia_num(x: f64) -> Value {
    crate::js::num(x)
}

fn is_update_name(file: &str) -> bool {
    file.strip_prefix("eval-").map(|r| r.strip_prefix("auto-").unwrap_or(r).starts_with("update-")).unwrap_or(false)
}

/// One saved report as the history shows it (EvalHistoryEntry as JSON); None when it can't be read as one.
pub fn history_entry(r: &Value, file: &str) -> Option<Value> {
    let at = finite(&r["evaluatedAt"])?;
    if !r["lift"].is_object() {
        return None;
    }
    let grouped = match &r["grouping"] {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    };
    if grouped || file.starts_with("eval-grouped-") {
        return None;
    }
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();
    let draft = point(&r["lift"]["delta"], &r["lift"]["low"], &r["lift"]["high"], r["gate"] == "passed", &s(&r["gateReason"]));
    if draft.is_null() {
        return None;
    }
    let m = &r["memory"];
    let memory = if m.is_object() && m["grid"].as_array().map(|g| !g.is_empty()).unwrap_or(false) {
        point(&m["full"]["delta"], &m["full"]["low"], &m["full"]["high"], m["gate"] == "passed", &s(&m["gateReason"]))
    } else {
        Value::Null
    };
    let c = &r["scoring"];
    let scoring = if c.is_object() && c["decisions"].as_array().map(|g| !g.is_empty()).unwrap_or(false) {
        point(&c["adjusted"]["delta"], &c["adjusted"]["low"], &c["adjusted"]["high"], c["gate"] == "passed", &s(&c["gateReason"]))
    } else {
        Value::Null
    };
    let sw = &r["swaps"];
    let swaps = if sw.is_object() && sw["situations"].as_f64().map(|n| n > 0.0).unwrap_or(false) {
        point(&sw["lift"]["delta"], &sw["lift"]["low"], &sw["lift"]["high"], sw["gate"] == "passed", &s(&sw["gateReason"]))
    } else {
        Value::Null
    };
    let u = &r["update"];
    let update = u.is_object() || is_update_name(file);
    let mut e = json!({ "at": irelia_num(at), "auto": file.starts_with("eval-auto-"), "update": update });
    if let Some(f) = finite(&u["fullAt"]).filter(|_| u.is_object()) {
        e["fullAt"] = irelia_num(f);
    }
    if let Some(n) = finite(&u["newGames"]).filter(|_| u.is_object()) {
        e["newGames"] = irelia_num(n);
    }
    e["file"] = json!(file);
    e["games"] = if r["games"].is_number() { r["games"].clone() } else { json!(0) };
    e["draft"] = draft;
    e["memory"] = memory;
    e["scoring"] = scoring;
    e["swaps"] = swaps;
    Some(e)
}

fn name_ok(f: &str) -> bool {
    let Some(stem) = f.strip_prefix("eval-").and_then(|r| r.strip_suffix(".json")) else { return false };
    let r = stem.strip_prefix("auto-").unwrap_or(stem);
    let r = r.strip_prefix("update-").unwrap_or(r);
    !r.is_empty() && r.chars().all(|c| c.is_ascii_digit() || c == '-')
}

/// The newest `limit` saved evaluations in `dir`, oldest first.
pub fn eval_history(dir: &Path, limit: usize) -> Vec<Value> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut entries: Vec<Value> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name_ok(&name) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        if let Some(h) = history_entry(&v, &name) {
            entries.push(h);
        }
    }
    entries.sort_by(|a, b| crate::engine::recommend::js_order(a["at"].as_f64().unwrap_or(0.0) - b["at"].as_f64().unwrap_or(0.0)));
    let n = entries.len();
    entries.into_iter().skip(n.saturating_sub(limit)).collect()
}

#[cfg(test)]
mod tests {
    //! Port of server/model/evalHistory.test.ts.
    use super::*;

    fn report(at: f64, lift: [f64; 3], gate: &str, memory: Option<(f64, f64, f64, &str)>, scoring: Option<(f64, f64, f64, &str)>) -> Value {
        let mut r = json!({
            "evaluatedAt": at, "games": 1000, "gate": gate, "gateReason": if gate == "passed" { "lift above zero" } else { "interval reaches zero" },
            "lift": { "draft": 0, "baseline": 0, "delta": lift[0], "low": lift[1], "high": lift[2] },
            "memory": match memory {
                Some(m) => json!({ "grid": [{ "decay": 0.75 }], "full": { "delta": m.0, "low": m.1, "high": m.2 }, "gate": m.3, "gateReason": "memory" }),
                None => json!({ "grid": [], "full": { "delta": 0, "low": null, "high": null }, "gate": "failed", "gateReason": "only 0 games from older patches" }),
            },
        });
        if let Some(c) = scoring {
            r["scoring"] = json!({ "decisions": [{ "key": "keystone" }], "adjusted": { "delta": c.0, "low": c.1, "high": c.2 }, "gate": c.3, "gateReason": "scoring" });
        }
        r
    }
    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }
    fn verdicts(h: &[Value], k: &str) -> Vec<Option<String>> {
        h.iter().map(|e| e[k]["verdict"].as_str().map(String::from)).collect()
    }

    #[test]
    fn oldest_first_with_verdicts() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path();
        write(dir, "eval-2026-10-03-10-00.json", &report(3.0, [0.01, 0.002, 0.02], "passed", Some((-0.01, -0.02, -0.001, "failed")), None).to_string());
        write(dir, "eval-auto-2026-10-01-10-00.json", &report(1.0, [0.005, -0.004, 0.012], "failed", None, None).to_string());
        write(dir, "eval-auto-2026-10-02-10-00.json", &report(2.0, [0.02, 0.01, 0.03], "failed", Some((0.004, -0.002, 0.01, "failed")), Some((0.01, 0.002, 0.02, "passed"))).to_string());
        write(dir, "eval-2026-10-04-10-00.txt", "the text report");
        write(dir, "eval-2026-10-05-10-00.json", "{ half-written");
        write(dir, "latest.json", &json!({ "evaluatedAt": 9, "gate": "passed" }).to_string());
        let h = eval_history(dir, HISTORY_LIMIT);
        let ids: Vec<(f64, bool, String)> = h.iter().map(|e| (e["at"].as_f64().unwrap(), e["auto"].as_bool().unwrap(), e["file"].as_str().unwrap().to_string())).collect();
        assert_eq!(ids, vec![(1.0, true, "eval-auto-2026-10-01-10-00.json".to_string()), (2.0, true, "eval-auto-2026-10-02-10-00.json".into()), (3.0, false, "eval-2026-10-03-10-00.json".into())]);
        assert_eq!(verdicts(&h, "draft"), vec![Some("unclear".into()), Some("unclear".into()), Some("better".into())]);
        assert_eq!(verdicts(&h, "memory"), vec![None, Some("unclear".into()), Some("worse".into())]);
        assert_eq!(verdicts(&h, "scoring"), vec![None, Some("better".into()), None]);
        assert_eq!(eval_history(dir, 2).iter().map(|e| e["at"].as_f64().unwrap()).collect::<Vec<_>>(), vec![2.0, 3.0]);
        assert!(eval_history(&dir.join("missing"), HISTORY_LIMIT).is_empty());
        assert!(h.iter().all(|e| e["update"] == false && e.get("fullAt").is_none()));
    }

    #[test]
    fn quick_updates_are_marked() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path();
        write(dir, "eval-auto-2026-10-21-10-00.json", &report(1.0, [0.01, 0.002, 0.02], "passed", None, None).to_string());
        let mut r2 = report(2.0, [0.011, 0.003, 0.02], "passed", None, None);
        r2["update"] = json!({ "fullAt": 1, "newGames": 312, "reason": "312 new games" });
        write(dir, "eval-auto-update-2026-10-21-12-00.json", &r2.to_string());
        let mut r3 = report(3.0, [0.012, 0.004, 0.021], "passed", None, None);
        r3["update"] = json!({ "fullAt": 1, "newGames": 40, "reason": "40 new games" });
        write(dir, "eval-update-2026-10-21-14-00.json", &r3.to_string());
        write(dir, "eval-update-2026-10-21-15-00.json", &report(4.0, [0.012, 0.004, 0.021], "passed", None, None).to_string());
        write(dir, "eval-quick-2026-10-21-16-00.json", &report(5.0, [0.0, 0.0, 0.0], "failed", None, None).to_string());
        let h = eval_history(dir, HISTORY_LIMIT);
        let got: Vec<Value> = h.iter().map(|e| json!([e["at"], e["auto"], e["update"], e.get("fullAt").cloned().unwrap_or(Value::Null), e.get("newGames").cloned().unwrap_or(Value::Null)])).collect();
        assert_eq!(got, vec![json!([1, true, false, null, null]), json!([2, true, true, 1, 312]), json!([3, false, true, 1, 40]), json!([4, false, true, null, null])]);
    }

    #[test]
    fn late_game_swaps_are_a_point() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path();
        let swaps = |delta: f64, low: Option<f64>, high: Option<f64>, gate: &str, situations: f64| {
            json!({ "lift": { "advice": 0.5 + delta, "keep": 0.5, "delta": delta, "low": low, "high": high }, "situations": situations, "games": 400, "swapped": 0.2, "advised": 0.15, "gate": gate, "gateReason": format!("swaps {gate}") })
        };
        let base = |at: f64| report(at, [0.01, 0.002, 0.02], "passed", None, None);
        write(dir, "eval-2026-10-20-10-00.json", &base(1.0).to_string());
        let mut r = base(2.0);
        r["swaps"] = swaps(0.012, Some(0.003), Some(0.021), "passed", 900.0);
        write(dir, "eval-2026-10-21-10-00.json", &r.to_string());
        let mut r = base(3.0);
        r["swaps"] = swaps(0.004, Some(-0.003), Some(0.011), "failed", 900.0);
        write(dir, "eval-2026-10-22-10-00.json", &r.to_string());
        let mut r = base(4.0);
        r["swaps"] = swaps(-0.01, Some(-0.02), Some(-0.002), "failed", 900.0);
        write(dir, "eval-2026-10-23-10-00.json", &r.to_string());
        let mut r = base(5.0);
        r["swaps"] = Value::Null;
        write(dir, "eval-2026-10-24-10-00.json", &r.to_string());
        let mut r = base(6.0);
        r["swaps"] = swaps(0.0, None, None, "failed", 0.0);
        write(dir, "eval-2026-10-25-10-00.json", &r.to_string());
        let h = eval_history(dir, HISTORY_LIMIT);
        assert_eq!(verdicts(&h, "swaps"), vec![None, Some("better".into()), Some("unclear".into()), Some("worse".into()), None, None]);
        assert_eq!(h[1]["swaps"], json!({ "delta": 0.012, "low": 0.003, "high": 0.021, "verdict": "better", "reason": "swaps passed" }));
    }
}
