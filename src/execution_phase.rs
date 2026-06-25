use anyhow::Result;
use seda_sdk_rs::{http_fetch, log, Process};
use serde::{Deserialize, Serialize};

/// Derive Volatility Surface Oracle — Execution Phase
///
/// Fetches three categories of data:
///   1. Multi-source spot price (Pyth, Coinbase, Kraken) for realized vol
///   2. Deribit DVOL index + historical realized vol
///   3. Deribit option book summary (mark_iv across strikes/expiries)
///
/// Input:  { "currency": "ETH", "pyth_feed_id": "0x...",
///           "spot_asset": "ETH-USD" }
///
/// Output: Compact vol snapshot for tally-phase aggregation

// ── Endpoints ───────────────────────────────────────────────────────
const PYTH_HERMES: &str = "https://hermes.pyth.network/v2/updates/price/latest";
const COINBASE_URL: &str = "https://api.coinbase.com/v2/prices";
const KRAKEN_URL: &str = "https://api.kraken.com/0/public/Ticker";
const DERIBIT_API: &str = "https://www.deribit.com/api/v2/public";

fn kraken_pair(asset: &str) -> Option<&'static str> {
    match asset.to_uppercase().as_str() {
        "ETH-USD" => Some("XETHZUSD"),
        "BTC-USD" => Some("XXBTZUSD"),
        _ => None,
    }
}

#[derive(Deserialize)]
pub struct OracleInput {
    /// Currency: "ETH" or "BTC"
    pub currency: String,
    /// Pyth feed ID
    #[serde(default)]
    pub pyth_feed_id: String,
    /// Spot asset pair for Coinbase/Kraken, e.g. "ETH-USD"
    #[serde(default = "default_spot_asset")]
    pub spot_asset: String,
}

fn default_spot_asset() -> String { "ETH-USD".to_string() }

/// Option snapshot from Deribit book summary
#[derive(Serialize, Deserialize, Clone)]
pub struct OptionSnap {
    /// Instrument name, e.g. "ETH-27JUN26-1650-C"
    pub i: String,
    /// Mark implied volatility (%)
    pub iv: f64,
    /// Strike price (USD)
    pub k: f64,
    /// Option type: "C" or "P"
    pub ot: String,
    /// Days to expiry
    pub dte: f64,
    /// Open interest
    pub oi: f64,
    /// Underlying price at time of quote
    pub u: f64,
}

/// Compact execution result
#[derive(Serialize, Deserialize)]
pub struct ExecutionResult {
    /// Currency
    pub cy: String,
    /// Spot prices from each source (micro-cents)
    pub sp: Vec<u64>,
    /// Source names
    pub sn: Vec<String>,
    /// DVOL index value (annualized IV %)
    pub dv: f64,
    /// Deribit historical realized vol (%)
    pub rv: f64,
    /// Top option snapshots (ATM + near-delta, sorted by OI)
    pub opts: Vec<OptionSnap>,
    /// Underlying price from Deribit
    pub up: f64,
}

/// Fetch spot from Pyth
fn fetch_pyth_spot(feed_id: &str) -> Option<u64> {
    if feed_id.is_empty() { return None; }
    let clean = feed_id.trim_start_matches("0x");
    let url = format!("{}?ids[]=0x{}&parsed=true", PYTH_HERMES, clean);
    let resp = http_fetch(&url, None);
    if !resp.is_ok() { return None; }
    let v: serde_json::Value = serde_json::from_slice(&resp.bytes).ok()?;
    let p = &v["parsed"][0]["price"];
    let raw = p["price"].as_str()?.parse::<i64>().ok()?;
    let expo = p["expo"].as_i64()?;
    let usd = raw as f64 * 10f64.powi(expo as i32);
    Some((usd * 1_000_000.0).round() as u64)
}

/// Fetch spot from Coinbase
fn fetch_coinbase_spot(asset: &str) -> Option<u64> {
    let url = format!("{}/{}/spot", COINBASE_URL, asset);
    let resp = http_fetch(&url, None);
    if !resp.is_ok() { return None; }
    let v: serde_json::Value = serde_json::from_slice(&resp.bytes).ok()?;
    let usd: f64 = v["data"]["amount"].as_str()?.parse().ok()?;
    Some((usd * 1_000_000.0).round() as u64)
}

/// Fetch spot from Kraken
fn fetch_kraken_spot(asset: &str) -> Option<u64> {
    let pair = kraken_pair(asset)?;
    let url = format!("{}?pair={}", KRAKEN_URL, pair);
    let resp = http_fetch(&url, None);
    if !resp.is_ok() { return None; }
    let v: serde_json::Value = serde_json::from_slice(&resp.bytes).ok()?;
    let usd: f64 = v["result"][pair]["c"][0].as_str()?.parse().ok()?;
    Some((usd * 1_000_000.0).round() as u64)
}

/// Fetch DVOL index from Deribit
fn fetch_dvol(currency: &str) -> f64 {
    let now_ms = 0u64; // We'll use a recent window
    let url = format!(
        "{}/get_volatility_index_data?currency={}&resolution=60&start_timestamp={}&end_timestamp={}",
        DERIBIT_API, currency,
        // Fetch last 2 hours of 1h candles
        "0", "9999999999999"
    );
    let resp = http_fetch(&url, None);
    if !resp.is_ok() {
        log!("DVOL fetch failed");
        return 0.0;
    }
    let v: serde_json::Value = match serde_json::from_slice(&resp.bytes) {
        Ok(v) => v,
        Err(_) => return 0.0,
    };
    // data is array of [timestamp, open, high, low, close]
    let data = &v["result"]["data"];
    if let Some(arr) = data.as_array() {
        if let Some(last) = arr.last() {
            // close is index 4
            if let Some(close) = last.get(4).and_then(|v| v.as_f64()) {
                log!("DVOL {}: {:.1}%", currency, close);
                return close;
            }
        }
    }
    0.0
}

/// Fetch historical realized vol from Deribit
fn fetch_historical_rv(currency: &str) -> f64 {
    let url = format!("{}/get_historical_volatility?currency={}", DERIBIT_API, currency);
    let resp = http_fetch(&url, None);
    if !resp.is_ok() {
        log!("Historical RV fetch failed");
        return 0.0;
    }
    let v: serde_json::Value = match serde_json::from_slice(&resp.bytes) {
        Ok(v) => v,
        Err(_) => return 0.0,
    };
    // result is array of [timestamp, rv_pct]
    if let Some(arr) = v["result"].as_array() {
        if let Some(last) = arr.last() {
            if let Some(rv) = last.get(1).and_then(|v| v.as_f64()) {
                log!("Historical RV {}: {:.1}%", currency, rv);
                return rv;
            }
        }
    }
    0.0
}

/// Fetch option book summary — returns top options by OI near ATM
fn fetch_option_surface(currency: &str) -> (Vec<OptionSnap>, f64) {
    let url = format!(
        "{}/get_book_summary_by_currency?currency={}&kind=option",
        DERIBIT_API, currency
    );
    let resp = http_fetch(&url, None);
    if !resp.is_ok() {
        log!("Option book fetch failed");
        return (Vec::new(), 0.0);
    }
    let v: serde_json::Value = match serde_json::from_slice(&resp.bytes) {
        Ok(v) => v,
        Err(_) => return (Vec::new(), 0.0),
    };

    let entries = match v["result"].as_array() {
        Some(a) => a,
        None => return (Vec::new(), 0.0),
    };

    // Get underlying price from first entry
    let underlying = entries.first()
        .and_then(|e| e["underlying_price"].as_f64())
        .unwrap_or(0.0);

    let mut snaps: Vec<OptionSnap> = Vec::new();

    for entry in entries {
        let name = match entry["instrument_name"].as_str() {
            Some(n) => n,
            None => continue,
        };
        let iv = entry["mark_iv"].as_f64().unwrap_or(0.0);
        let oi = entry["open_interest"].as_f64().unwrap_or(0.0);
        let up = entry["underlying_price"].as_f64().unwrap_or(underlying);

        if iv <= 0.0 { continue; }

        // Parse instrument name: ETH-27JUN26-1650-C
        let parts: Vec<&str> = name.split('-').collect();
        if parts.len() < 4 { continue; }

        let strike: f64 = match parts[2].parse() {
            Ok(k) => k,
            Err(_) => continue,
        };
        let opt_type = parts[3]; // "C" or "P"

        // Estimate DTE from expiry string (rough — good enough for surface)
        // We can't use real dates in WASM, so we use a simple heuristic:
        // options listed on Deribit have DTE encoded in the data
        let mid_price = entry["mid_price"].as_f64().unwrap_or(0.0);
        let dte = if mid_price > 0.0 && up > 0.0 {
            // Rough DTE from ATM option price using vol
            // For near-ATM: price ≈ 0.4 * vol * sqrt(T/365) * spot
            // T ≈ (price / (0.4 * vol/100 * spot))^2 * 365
            let vol_dec = iv / 100.0;
            if vol_dec > 0.0 {
                let ratio = mid_price / (0.4 * vol_dec * up);
                let t_years = ratio * ratio;
                (t_years * 365.0).max(0.0).min(365.0)
            } else {
                0.0
            }
        } else {
            0.0
        };

        snaps.push(OptionSnap {
            i: name.to_string(),
            iv,
            k: strike,
            ot: opt_type.to_string(),
            dte,
            oi,
            u: up,
        });
    }

    // Sort by OI descending, keep top options near ATM
    snaps.sort_by(|a, b| b.oi.partial_cmp(&a.oi).unwrap_or(std::cmp::Ordering::Equal));

    // Filter: keep options within 20% of spot, top 20 by OI
    let filtered: Vec<OptionSnap> = snaps.into_iter()
        .filter(|s| {
            let moneyness = (s.k - underlying).abs() / underlying;
            moneyness < 0.20
        })
        .take(20)
        .collect();

    log!("Fetched {} near-ATM options for {}", filtered.len(), currency);
    (filtered, underlying)
}

pub fn execution_phase() -> Result<()> {
    let raw_input = String::from_utf8(Process::get_inputs())?;
    let input: OracleInput = serde_json::from_str(raw_input.trim())?;

    log!("Vol surface oracle: {} (spot: {})", input.currency, input.spot_asset);

    // 1. Multi-source spot
    let mut spots: Vec<u64> = Vec::new();
    let mut names: Vec<String> = Vec::new();

    if let Some(p) = fetch_pyth_spot(&input.pyth_feed_id) {
        spots.push(p); names.push("Pyth".into());
        log!("Pyth: ${:.2}", p as f64 / 1_000_000.0);
    }
    if let Some(p) = fetch_coinbase_spot(&input.spot_asset) {
        spots.push(p); names.push("Coinbase".into());
        log!("Coinbase: ${:.2}", p as f64 / 1_000_000.0);
    }
    if let Some(p) = fetch_kraken_spot(&input.spot_asset) {
        spots.push(p); names.push("Kraken".into());
        log!("Kraken: ${:.2}", p as f64 / 1_000_000.0);
    }

    if spots.is_empty() {
        Process::error(b"No spot sources returned valid prices");
    }

    // 2. DVOL + historical RV
    let dvol = fetch_dvol(&input.currency);
    let rv = fetch_historical_rv(&input.currency);

    // 3. Option surface
    let (opts, underlying) = fetch_option_surface(&input.currency);

    let result = ExecutionResult {
        cy: input.currency,
        sp: spots,
        sn: names,
        dv: ((dvol * 100.0).round()) / 100.0,
        rv: ((rv * 100.0).round()) / 100.0,
        opts,
        up: ((underlying * 100.0).round()) / 100.0,
    };

    let json_bytes = serde_json::to_vec(&result)?;
    log!("Vol snapshot: {} bytes, DVOL={:.1}%, RV={:.1}%, {} options",
        json_bytes.len(), dvol, rv, result.opts.len());
    Process::success(&json_bytes);

    #[allow(unreachable_code)]
    Ok(())
}
