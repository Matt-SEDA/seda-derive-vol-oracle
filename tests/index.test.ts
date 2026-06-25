import { afterEach, describe, it, expect, mock } from "bun:test";
import { file } from "bun";
import { testOracleProgramExecution, testOracleProgramTally } from "@seda-protocol/dev-tools";

const WASM_PATH = "target/wasm32-wasip1/release-wasm/derive-vol-oracle.wasm";

const fetchMock = mock();
afterEach(() => { fetchMock.mockRestore(); });

// --- Mock responses ---
const PYTH_ETH = {
  parsed: [{ price: { price: "165413000000", conf: "5000000", expo: -8, publish_time: 1750000000 } }],
};
const COINBASE_ETH = { data: { amount: "1654.50" } };
const KRAKEN_ETH = { result: { XETHZUSD: { c: ["1654.20", "1.0"] } } };
const DVOL = { result: { data: [[1750000000000, 55.2, 58.1, 54.0, 57.8]] } };
const HIST_RV = { result: [[1749900000000, 45.2], [1750000000000, 48.7]] };
const INSTRUMENTS = {
  result: [
    { instrument_name: "ETH-27JUN26-1650-C", expiration_timestamp: 1751000000000 },
    { instrument_name: "ETH-27JUN26-1650-P", expiration_timestamp: 1751000000000 },
  ],
};
const TICKER_CALL = {
  result: {
    mark_iv: 56.5, underlying_price: 1654.0,
    greeks: { delta: 0.52, gamma: 0.006, vega: 0.48, theta: -6.9, rho: 0.05 },
  },
};
const TICKER_PUT = {
  result: {
    mark_iv: 58.2, underlying_price: 1654.0,
    greeks: { delta: -0.48, gamma: 0.006, vega: 0.48, theta: -6.5, rho: -0.04 },
  },
};

function mockApis() {
  let tickerCallCount = 0;
  fetchMock.mockImplementation((...args: any[]) => {
    const u = String(args[0] || "");
    if (u.includes("hermes.pyth.network")) return new Response(JSON.stringify(PYTH_ETH));
    if (u.includes("coinbase")) return new Response(JSON.stringify(COINBASE_ETH));
    if (u.includes("kraken")) return new Response(JSON.stringify(KRAKEN_ETH));
    if (u.includes("get_volatility_index")) return new Response(JSON.stringify(DVOL));
    if (u.includes("get_historical_volatility")) return new Response(JSON.stringify(HIST_RV));
    if (u.includes("get_instruments")) return new Response(JSON.stringify(INSTRUMENTS));
    if (u.includes("ticker")) {
      tickerCallCount++;
      return new Response(JSON.stringify(tickerCallCount <= 1 ? TICKER_CALL : TICKER_PUT));
    }
    return new Response(JSON.stringify(PYTH_ETH));
  });
}

function makeInput(overrides: Record<string, any> = {}) {
  return JSON.stringify({
    currency: "ETH",
    pyth_feed_id: "0xff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace",
    spot_asset: "ETH-USD",
    ...overrides,
  });
}

describe("Vol Oracle - execution phase", () => {
  it("should fetch spot + DVOL + RV + ATM tickers", async () => {
    mockApis();
    const wasm = await file(WASM_PATH).arrayBuffer();
    const vmResult = await testOracleProgramExecution(
      Buffer.from(wasm), Buffer.from(makeInput()), fetchMock
    );
    expect(vmResult.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vmResult.result).toString("utf-8"));

    expect(r.cy).toBe("ETH");
    expect(r.sp.length).toBe(3);
    expect(r.dv).toBeGreaterThan(0);
    expect(r.rv).toBeGreaterThan(0);
    expect(r.ac).toBeGreaterThan(0);
    expect(r.ap).toBeGreaterThan(0);
    expect(r.gm).toBeGreaterThan(0);

    console.log("Execution:", JSON.stringify(r, null, 2));
  });
});

describe("Vol Oracle - tally phase", () => {
  it("should produce NORMAL regime surface with skew + VRP", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();
    const reveal = JSON.stringify({
      cy: "ETH", sp: [1654_130_000, 1654_500_000, 1654_200_000],
      sn: ["Pyth", "Coinbase", "Kraken"],
      dv: 57.8, rv: 48.7, up: 1654.0,
      ac: 56.5, ap: 58.2, cd: 0.52, pd: -0.48, gm: 0.006, vg: 0.48,
    });
    const vmResult = await testOracleProgramTally(
      Buffer.from(wasm), Buffer.from("tally"),
      [{ exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(reveal) }],
    );
    expect(vmResult.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vmResult.result).toString("utf-8"));

    expect(r.regime).toBe("NORMAL");
    expect(r.vrp).toBeGreaterThan(0);
    expect(r.skew).toBeGreaterThan(0);
    expect(r.atm).toBeGreaterThan(50);

    console.log("NORMAL:", JSON.stringify(r, null, 2));
  });

  it("should detect CRISIS regime", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();
    const reveal = JSON.stringify({
      cy: "ETH", sp: [1654_000_000], sn: ["Pyth"],
      dv: 92.0, rv: 88.0, up: 1654.0,
      ac: 90.0, ap: 95.0, cd: 0.51, pd: -0.49, gm: 0.008, vg: 0.55,
    });
    const vmResult = await testOracleProgramTally(
      Buffer.from(wasm), Buffer.from("tally"),
      [{ exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(reveal) }],
    );
    expect(vmResult.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vmResult.result).toString("utf-8"));

    expect(r.regime).toBe("CRISIS");
    expect(r.rscore).toBeGreaterThanOrEqual(75);

    console.log("CRISIS:", JSON.stringify(r, null, 2));
  });

  it("should compute consensus across multiple executors", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();
    const r1 = JSON.stringify({
      cy: "ETH", sp: [1654_000_000, 1654_500_000], sn: ["Pyth", "Coinbase"],
      dv: 57.0, rv: 49.0, up: 1654.0,
      ac: 55.0, ap: 57.5, cd: 0.51, pd: -0.49, gm: 0.005, vg: 0.47,
    });
    const r2 = JSON.stringify({
      cy: "ETH", sp: [1654_200_000, 1654_400_000], sn: ["Pyth", "Coinbase"],
      dv: 58.0, rv: 48.5, up: 1654.0,
      ac: 56.0, ap: 58.0, cd: 0.52, pd: -0.48, gm: 0.006, vg: 0.49,
    });
    const vmResult = await testOracleProgramTally(
      Buffer.from(wasm), Buffer.from("tally"),
      [
        { exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(r1) },
        { exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(r2) },
      ],
    );
    expect(vmResult.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vmResult.result).toString("utf-8"));

    expect(r.ex).toBe(2);
    expect(r.ok).toBe(2);
    expect(r.dvol).toBeCloseTo(57.5, 0);

    console.log("Multi-executor:", JSON.stringify(r, null, 2));
  });
});
