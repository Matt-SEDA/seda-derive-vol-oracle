use anyhow::Result;
use seda_sdk_rs::{http_fetch, log, Process};
use serde::{Deserialize, Serialize};

/// Derive Volatility Surface Oracle — Execution Phase (gas-optimized)
///
/// Uses targeted API calls instead of bulk option book to stay within gas limits.
///
///   1. Multi-source spot price (Pyth, Coinbase, Kraken)
///   2. Deribit DVOL index (single number — the market's IV benchmark)
///   3. Deribit historical realized vol (single number)
///   4. Deribit ticker for a specific near-ATM instrument (gives greeks + IV)
///
/// Total: 6 small HTTP calls instead of 1 massive 788-instrument bulk call.

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

/// Compact execution result — stays well under 1024 bytes
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
    /// Historical realized vol (%)
    pub rv: f64,
    /// Deribit underlying/index price
    pub up: f64,
    /// ATM call IV from ticker (%)
    pub ac: f64,
    /// ATM put IV from ticker (%)
    pub ap: f64,
    /// ATM call delta
    pub cd: f64,
    /// ATM put delta
    pub pd: f64,
    /// ATM gamma
    pub gm: f64,
    /// ATM vega
    pub vg: f64,
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

fn fetch_historical_rv(currency: &str) -> f64 {
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

/// Fetch a single option ticker from Deribit — returns (mark_iv, delta, gamma, vega, underlying)
fn fetch_ticker(instrument: &str) -> (f64, f64, f64, f64, f64) {
    let url = format!("{}/ticker?instrument_name={}", DERIBIT_API, instrument);
    let resp = http_fetch(&url, None);
    if !resp.is_ok() {
        log!("Ticker fetch failed: {}", instrument);
        return (0.0, 0.0, 0.0, 0.0, 0.0);
    }
    let v: serde_json::Value = match serde_json::from_slice(&resp.bytes) {
        Ok(v) => v,
        Err(_) => return (0.0, 0.0, 0.0, 0.0, 0.0),
    };
    let r = &v["result"];
    let iv = r["mark_iv"].as_f64().unwrap_or(0.0);
    let delta = r["greeks"]["delta"].as_f64().unwrap_or(0.0);
    let gamma = r["greeks"]["gamma"].as_f64().unwrap_or(0.0);
    let vega = r["greeks"]["vega"].as_f64().unwrap_or(0.0);
    let underlying = r["underlying_price"].as_f64().unwrap_or(0.0);
    log!("{}: IV={:.1}%, delta={:.3}", instrument, iv, delta);
    (iv, delta, gamma, vega, underlying)
}

/// Find a near-ATM instrument name for the nearest expiry
/// Deribit instruments: ETH-27JUN26-1650-C
/// We round spot to nearest $25 (ETH) or $500 (BTC) strike
fn atm_instrument(currency: &str, spot: f64, opt_type: &str, expiry: &str) -> String {
    let strike_step = if currency == "BTC" { 500.0 } else { 25.0 };
    let rounded = (spot / strike_step).round() * strike_step;
    format!("{}-{}-{}-{}", currency, expiry, rounded as u64, opt_type)
}

/// Find the nearest expiry from Deribit's instruments endpoint (lightweight)
fn fetch_nearest_expiry(currency: &str) -> String {
    let url = format!(
        "{}/get_instruments?currency={}&kind=option&expired=false",
        DERIBIT_API, currency
    );
    let resp = http_fetch(&url, None);
    if !resp.is_ok() { return "".to_string(); }

    let v: serde_json::Value = match serde_json::from_slice(&resp.bytes) {
        Ok(v) => v,
        Err(_) => return "".to_string(),
    };

    // Find the smallest expiration_timestamp
    let mut min_ts = u64::MAX;
    let mut min_expiry = String::new();

    if let Some(arr) = v["result"].as_array() {
        for inst in arr {
            if let Some(ts) = inst["expiration_timestamp"].as_u64() {
                if ts < min_ts {
                    min_ts = ts;
                    // Extract expiry from instrument name: ETH-27JUN26-1650-C -> 27JUN26
                    if let Some(name) = inst["instrument_name"].as_str() {
                        let parts: Vec<&str> = name.split('-').collect();
                        if parts.len() >= 4 {
                            min_expiry = parts[1].to_string();
                        }
                    }
                }
            }
        }
    }
    log!("Nearest expiry: {}", min_expiry);
    min_expiry
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

    // Compute median spot for ATM strike lookup
    let mut sorted_spots = spots.clone();
    sorted_spots.sort();
    let median_spot = if sorted_spots.len() % 2 == 0 {
        let mid = sorted_spots.len() / 2;
        (sorted_spots[mid - 1] + sorted_spots[mid]) / 2
    } else {
        sorted_spots[sorted_spots.len() / 2]
    };
    let spot_usd = median_spot as f64 / 1_000_000.0;

    // 2. DVOL + RV (2 small calls)
    let dvol = fetch_dvol(&input.currency);
    let rv = fetch_historical_rv(&input.currency);

    // 3. Find nearest expiry, then fetch ATM call + ATM put tickers (2 tiny calls)
    let expiry = fetch_nearest_expiry(&input.currency);

    let (mut atm_call_iv, mut call_delta, mut gamma, mut vega, mut underlying) =
        (0.0, 0.0, 0.0, 0.0, 0.0);
    let (mut atm_put_iv, mut put_delta) = (0.0, 0.0);

    if !expiry.is_empty() {
        let call_name = atm_instrument(&input.currency, spot_usd, "C", &expiry);
        let (c_iv, c_d, c_g, c_v, c_u) = fetch_ticker(&call_name);
        atm_call_iv = c_iv;
        call_delta = c_d;
        gamma = c_g;
        vega = c_v;
        underlying = c_u;

        let put_name = atm_instrument(&input.currency, spot_usd, "P", &expiry);
        let (p_iv, p_d, _, _, _) = fetch_ticker(&put_name);
        atm_put_iv = p_iv;
        put_delta = p_d;
    }

    if underlying <= 0.0 {
        underlying = spot_usd;
    }

    let round2 = |v: f64| ((v * 100.0).round()) / 100.0;

    let result = ExecutionResult {
        cy: input.currency,
        sp: spots,
        sn: names,
        dv: round2(dvol),
        rv: round2(rv),
        up: round2(underlying),
        ac: round2(atm_call_iv),
        ap: round2(atm_put_iv),
        cd: round2(call_delta),
        pd: round2(put_delta),
        gm: round2(gamma),
        vg: round2(vega),
    };

    let json_bytes = serde_json::to_vec(&result)?;
    log!("Vol snapshot: {} bytes, DVOL={:.1}%, RV={:.1}%, ATM C={:.1}% P={:.1}%",
        json_bytes.len(), dvol, rv, atm_call_iv, atm_put_iv);
    Process::success(&json_bytes);

    #[allow(unreachable_code)]
    Ok(())
}
