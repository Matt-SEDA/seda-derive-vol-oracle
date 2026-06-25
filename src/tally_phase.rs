use anyhow::Result;
use seda_sdk_rs::{elog, get_reveals, log, Process};
use serde::Serialize;

use crate::execution_phase::{ExecutionResult, OptionSnap};

/// Derive Volatility Surface Oracle — Tally Phase
///
/// Aggregates vol data from multiple executors into a consensus vol surface:
///   1. Median spot price across executors and sources
///   2. Median DVOL and RV across executors
///   3. ATM IV extraction (nearest-strike options)
///   4. 25-delta skew computation (put IV - call IV at ~25 delta)
///   5. Term structure (IV at nearest expiry vs further expiry)
///   6. Vol risk premium = IV - RV
///   7. Regime classification: LOW / NORMAL / ELEVATED / CRISIS

/// Vol regime thresholds (annualized IV %)
const REGIME_LOW: f64 = 30.0;
const REGIME_ELEVATED: f64 = 60.0;
const REGIME_CRISIS: f64 = 80.0;

/// Compact tally output — the vol surface
#[derive(Serialize)]
struct VolSurface {
    /// Currency
    cy: String,
    /// Consensus spot price (USD, 2dp)
    spot: f64,
    /// DVOL index (annualized IV %)
    dvol: f64,
    /// Historical realized vol (%)
    rv: f64,
    /// Vol risk premium: IV - RV (positive = options expensive)
    vrp: f64,
    /// ATM implied vol (%) — nearest strike to spot
    atm: f64,
    /// 25-delta put IV (%)
    p25: f64,
    /// 25-delta call IV (%)
    c25: f64,
    /// Skew: put IV - call IV at 25-delta (positive = put premium)
    skew: f64,
    /// Near-term IV (shortest expiry ATM)
    iv_near: f64,
    /// Far-term IV (longest expiry ATM)
    iv_far: f64,
    /// Term structure slope: far - near (positive = contango)
    ts: f64,
    /// Vol regime: "LOW", "NORMAL", "ELEVATED", "CRISIS"
    regime: String,
    /// Regime score 0-100 (0=calm, 100=extreme)
    rscore: u8,
    /// Sources used for spot
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
    if len % 2 == 0 {
        (vals[len / 2 - 1] + vals[len / 2]) / 2.0
    } else {
        vals[len / 2]
    }
}

fn median_u64(vals: &mut Vec<u64>) -> u64 {
    if vals.is_empty() { return 0; }
    vals.sort();
    let len = vals.len();
    if len % 2 == 0 {
        (vals[len / 2 - 1] + vals[len / 2]) / 2
    } else {
        vals[len / 2]
    }
}

fn classify_regime(dvol: f64, rv: f64) -> (String, u8) {
    // Use the higher of IV and RV for regime classification
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

/// Extract ATM IV from options nearest to spot
fn extract_atm_iv(opts: &[OptionSnap], spot: f64) -> f64 {
    if opts.is_empty() || spot <= 0.0 { return 0.0; }

    // Find calls and puts nearest to spot
    let mut best_call: Option<&OptionSnap> = None;
    let mut best_call_dist = f64::MAX;
    let mut best_put: Option<&OptionSnap> = None;
    let mut best_put_dist = f64::MAX;

    for opt in opts {
        let dist = (opt.k - spot).abs();
        if opt.ot == "C" && dist < best_call_dist {
            best_call_dist = dist;
            best_call = Some(opt);
        }
        if opt.ot == "P" && dist < best_put_dist {
            best_put_dist = dist;
            best_put = Some(opt);
        }
    }

    match (best_call, best_put) {
        (Some(c), Some(p)) => (c.iv + p.iv) / 2.0,
        (Some(c), None) => c.iv,
        (None, Some(p)) => p.iv,
        (None, None) => 0.0,
    }
}

/// Extract 25-delta skew
/// Approximation: 25-delta call ≈ strike 5-10% above spot
///                 25-delta put  ≈ strike 5-10% below spot
fn extract_25d_skew(opts: &[OptionSnap], spot: f64) -> (f64, f64, f64) {
    if opts.is_empty() || spot <= 0.0 { return (0.0, 0.0, 0.0); }

    // Target: calls at 105-110% of spot, puts at 90-95% of spot
    let call_target = spot * 1.07;
    let put_target = spot * 0.93;

    let mut best_call_iv = 0.0f64;
    let mut best_call_dist = f64::MAX;
    let mut best_put_iv = 0.0f64;
    let mut best_put_dist = f64::MAX;

    for opt in opts {
        if opt.ot == "C" {
            let dist = (opt.k - call_target).abs();
            if dist < best_call_dist {
                best_call_dist = dist;
                best_call_iv = opt.iv;
            }
        }
        if opt.ot == "P" {
            let dist = (opt.k - put_target).abs();
            if dist < best_put_dist {
                best_put_dist = dist;
                best_put_iv = opt.iv;
            }
        }
    }

    let skew = if best_put_iv > 0.0 && best_call_iv > 0.0 {
        best_put_iv - best_call_iv
    } else {
        0.0
    };

    (best_put_iv, best_call_iv, skew)
}

/// Extract term structure: IV at shortest vs longest DTE
fn extract_term_structure(opts: &[OptionSnap], spot: f64) -> (f64, f64, f64) {
    if opts.is_empty() || spot <= 0.0 { return (0.0, 0.0, 0.0); }

    // Group by rough DTE bucket, take ATM options
    let atm_opts: Vec<&OptionSnap> = opts.iter()
        .filter(|o| {
            let moneyness = (o.k - spot).abs() / spot;
            moneyness < 0.05 // within 5% of spot
        })
        .collect();

    if atm_opts.is_empty() { return (0.0, 0.0, 0.0); }

    let mut min_dte = f64::MAX;
    let mut max_dte = 0.0f64;
    let mut near_iv = 0.0f64;
    let mut far_iv = 0.0f64;

    for opt in &atm_opts {
        if opt.dte > 0.0 && opt.dte < min_dte {
            min_dte = opt.dte;
            near_iv = opt.iv;
        }
        if opt.dte > max_dte {
            max_dte = opt.dte;
            far_iv = opt.iv;
        }
    }

    let slope = if near_iv > 0.0 && far_iv > 0.0 { far_iv - near_iv } else { 0.0 };
    (near_iv, far_iv, slope)
}

pub fn tally_phase() -> Result<()> {
    let reveals = get_reveals()?;
    let num_executors = reveals.len();

    log!("Vol surface tally: {} executor reveals", num_executors);

    let mut results: Vec<ExecutionResult> = Vec::new();
    for reveal in reveals {
        match serde_json::from_slice::<ExecutionResult>(&reveal.body.reveal) {
            Ok(r) => results.push(r),
            Err(e) => { elog!("Failed to parse reveal: {}", e); }
        }
    }

    if results.is_empty() {
        Process::error(b"No valid vol reveals");
    }

    let num_valid = results.len();
    let currency = results[0].cy.clone();

    // ── 1. Consensus spot price ─────────────────────────────────────
    let mut all_spots: Vec<u64> = results.iter()
        .flat_map(|r| r.sp.iter().copied())
        .filter(|p| *p > 0)
        .collect();
    let spot_micro = median_u64(&mut all_spots);
    let spot_usd = spot_micro as f64 / 1_000_000.0;
    let src_count = results[0].sn.len();

    log!("Consensus spot: ${:.2} ({} sources)", spot_usd, src_count);

    // ── 2. Consensus DVOL and RV ────────────────────────────────────
    let mut dvols: Vec<f64> = results.iter().map(|r| r.dv).filter(|v| *v > 0.0).collect();
    let dvol = median_f64(&mut dvols);

    let mut rvs: Vec<f64> = results.iter().map(|r| r.rv).filter(|v| *v > 0.0).collect();
    let rv = median_f64(&mut rvs);

    let vrp = if dvol > 0.0 && rv > 0.0 { dvol - rv } else { 0.0 };

    log!("DVOL: {:.1}%, RV: {:.1}%, VRP: {:.1}%", dvol, rv, vrp);

    // ── 3. Merge option snapshots across executors ───────────────────
    // Take the option set from the executor with the most options
    let best_opts = results.iter()
        .max_by_key(|r| r.opts.len())
        .map(|r| &r.opts)
        .unwrap_or(&results[0].opts);

    // ── 4. ATM IV ───────────────────────────────────────────────────
    let atm_iv = extract_atm_iv(best_opts, spot_usd);
    log!("ATM IV: {:.1}%", atm_iv);

    // ── 5. 25-delta skew ────────────────────────────────────────────
    let (p25_iv, c25_iv, skew) = extract_25d_skew(best_opts, spot_usd);
    log!("25d skew: P={:.1}%, C={:.1}%, skew={:.1}%", p25_iv, c25_iv, skew);

    // ── 6. Term structure ───────────────────────────────────────────
    let (iv_near, iv_far, ts_slope) = extract_term_structure(best_opts, spot_usd);
    log!("Term structure: near={:.1}%, far={:.1}%, slope={:.1}%", iv_near, iv_far, ts_slope);

    // ── 7. Regime classification ────────────────────────────────────
    let (regime, rscore) = classify_regime(dvol, rv);
    log!("Regime: {} (score: {})", regime, rscore);

    // ── 8. Emit surface ─────────────────────────────────────────────
    let round2 = |v: f64| ((v * 100.0).round()) / 100.0;

    let output = VolSurface {
        cy: currency,
        spot: round2(spot_usd),
        dvol: round2(dvol),
        rv: round2(rv),
        vrp: round2(vrp),
        atm: round2(atm_iv),
        p25: round2(p25_iv),
        c25: round2(c25_iv),
        skew: round2(skew),
        iv_near: round2(iv_near),
        iv_far: round2(iv_far),
        ts: round2(ts_slope),
        regime,
        rscore,
        src: src_count,
        ex: num_executors,
        ok: num_valid,
    };

    let json_bytes = serde_json::to_vec(&output)?;
    log!("Vol surface: {} bytes", json_bytes.len());
    Process::success(&json_bytes);

    #[allow(unreachable_code)]
    Ok(())
}
