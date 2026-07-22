//! Intervention prediction (N04) — will the operator escalate at this point?
//!
//! `GET /api/interventions/predict?convergence=<f64>&round=<u32>` trains a tiny
//! logistic-regression model **at request time** from
//! `sessions/intervention_log.jsonl`, using features `[convergence, round]` to
//! predict the escalate-class interventions (`escalate_specops`,
//! `escalate_munger`, `escalate_contrarian`, `escalate_kiss`). Training is
//! cheap (a few hundred gradient steps over a handful of rows).
//!
//! With < 30 usable samples the logistic fit is unreliable, so we fall back to
//! the overall escalation **frequency** and report `method: "frequency"`.
//! Malformed log lines are skipped.

use serde_json::{Value, json};

use super::intervention_log;

const MIN_SAMPLES_FOR_LOGREG: usize = 30;
const ESCALATE_ACTIONS: &[&str] = &[
    "escalate_specops",
    "escalate_munger",
    "escalate_contrarian",
    "escalate_kiss",
];

/// One training row: features + binary label (1 = escalate-class).
#[derive(Debug, Clone, Copy)]
struct Sample {
    convergence: f64,
    round: f64,
    escalated: bool,
}

/// Build the prediction response for the given query point.
pub fn predict(convergence: f64, round: u32) -> Value {
    let samples = load_samples(&intervention_log::load_all(None));
    predict_from_samples(&samples, convergence, round)
}

/// Pure core: train + score against an explicit sample set (testable without
/// touching disk).
fn predict_from_samples(samples: &[Sample], convergence: f64, round: u32) -> Value {
    let n = samples.len();
    if n < MIN_SAMPLES_FOR_LOGREG {
        let escalations = samples.iter().filter(|s| s.escalated).count();
        let probability = if n == 0 {
            0.0
        } else {
            escalations as f64 / n as f64
        };
        return json!({
            "probability": round6(probability),
            "method": "frequency",
            "n_samples": n,
        });
    }

    let model = train_logreg(samples);
    let probability = model.predict(convergence, round as f64);
    json!({
        "probability": round6(probability),
        "method": "logreg",
        "n_samples": n,
    })
}

/// Parse intervention-log entries into training samples. Each PAUSE row is one
/// sample; its label is whether the recorded action was an escalation. Rows
/// missing `convergence_at_pause` or `round_num` are skipped (can't featurize).
fn load_samples(entries: &[Value]) -> Vec<Sample> {
    entries
        .iter()
        .filter_map(|e| {
            let convergence = e.get("convergence_at_pause").and_then(|x| x.as_f64())?;
            let round = e
                .get("round_num")
                .and_then(|x| x.as_f64().or_else(|| x.as_i64().map(|i| i as f64)))?;
            let action = e.get("action").and_then(|x| x.as_str()).unwrap_or("");
            Some(Sample {
                convergence,
                round,
                escalated: ESCALATE_ACTIONS.contains(&action),
            })
        })
        .collect()
}

/// A 2-feature logistic-regression model with standardized inputs.
struct LogReg {
    w_conv: f64,
    w_round: f64,
    bias: f64,
    conv_mean: f64,
    conv_std: f64,
    round_mean: f64,
    round_std: f64,
}

impl LogReg {
    fn predict(&self, convergence: f64, round: f64) -> f64 {
        let xc = (convergence - self.conv_mean) / self.conv_std;
        let xr = (round - self.round_mean) / self.round_std;
        sigmoid(self.bias + self.w_conv * xc + self.w_round * xr)
    }
}

fn sigmoid(z: f64) -> f64 {
    1.0 / (1.0 + (-z).exp())
}

/// Standardize features, then run fixed-iteration batch gradient descent.
fn train_logreg(samples: &[Sample]) -> LogReg {
    let n = samples.len() as f64;

    let conv_mean = samples.iter().map(|s| s.convergence).sum::<f64>() / n;
    let round_mean = samples.iter().map(|s| s.round).sum::<f64>() / n;
    let conv_std = std_dev(samples.iter().map(|s| s.convergence), conv_mean, n);
    let round_std = std_dev(samples.iter().map(|s| s.round), round_mean, n);

    // Pre-standardize.
    let xs: Vec<(f64, f64, f64)> = samples
        .iter()
        .map(|s| {
            let xc = (s.convergence - conv_mean) / conv_std;
            let xr = (s.round - round_mean) / round_std;
            let y = if s.escalated { 1.0 } else { 0.0 };
            (xc, xr, y)
        })
        .collect();

    let mut w_conv = 0.0;
    let mut w_round = 0.0;
    let mut bias = 0.0;
    let lr = 0.1;
    let l2 = 1e-4;

    for _ in 0..2000 {
        let mut g_conv = 0.0;
        let mut g_round = 0.0;
        let mut g_bias = 0.0;
        for &(xc, xr, y) in &xs {
            let pred = sigmoid(bias + w_conv * xc + w_round * xr);
            let err = pred - y;
            g_conv += err * xc;
            g_round += err * xr;
            g_bias += err;
        }
        w_conv -= lr * (g_conv / n + l2 * w_conv);
        w_round -= lr * (g_round / n + l2 * w_round);
        bias -= lr * (g_bias / n);
    }

    LogReg {
        w_conv,
        w_round,
        bias,
        conv_mean,
        conv_std,
        round_mean,
        round_std,
    }
}

/// Population std-dev with a floor so standardization never divides by ~0
/// (constant feature → std 1.0, which maps every value to 0 after centering).
fn std_dev(it: impl Iterator<Item = f64>, mean: f64, n: f64) -> f64 {
    let var = it.map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    let sd = var.sqrt();
    if sd < 1e-9 { 1.0 } else { sd }
}

fn round6(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(convergence: f64, round: f64, escalated: bool) -> Sample {
        Sample {
            convergence,
            round,
            escalated,
        }
    }

    #[test]
    fn frequency_fallback_under_min_samples() {
        // 3 samples, 1 escalation → frequency 1/3.
        let samples = vec![
            sample(0.5, 1.0, true),
            sample(0.9, 2.0, false),
            sample(0.8, 1.0, false),
        ];
        let out = predict_from_samples(&samples, 0.4, 1);
        assert_eq!(out["method"], "frequency");
        assert_eq!(out["n_samples"], 3);
        let p = out["probability"].as_f64().unwrap();
        assert!((p - 1.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn frequency_fallback_zero_samples_is_zero() {
        let out = predict_from_samples(&[], 0.4, 1);
        assert_eq!(out["method"], "frequency");
        assert_eq!(out["n_samples"], 0);
        assert_eq!(out["probability"], 0.0);
    }

    #[test]
    fn logreg_learns_a_separable_toy_set() {
        // Build 40 samples where LOW convergence → escalate, HIGH → don't.
        // The logreg should put a high escalate-probability on a low-conv query
        // and a low one on a high-conv query.
        let mut samples = Vec::new();
        for i in 0..20 {
            let round = (i % 3 + 1) as f64;
            // Low convergence cohort → escalate.
            samples.push(sample(0.1 + (i as f64) * 0.005, round, true));
            // High convergence cohort → no escalate.
            samples.push(sample(0.85 + (i as f64) * 0.005, round, false));
        }
        assert!(samples.len() >= MIN_SAMPLES_FOR_LOGREG);

        let out_low = predict_from_samples(&samples, 0.1, 2);
        assert_eq!(out_low["method"], "logreg");
        assert_eq!(out_low["n_samples"], 40);
        let p_low = out_low["probability"].as_f64().unwrap();

        let out_high = predict_from_samples(&samples, 0.9, 2);
        let p_high = out_high["probability"].as_f64().unwrap();

        assert!(
            p_low > 0.6,
            "low convergence should predict escalation, got {p_low}"
        );
        assert!(
            p_high < 0.4,
            "high convergence should predict no escalation, got {p_high}"
        );
        assert!(p_low > p_high, "monotonic in the learned direction");
    }

    #[test]
    fn load_samples_skips_malformed_and_labels_escalations() {
        let entries = vec![
            json!({"action": "escalate_specops", "round_num": 1, "convergence_at_pause": 0.5}),
            json!({"action": "continue", "round_num": 2, "convergence_at_pause": 0.8}),
            // Missing convergence — skipped.
            json!({"action": "escalate_munger", "round_num": 1}),
            // Garbage — skipped.
            json!({"foo": "bar"}),
        ];
        let samples = load_samples(&entries);
        assert_eq!(samples.len(), 2, "two featurizable rows");
        assert!(samples[0].escalated);
        assert!(!samples[1].escalated);
    }
}
