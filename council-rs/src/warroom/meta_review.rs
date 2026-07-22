//! Meta-review of the self-audit loop — reads weekly summaries + intervention
//! log, assesses signal quality, recommends ONE parameter to tune.
//! Writes runs/meta_review_YYYYMMDD.md via Tera template.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::PathBuf;

use chrono::Utc;
use serde_json::{Value, json};

use super::{runs_dir, sessions_dir};

fn lock_path() -> PathBuf {
    sessions_dir().join("meta_review.lock")
}

pub fn is_running() -> bool {
    lock_path().exists()
}

pub fn acquire_lock() -> bool {
    let path = lock_path();
    if path.exists() {
        return false;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, Utc::now().to_rfc3339()).is_ok()
}

pub fn release_lock() {
    let _ = std::fs::remove_file(lock_path());
}

fn load_jsonl(path: &std::path::Path) -> Vec<Value> {
    if !path.exists() {
        return vec![];
    }
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

/// Load all weekly_drift_*.json files from runs/ as the summary corpus.
/// Rust drift.rs writes per-day files (not the Python weekly_summaries.jsonl),
/// so we glob those instead.
fn load_weekly_summaries() -> Vec<Value> {
    let dir = runs_dir();
    if !dir.exists() {
        return vec![];
    }
    let mut entries: Vec<_> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("weekly_drift_") && n.ends_with(".json"))
            })
            .collect(),
        Err(_) => return vec![],
    };
    entries.sort_by_key(|a| a.file_name());
    entries
        .iter()
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|c| serde_json::from_str(&c).ok())
        .collect()
}

struct SignalStats {
    weeks: usize,
    mean: f64,
    stdev: f64,
    cv: f64,
    slope: f64,
    trend: String,
    stability: String,
    flips_total: u64,
}

fn signal_stats(summaries: &[Value]) -> Option<SignalStats> {
    let drifts: Vec<f64> = summaries
        .iter()
        .filter_map(|s| s.get("avg_drift").and_then(|v| v.as_f64()))
        .collect();
    let flips: Vec<u64> = summaries
        .iter()
        .filter_map(|s| s.get("confidence_flips").and_then(|v| v.as_u64()))
        .collect();
    if drifts.is_empty() {
        return None;
    }

    let n = drifts.len();
    let mean = drifts.iter().sum::<f64>() / n as f64;
    let variance = if n > 1 {
        drifts.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / (n - 1) as f64
    } else {
        0.0
    };
    let stdev = variance.sqrt();
    let cv = if mean > 0.0 { stdev / mean } else { 0.0 };

    let slope = if n > 2 {
        let x_mean = (n - 1) as f64 / 2.0;
        let denom: f64 = (0..n).map(|i| (i as f64 - x_mean).powi(2)).sum();
        if denom > 0.0 {
            (0..n)
                .map(|i| (i as f64 - x_mean) * (drifts[i] - mean))
                .sum::<f64>()
                / denom
        } else {
            0.0
        }
    } else {
        0.0
    };

    let trend = if slope > 0.01 {
        "rising"
    } else if slope < -0.01 {
        "falling"
    } else {
        "flat"
    };
    let stability = if cv < 0.3 {
        "stable"
    } else if cv < 0.6 {
        "moderate noise"
    } else {
        "noisy"
    };

    Some(SignalStats {
        weeks: summaries.len(),
        mean,
        stdev,
        cv,
        slope,
        trend: trend.to_string(),
        stability: stability.to_string(),
        flips_total: flips.iter().sum(),
    })
}

fn flips_by_cabinet(summaries: &[Value], index_meta: &HashMap<String, Value>) -> Vec<Value> {
    let mut by_cab: HashMap<String, u64> = HashMap::new();
    for s in summaries {
        let h = match s.get("headline_session") {
            Some(v) => v,
            None => continue,
        };
        let sid = match h.get("session_id").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => continue,
        };
        if h.get("confidence_changed").and_then(|v| v.as_bool()) != Some(true) {
            continue;
        }
        let cab = index_meta
            .get(sid)
            .and_then(|m| m.get("cabinet").and_then(|v| v.as_str()))
            .unwrap_or("unknown");
        *by_cab.entry(cab.to_string()).or_default() += 1;
    }
    let mut sorted: Vec<_> = by_cab.into_iter().collect();
    sorted.sort_by_key(|x| std::cmp::Reverse(x.1));
    sorted.truncate(5);
    sorted
        .into_iter()
        .map(|(cab, count)| json!({"cabinet": cab, "count": count}))
        .collect()
}

fn anchoring_history(summaries: &[Value]) -> Vec<Value> {
    let mut keyword_scores: HashMap<String, Vec<f64>> = HashMap::new();
    for s in summaries {
        let anchors = match s.get("top_anchoring").and_then(|v| v.as_array()) {
            Some(a) => a,
            None => continue,
        };
        for entry in anchors {
            let kw = match entry.get("keyword").and_then(|v| v.as_str()) {
                Some(k) => k,
                None => continue,
            };
            let score = entry
                .get("score")
                .or_else(|| entry.get("avg_drift"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            keyword_scores
                .entry(kw.to_string())
                .or_default()
                .push(score);
        }
    }
    let mut ranked: Vec<_> = keyword_scores
        .into_iter()
        .map(|(kw, scores)| {
            let avg = scores.iter().sum::<f64>() / scores.len() as f64;
            json!({
                "keyword": kw,
                "weeks": scores.len(),
                "avg_drift": format!("{:.3}", avg),
            })
        })
        .collect();
    ranked.sort_by(|a, b| {
        let aw = a.get("weeks").and_then(|v| v.as_u64()).unwrap_or(0);
        let bw = b.get("weeks").and_then(|v| v.as_u64()).unwrap_or(0);
        bw.cmp(&aw)
    });
    ranked.truncate(5);
    ranked
}

struct InterventionStats {
    total: usize,
    weeks: usize,
    avg_conv: Option<f64>,
    action_mix: String,
    trend: String,
    early: String,
    recent: String,
}

fn intervention_stats(interventions: &[Value]) -> InterventionStats {
    if interventions.is_empty() {
        return InterventionStats {
            total: 0,
            weeks: 0,
            avg_conv: None,
            action_mix: String::new(),
            trend: "n/a".into(),
            early: "0".into(),
            recent: "0".into(),
        };
    }

    let mut by_week: HashMap<String, usize> = HashMap::new();
    for i in interventions {
        let ts = i
            .get("ts")
            .or_else(|| i.get("logged_at"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if ts.len() < 10 {
            continue;
        }
        let week_key = &ts[..10];
        *by_week.entry(week_key.to_string()).or_default() += 1;
    }

    let convs: Vec<f64> = interventions
        .iter()
        .filter_map(|i| i.get("convergence_at_pause").and_then(|v| v.as_f64()))
        .collect();
    let avg_conv = if convs.is_empty() {
        None
    } else {
        Some(convs.iter().sum::<f64>() / convs.len() as f64)
    };

    let mut actions: HashMap<String, usize> = HashMap::new();
    for i in interventions {
        let a = i.get("action").and_then(|v| v.as_str()).unwrap_or("?");
        *actions.entry(a.to_string()).or_default() += 1;
    }
    let mut action_vec: Vec<_> = actions.into_iter().collect();
    action_vec.sort_by_key(|x| std::cmp::Reverse(x.1));
    let action_mix = action_vec
        .iter()
        .map(|(a, c)| format!("{}={}", a, c))
        .collect::<Vec<_>>()
        .join(", ");

    let weeks = by_week.len();
    let mut week_counts: Vec<usize> = by_week.values().copied().collect();
    week_counts.sort();

    let (trend, early_s, recent_s) = if week_counts.len() >= 4 {
        let half = week_counts.len() / 2;
        let early = week_counts[..half].iter().sum::<usize>() as f64 / half.max(1) as f64;
        let recent = week_counts[half..].iter().sum::<usize>() as f64
            / (week_counts.len() - half).max(1) as f64;
        let delta = recent - early;
        let t = if delta > 0.5 {
            "rising"
        } else if delta < -0.5 {
            "falling"
        } else {
            "flat"
        };
        (
            t.to_string(),
            format!("{:.1}", early),
            format!("{:.1}", recent),
        )
    } else {
        ("n/a".into(), "0".into(), "0".into())
    };

    InterventionStats {
        total: interventions.len(),
        weeks,
        avg_conv,
        action_mix,
        trend,
        early: early_s,
        recent: recent_s,
    }
}

fn recommend(stats: &SignalStats, flips: &[Value], interv: &InterventionStats) -> String {
    if stats.mean > 0.4 {
        return format!(
            "**Lower the precedent similarity threshold** (currently 0.30).\n\n\
            Reason: average drift is **{:.3}** — precedent injection is moving verdicts substantially. \
            Try raising the threshold to 0.45 to admit only stronger semantic matches and reduce \
            weak-anchor noise. Re-evaluate after 2 more weeks.",
            stats.mean
        );
    }
    if let Some(avg) = interv.avg_conv
        && avg > 0.78
    {
        return format!(
            "**Raise the auto-specops convergence threshold** (currently 0.8).\n\n\
                Reason: users intervene at avg convergence **{:.3}** — they distrust the council \
                *before* it hits the auto-escalation gate. Raising the threshold to 0.85 fires \
                SpecOps earlier on borderline cases and may reduce manual intervention pressure.",
            avg
        );
    }
    let total_flips: u64 = flips
        .iter()
        .filter_map(|f| f.get("count").and_then(|v| v.as_u64()))
        .sum();
    if total_flips > 0 && !flips.is_empty() {
        let top_cab = flips[0]
            .get("cabinet")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let top_count = flips[0].get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        if (top_count as f64 / total_flips as f64) > 0.6 {
            return format!(
                "**Increase weekly_drift `--limit`** from 8 → 16 to oversample.\n\n\
                Reason: {}/{} confidence flips concentrate in **{}** (>60%). More sessions per \
                week deepens the signal in that cabinet specifically.",
                top_count, total_flips, top_cab
            );
        }
    }
    if stats.stability == "stable" {
        return format!(
            "**Widen weekly_drift `--window`** from 7 → 14 days.\n\n\
            Reason: signal is stable (CV={:.2}) — short windows are noisy without informational \
            gain. A 14-day window halves bi-weekly sampling rate while preserving trend visibility.",
            stats.cv
        );
    }
    format!(
        "**Tighten weekly_drift `--window`** from 7 → 5 days.\n\n\
        Reason: signal is noisy (CV={:.2}). Shorter windows isolate which week's sessions \
        drove the noise spike.",
        stats.cv
    )
}

/// Run the meta-review. Returns JSON with the report path or an error.
pub fn run(tera: Option<&tera::Tera>) -> Value {
    let interventions_path = sessions_dir().join("intervention_log.jsonl");
    let index_path = sessions_dir().join("index.jsonl");

    let summaries = load_weekly_summaries();
    let interventions = load_jsonl(&interventions_path);
    let index_entries = load_jsonl(&index_path);

    let index_meta: HashMap<String, Value> = index_entries
        .into_iter()
        .filter_map(|v| {
            let id = v
                .get("session_id")
                .or_else(|| v.get("id"))
                .and_then(|x| x.as_str())
                .map(String::from)?;
            Some((id, v))
        })
        .collect();

    let today = Utc::now().format("%Y%m%d").to_string();
    let out_path = runs_dir().join(format!("meta_review_{}.md", today));
    let _ = std::fs::create_dir_all(runs_dir());

    if summaries.len() < 2 {
        let body = format!(
            "# Meta-review {} — insufficient data\n\n\
            Found **{}** weekly drift summaries. Need at least 2 to assess signal trends.\n\n\
            Source: `runs/weekly_drift_*.json`\n",
            today,
            summaries.len()
        );
        let _ = std::fs::write(&out_path, &body);
        return json!({
            "report_path": out_path.to_string_lossy(),
            "status": "insufficient_data",
            "weeks": summaries.len(),
        });
    }

    let stats = match signal_stats(&summaries) {
        Some(s) => s,
        None => return json!({"status": "no_drift_data"}),
    };
    let flips = flips_by_cabinet(&summaries, &index_meta);
    let anchors = anchoring_history(&summaries);
    let interv = intervention_stats(&interventions);
    let rec = recommend(&stats, &flips, &interv);

    let body = if let Some(t) = tera {
        let mut ctx = tera::Context::new();
        ctx.insert("today", &today);
        ctx.insert(
            "stats",
            &json!({
                "weeks": stats.weeks,
                "mean": format!("{:.3}", stats.mean),
                "stdev": format!("{:.3}", stats.stdev),
                "cv": format!("{:.2}", stats.cv),
                "slope": format!("{:+.4}", stats.slope),
                "trend": stats.trend,
                "stability": stats.stability,
                "flips_total": stats.flips_total,
            }),
        );
        ctx.insert("flips", &flips);
        ctx.insert("anchors", &anchors);
        ctx.insert(
            "interventions",
            &json!({
                "total": interv.total,
                "weeks": interv.weeks,
                "avg_conv": interv.avg_conv.map(|v| format!("{:.3}", v)),
                "action_mix": interv.action_mix,
                "trend": interv.trend,
                "early": interv.early,
                "recent": interv.recent,
            }),
        );
        ctx.insert("recommendation", &rec);
        t.render("meta_review.tera", &ctx).unwrap_or_else(|e| {
            eprintln!("[meta_review] template render failed: {}", e);
            format!(
                "# Meta-review of the self-audit loop — {}\n\n{}\n",
                today, rec
            )
        })
    } else {
        format!(
            "# Meta-review of the self-audit loop — {}\n\n{}\n",
            today, rec
        )
    };

    if let Err(e) = std::fs::write(&out_path, &body) {
        return json!({
            "status": "write_failed",
            "error": format!("failed to write meta-review: {}", e),
        });
    }

    json!({
        "report_path": out_path.to_string_lossy(),
        "status": "complete",
        "weeks": stats.weeks,
        "mean_drift": (stats.mean * 1000.0).round() / 1000.0,
        "stability": stats.stability,
        "recommendation_preview": rec.chars().take(200).collect::<String>(),
    })
}

/// Read the latest meta-review report.
pub fn latest() -> Option<Value> {
    let dir = runs_dir();
    if !dir.exists() {
        return None;
    }
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("meta_review_") && n.ends_with(".md"))
        })
        .collect();
    files.sort_by_key(|x| std::cmp::Reverse(x.file_name()));
    let path = files.first()?.path();
    let content = std::fs::read_to_string(&path).ok()?;
    Some(json!({
        "name": path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
        "content": content,
        "mtime": super::drift::iso_mtime_pub(&path),
    }))
}
