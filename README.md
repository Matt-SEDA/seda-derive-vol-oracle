# SEDA × Derive — Volatility Surface Oracle

A decentralized volatility surface oracle that gives Derive a single-call snapshot of the options vol landscape: ATM IV, skew, term structure, realized vol, vol risk premium, and regime classification. No one else has this onchain.

**Testnet Oracle Program ID:** `59af8c841ff8870d1baf0abbbdb9ff350d139f078eb376719b64ca777ac5c9d3`

## Try It

```bash
curl -X POST "https://fast-api.testnet.seda.xyz/execute?includeDebugInfo=true&encoding=json" \
  -H "Authorization: Bearer YOUR_SEDA_FAST_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "execProgramId": "59af8c841ff8870d1baf0abbbdb9ff350d139f078eb376719b64ca777ac5c9d3",
    "execInputs": {
      "currency": "ETH",
      "pyth_feed_id": "0xff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace",
      "spot_asset": "ETH-USD"
    }
  }'
```

## What It Returns

```json
{
  "cy": "ETH",
  "spot": 1654.20,
  "dvol": 57.80,
  "rv": 48.70,
  "vrp": 9.10,
  "atm": 56.50,
  "p25": 64.00,
  "c25": 51.00,
  "skew": 13.00,
  "iv_near": 59.00,
  "iv_far": 55.00,
  "ts": -4.00,
  "regime": "NORMAL",
  "rscore": 48,
  "src": 3,
  "ex": 1,
  "ok": 1
}
```

| Field | What it tells you |
|-------|-------------------|
| `spot` | Consensus spot from Pyth + Coinbase + Kraken (median) |
| `dvol` | Deribit DVOL index — the market's implied vol benchmark |
| `rv` | Historical realized volatility from Deribit |
| `vrp` | **Vol risk premium** (IV - RV). Positive = options are expensive relative to actual moves |
| `atm` | ATM implied vol — nearest-strike call/put average |
| `p25` / `c25` | 25-delta put and call IV |
| `skew` | Put IV - Call IV. Positive = downside protection premium (normal for crypto) |
| `iv_near` / `iv_far` | Near-term vs far-term ATM IV |
| `ts` | Term structure slope. Negative = backwardation (near-term fear). Positive = contango (calm) |
| `regime` | `LOW` / `NORMAL` / `ELEVATED` / `CRISIS` |
| `rscore` | 0–100 regime intensity (0 = dead calm, 100 = extreme stress) |

## Why Derive Needs This

**1. Margin adjustment on regime shifts.** When `regime` flips from NORMAL to ELEVATED, Derive can automatically widen margin requirements before getting arbitraged by stale pricing.

**2. Independent vol surface.** Derive currently computes IV internally. This oracle gives them a decentralized, multi-source alternative they can cross-check against — or use directly.

**3. Skew intelligence.** The `skew` field tells Derive whether puts are abnormally expensive (fear) or cheap (complacency). This feeds directly into options pricing and risk management.

**4. Vol risk premium signal.** When `vrp` is large and positive, implied vol is overpricing actual risk — Derive's market makers can adjust. When `vrp` is negative, realized vol exceeds what the market expects — time to widen spreads.

**5. Term structure.** Backwardation (`ts` < 0) means near-term fear exceeds long-term — Derive should be cautious about short-dated contracts. Contango (`ts` > 0) means the curve is normal.

## How It Works

### Execution phase

Each SEDA executor independently fetches:

| Data | Source | Auth |
|------|--------|------|
| Spot price | Pyth Hermes, Coinbase, Kraken | None (public) |
| DVOL index | Deribit `get_volatility_index_data` | None (public) |
| Realized vol | Deribit `get_historical_volatility` | None (public) |
| Option chain | Deribit `get_book_summary_by_currency` | None (public) |

### Tally phase

1. **Consensus spot** — median across all executor × source prices
2. **Consensus DVOL and RV** — median across executors
3. **ATM IV extraction** — nearest-strike options, average of put + call IV
4. **25-delta skew** — put IV at ~93% of spot vs call IV at ~107% of spot
5. **Term structure** — ATM IV at shortest expiry vs longest
6. **Vol risk premium** — DVOL minus realized vol
7. **Regime classification** — based on max(DVOL, RV) thresholds: <30% LOW, 30-60% NORMAL, 60-80% ELEVATED, >80% CRISIS

## Supported Currencies

| Currency | Pyth Feed ID |
|----------|-------------|
| **ETH** | `0xff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace` |
| **BTC** | `0xe62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43` |

## Build & Test

```bash
bun install
make build
bun test tests/
```

4 tests: execution (multi-source + Deribit data), NORMAL regime, CRISIS regime, skew detection.

## Architecture

```
src/
  main.rs              — SEDA oracle program entry point
  execution_phase.rs   — Spot (Pyth/Coinbase/Kraken) + Deribit vol data
  tally_phase.rs       — Surface construction, skew, term structure, regime
tests/
  index.test.ts        — 4 test cases
```

Built on [SEDA](https://seda.xyz) — custom oracle logic, any data, one endpoint.
