use std::fmt::Write as _;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const TARGET_LATENCY_MS: f64 = 150.0;

#[derive(Debug, Clone, Deserialize)]
pub struct ValidatorSample {
    pub identity: String,
    pub uptime: f64,
    #[serde(default)]
    pub avg_latency_ms: f64,
    #[serde(default)]
    pub finality_faults: u32,
    #[serde(default)]
    pub missed_slots: u32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ScorecardEntry {
    pub identity: String,
    pub score: f64,
    pub grade: String,
    pub uptime: f64,
    pub avg_latency_ms: f64,
    pub finality_faults: u32,
    pub missed_slots: u32,
}

#[derive(Debug, Error)]
pub enum ScorecardError {
    #[error("no validator samples provided")]
    Empty,
}

pub fn load_samples(json: &str) -> Result<Vec<ValidatorSample>> {
    let samples: Vec<ValidatorSample> = serde_json::from_str(json)?;
    Ok(samples)
}

pub fn compute_score(sample: &ValidatorSample) -> f64 {
    let mut score = 100.0;

    let uptime_gap = (100.0 - sample.uptime).max(0.0);
    score -= uptime_gap * 0.6;

    if sample.avg_latency_ms > TARGET_LATENCY_MS {
        let latency_over = sample.avg_latency_ms - TARGET_LATENCY_MS;
        let penalty = (latency_over / TARGET_LATENCY_MS) * 25.0;
        score -= penalty;
    }

    score -= sample.finality_faults as f64 * 6.0;
    score -= sample.missed_slots as f64 * 0.4;

    score.clamp(0.0, 100.0)
}

fn letter_grade(score: f64) -> String {
    if score >= 90.0 {
        "A".into()
    } else if score >= 75.0 {
        "B".into()
    } else if score >= 60.0 {
        "C".into()
    } else {
        "D".into()
    }
}

pub fn generate_scorecard(samples: &[ValidatorSample]) -> Result<Vec<ScorecardEntry>> {
    if samples.is_empty() {
        return Err(ScorecardError::Empty.into());
    }

    let mut entries: Vec<ScorecardEntry> = samples
        .iter()
        .map(|sample| {
            let score = compute_score(sample);
            ScorecardEntry {
                identity: sample.identity.clone(),
                score,
                grade: letter_grade(score),
                uptime: sample.uptime,
                avg_latency_ms: sample.avg_latency_ms,
                finality_faults: sample.finality_faults,
                missed_slots: sample.missed_slots,
            }
        })
        .collect();

    entries.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    Ok(entries)
}

pub fn render_markdown(entries: &[ScorecardEntry]) -> String {
    let mut out = String::new();
    out.push_str("| Rank | Validator | Score | Grade | Uptime | Latency (ms) | Faults | Missed |");
    out.push('\n');
    out.push_str("|------|-----------|-------|-------|--------|--------------|--------|--------|");
    out.push('\n');

    for (idx, entry) in entries.iter().enumerate() {
        let _ = writeln!(
            out,
            "| {} | {} | {:.1} | {} | {:.2}% | {:.1} | {} | {} |",
            idx + 1,
            entry.identity,
            entry.score,
            entry.grade,
            entry.uptime,
            entry.avg_latency_ms,
            entry.finality_faults,
            entry.missed_slots
        );
    }

    out
}

pub fn render_csv(entries: &[ScorecardEntry]) -> String {
    let mut out =
        String::from("rank,validator,score,grade,uptime,latency_ms,finality_faults,missed_slots\n");
    for (idx, entry) in entries.iter().enumerate() {
        let _ = writeln!(
            out,
            "{},{},{:.2},{},{:.4},{:.2},{},{}",
            idx + 1,
            entry.identity,
            entry.score,
            entry.grade,
            entry.uptime,
            entry.avg_latency_ms,
            entry.finality_faults,
            entry.missed_slots
        );
    }
    out
}
