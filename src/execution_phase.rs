use anyhow::Result;
use seda_sdk_rs::{http_fetch, log, Process};
use serde::{Deserialize, Serialize};

/// Derive Volatility Surface Oracle — Execution Phase (gas-minimal)
///
/// 5 HTTP calls total — fits within SEDA Fast gas limits:
///   1. Pyth spot
///   2. Coinbase spot
///   3. Kraken spot
///   4. Deribit DVOL (= 30-day synthetic ATM IV)
///   5. Deribit historical realized vol

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
    pub currency: String,
    #[serde(default)]
    pub pyth_feed_id: String,
    #[serde(default = "default_spot_asset")]
    pub spot_asset: String,
}

fn default_spot_asset() -> String { "ETH-USD".to_string() }

/// Compact execution result
#[derive(Serialize, Deserialize)]
pub struct ExecutionResult {
    /// Currency
    pub cy: String,
    /// Spot prices from each source (micro-cents)
    pub sp: Vec<u64>,
    /// Source names
    pub sn: Vec<String>,
    /// DVOL index (annualized IV %, = 30-day synthetic ATM vol)
    pub dv: f64,
    /// Historical realized vol (%)
    pub rv: f64,
}

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

fn fetch_coinbase_spot(asset: &str) -> Option<u64> {
    let url = format!("{}/{}/spot", COINBASE_URL, asset);
    let resp = http_fetch(&url, None);
    if !resp.is_ok() { return None; }
    let v: serde_json::Value = serde_json::from_slice(&resp.bytes).ok()?;
    let usd: f64 = v["data"]["amount"].as_str()?.parse().ok()?;
    Some((usd * 1_000_000.0).round() as u64)
}

fn fetch_kraken_spot(asset: &str) -> Option<u64> {
    let pair = kraken_pair(asset)?;
    let url = format!("{}?pair={}", KRAKEN_URL, pair);
    let resp = http_fetch(&url, None);
    if !resp.is_ok() { return None; }
    let v: serde_json::Value = serde_json::from_slice(&resp.bytes).ok()?;
    let usd: f64 = v["result"][pair]["c"][0].as_str()?.parse().ok()?;
    Some((usd * 1_000_000.0).round() as u64)
}

fn fetch_dvol(currency: &str) -> f64 {
    let url = format!(
        "{}/get_volatility_index_data?currency={}&resolution=3600&start_timestamp=0&end_timestamp=9999999999999",
        DERIBIT_API, currency
    );
    let resp = http_fetch(&url, None);
    if !resp.is_ok() { return 0.0; }
    let v: serde_json::Value = match serde_json::from_slice(&resp.bytes) {
        Ok(v) => v,
        Err(_) => return 0.0,
    };
    if let Some(arr) = v["result"]["data"].as_array() {
        if let Some(last) = arr.last() {
            if let Some(close) = last.get(4).and_then(|v| v.as_f64()) {
                log!("DVOL {}: {:.1}%", currency, close);
                return close;
            }
        }
    }
    0.0
}

fn fetch_rv(currency: &str) -> f64 {
    let url = format!("{}/get_historical_volatility?currency={}", DERIBIT_API, currency);
    let resp = http_fetch(&url, None);
    if !resp.is_ok() { return 0.0; }
    let v: serde_json::Value = match serde_json::from_slice(&resp.bytes) {
        Ok(v) => v,
        Err(_) => return 0.0,
    };
    if let Some(arr) = v["result"].as_array() {
        if let Some(last) = arr.last() {
            if let Some(rv) = last.get(1).and_then(|v| v.as_f64()) {
                log!("RV {}: {:.1}%", currency, rv);
                return rv;
            }
        }
    }
    0.0
}

pub fn execution_phase() -> Result<()> {
    let raw_input = String::from_utf8(Process::get_inputs())?;
    let input: OracleInput = serde_json::from_str(raw_input.trim())?;

    log!("Vol oracle: {} ({})", input.currency, input.spot_asset);

    // 1-3. Multi-source spot
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

    // 4. DVOL (Deribit's 30-day synthetic ATM IV)
    let dvol = fetch_dvol(&input.currency);

    // 5. Historical realized vol
    let rv = fetch_rv(&input.currency);

    let round2 = |v: f64| ((v * 100.0).round()) / 100.0;

    let result = ExecutionResult {
        cy: input.currency,
        sp: spots,
        sn: names,
        dv: round2(dvol),
        rv: round2(rv),
    };

    let json_bytes = serde_json::to_vec(&result)?;
    log!("Done: {} bytes", json_bytes.len());
    Process::success(&json_bytes);

    #[allow(unreachable_code)]
    Ok(())
}
