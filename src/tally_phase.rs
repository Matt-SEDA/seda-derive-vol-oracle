use anyhow::Result;
use seda_sdk_rs::{elog, get_reveals, log, Process};
use serde::Serialize;

use crate::execution_phase::ExecutionResult;

/// Derive Volatility Surface Oracle — Tally Phase
///
/// Aggregates vol data from multiple executors into a consensus vol surface.

const REGIME_LOW: f64 = 30.0;
const REGIME_ELEVATED: f64 = 60.0;
const REGIME_CRISIS: f64 = 80.0;

#[derive(Serialize)]
struct VolSurface {
    /// Currency
    cy: String,
    /// Consensus spot (USD)
    spot: f64,
    /// DVOL index (%)
    dvol: f64,
    /// Realized vol (%)
    rv: f64,
    /// Vol risk premium: DVOL - RV
    vrp: f64,
    /// ATM IV: average of call + put IV (%)
    atm: f64,
    /// ATM call IV (%)
    ac: f64,
    /// ATM put IV (%)
    ap: f64,
    /// Put-call IV spread (put IV - call IV, positive = put premium)
    skew: f64,
    /// ATM call delta
    cd: f64,
    /// ATM put delta
    pd: f64,
    /// ATM gamma
    gm: f64,
    /// ATM vega
    vg: f64,
    /// Vol regime
    regime: String,
    /// Regime score 0-100
    rscore: u8,
    /// Spot sources used
    src: usize,
    /// Total executors
    ex: usize,
    /// Valid reveals
    ok: usize,
}

fn median_f64(vals: &mut Vec<f64>) -> f64 {
    if vals.is_empty() { return 0.0; }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let len = vals.len();
    if len % 2 == 0 { (vals[len / 2 - 1] + vals[len / 2]) / 2.0 } else { vals[len / 2] }
}

fn median_u64(vals: &mut Vec<u64>) -> u64 {
    if vals.is_empty() { return 0; }
    vals.sort();
    let len = vals.len();
    if len % 2 == 0 { (vals[len / 2 - 1] + vals[len / 2]) / 2 } else { vals[len / 2] }
}

fn classify_regime(dvol: f64, rv: f64) -> (String, u8) {
    let vol_level = dvol.max(rv);
    if vol_level < REGIME_LOW {
        let score = ((vol_level / REGIME_LOW) * 25.0).round() as u8;
        ("LOW".to_string(), score)
    } else if vol_level < REGIME_ELEVATED {
        let score = (25.0 + ((vol_level - REGIME_LOW) / (REGIME_ELEVATED - REGIME_LOW)) * 25.0).round() as u8;
        ("NORMAL".to_string(), score)
    } else if vol_level < REGIME_CRISIS {
        let score = (50.0 + ((vol_level - REGIME_ELEVATED) / (REGIME_CRISIS - REGIME_ELEVATED)) * 25.0).round() as u8;
        ("ELEVATED".to_string(), score)
    } else {
        let score = (75.0 + ((vol_level - REGIME_CRISIS) / 40.0).min(1.0) * 25.0).round() as u8;
        ("CRISIS".to_string(), score.min(100))
    }
}

pub fn tally_phase() -> Result<()> {
    let reveals = get_reveals()?;
    let num_executors = reveals.len();

    log!("Vol surface tally: {} reveals", num_executors);

    let mut results: Vec<ExecutionResult> = Vec::new();
    for reveal in reveals {
        match serde_json::from_slice::<ExecutionResult>(&reveal.body.reveal) {
            Ok(r) => results.push(r),
            Err(e) => { elog!("Parse error: {}", e); }
        }
    }

    if results.is_empty() {
        Process::error(b"No valid vol reveals");
    }

    let num_valid = results.len();
    let currency = results[0].cy.clone();
    let src_count = results[0].sn.len();

    // Consensus spot
    let mut all_spots: Vec<u64> = results.iter()
        .flat_map(|r| r.sp.iter().copied())
        .filter(|p| *p > 0)
        .collect();
    let spot_micro = median_u64(&mut all_spots);
    let spot_usd = spot_micro as f64 / 1_000_000.0;

    // Consensus DVOL + RV
    let mut dvols: Vec<f64> = results.iter().map(|r| r.dv).filter(|v| *v > 0.0).collect();
    let dvol = median_f64(&mut dvols);
    let mut rvs: Vec<f64> = results.iter().map(|r| r.rv).filter(|v| *v > 0.0).collect();
    let rv = median_f64(&mut rvs);
    let vrp = if dvol > 0.0 && rv > 0.0 { dvol - rv } else { 0.0 };

    // Consensus ATM IVs
    let mut call_ivs: Vec<f64> = results.iter().map(|r| r.ac).filter(|v| *v > 0.0).collect();
    let atm_call = median_f64(&mut call_ivs);
    let mut put_ivs: Vec<f64> = results.iter().map(|r| r.ap).filter(|v| *v > 0.0).collect();
    let atm_put = median_f64(&mut put_ivs);
    let atm_iv = if atm_call > 0.0 && atm_put > 0.0 { (atm_call + atm_put) / 2.0 } else { atm_call.max(atm_put) };
    let skew = if atm_put > 0.0 && atm_call > 0.0 { atm_put - atm_call } else { 0.0 };

    // Consensus greeks
    let mut deltas_c: Vec<f64> = results.iter().map(|r| r.cd).filter(|v| *v != 0.0).collect();
    let call_delta = median_f64(&mut deltas_c);
    let mut deltas_p: Vec<f64> = results.iter().map(|r| r.pd).filter(|v| *v != 0.0).collect();
    let put_delta = median_f64(&mut deltas_p);
    let mut gammas: Vec<f64> = results.iter().map(|r| r.gm).filter(|v| *v > 0.0).collect();
    let gamma = median_f64(&mut gammas);
    let mut vegas: Vec<f64> = results.iter().map(|r| r.vg).filter(|v| *v > 0.0).collect();
    let vega = median_f64(&mut vegas);

    // Regime
    let (regime, rscore) = classify_regime(dvol, rv);

    log!("Surface: spot=${:.2} DVOL={:.1}% RV={:.1}% ATM={:.1}% skew={:.1}% regime={}",
        spot_usd, dvol, rv, atm_iv, skew, regime);

    let round2 = |v: f64| ((v * 100.0).round()) / 100.0;

    let output = VolSurface {
        cy: currency,
        spot: round2(spot_usd),
        dvol: round2(dvol),
        rv: round2(rv),
        vrp: round2(vrp),
        atm: round2(atm_iv),
        ac: round2(atm_call),
        ap: round2(atm_put),
        skew: round2(skew),
        cd: round2(call_delta),
        pd: round2(put_delta),
        gm: round2(gamma),
        vg: round2(vega),
        regime,
        rscore,
        src: src_count,
        ex: num_executors,
        ok: num_valid,
    };

    let json_bytes = serde_json::to_vec(&output)?;
    Process::success(&json_bytes);

    #[allow(unreachable_code)]
    Ok(())
}
