import { afterEach, describe, it, expect, mock } from "bun:test";
import { file } from "bun";
import { testOracleProgramExecution, testOracleProgramTally } from "@seda-protocol/dev-tools";

const WASM_PATH = "target/wasm32-wasip1/release-wasm/derive-vol-oracle.wasm";

const fetchMock = mock();

afterEach(() => {
  fetchMock.mockRestore();
});

// --- Mock responses ---

const PYTH_ETH = {
  parsed: [{ price: { price: "165413000000", conf: "5000000", expo: -8, publish_time: 1750000000 } }],
};

const COINBASE_ETH = { data: { amount: "1654.50" } };

const KRAKEN_ETH = { result: { XETHZUSD: { c: ["1654.20", "1.0"] } } };

const DVOL_RESPONSE = {
  result: {
    data: [
      [1750000000000, 55.2, 58.1, 54.0, 57.8],
    ],
  },
};

const DVOL_CRISIS = {
  result: {
    data: [
      [1750000000000, 82.0, 95.0, 80.0, 92.5],
    ],
  },
};

const HIST_RV = {
  result: [
    [1749900000000, 45.2],
    [1750000000000, 48.7],
  ],
};

// Simplified option book — 6 options near ATM
function makeOptionBook(underlying: number, atmIv: number) {
  const strikes = [
    { k: underlying * 0.93, type: "P", iv: atmIv + 8 },   // 25d put (OTM put)
    { k: underlying * 0.97, type: "P", iv: atmIv + 3 },   // near ATM put
    { k: underlying * 1.0,  type: "C", iv: atmIv },       // ATM call
    { k: underlying * 1.0,  type: "P", iv: atmIv + 1 },   // ATM put
    { k: underlying * 1.03, type: "C", iv: atmIv - 2 },   // near OTM call
    { k: underlying * 1.07, type: "C", iv: atmIv - 5 },   // 25d call (OTM call)
  ];

  return {
    result: strikes.map((s, i) => ({
      instrument_name: `ETH-27JUN26-${Math.round(s.k)}-${s.type}`,
      mark_iv: s.iv,
      underlying_price: underlying,
      mid_price: 0.02 + i * 0.005,
      open_interest: 500 - i * 50,
    })),
  };
}

const OPTION_BOOK_NORMAL = makeOptionBook(1654, 56);
const OPTION_BOOK_CRISIS = makeOptionBook(1654, 88);

function mockApis(dvol = DVOL_RESPONSE, optionBook = OPTION_BOOK_NORMAL) {
  fetchMock.mockImplementation((...args: any[]) => {
    const urlStr = String(args[0] || "");
    if (urlStr.includes("hermes.pyth.network")) {
      return new Response(JSON.stringify(PYTH_ETH));
    }
    if (urlStr.includes("api.coinbase.com")) {
      return new Response(JSON.stringify(COINBASE_ETH));
    }
    if (urlStr.includes("api.kraken.com")) {
      return new Response(JSON.stringify(KRAKEN_ETH));
    }
    if (urlStr.includes("get_volatility_index_data")) {
      return new Response(JSON.stringify(dvol));
    }
    if (urlStr.includes("get_historical_volatility")) {
      return new Response(JSON.stringify(HIST_RV));
    }
    if (urlStr.includes("get_book_summary_by_currency")) {
      return new Response(JSON.stringify(optionBook));
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

// --- Execution tests ---

describe("Vol Oracle - execution phase", () => {
  it("should fetch spot from 3 sources + DVOL + RV + options", async () => {
    mockApis();
    const wasm = await file(WASM_PATH).arrayBuffer();
    const vmResult = await testOracleProgramExecution(
      Buffer.from(wasm), Buffer.from(makeInput()), fetchMock
    );
    expect(vmResult.exitCode).toBe(0);
    const result = JSON.parse(Buffer.from(vmResult.result).toString("utf-8"));

    expect(result.cy).toBe("ETH");
    expect(result.sp.length).toBe(3);           // 3 spot sources
    expect(result.dv).toBeGreaterThan(0);        // DVOL populated
    expect(result.rv).toBeGreaterThan(0);        // RV populated
    expect(result.opts.length).toBeGreaterThan(0); // options fetched

    console.log("Execution result:", JSON.stringify(result, null, 2));
  });
});

// --- Tally tests ---

describe("Vol Oracle - tally phase", () => {
  it("should produce a NORMAL regime vol surface", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();

    const reveal = JSON.stringify({
      cy: "ETH",
      sp: [1654_130_000, 1654_500_000, 1654_200_000],
      sn: ["Pyth", "Coinbase", "Kraken"],
      dv: 57.8,
      rv: 48.7,
      opts: OPTION_BOOK_NORMAL.result.map((o: any) => ({
        i: o.instrument_name,
        iv: o.mark_iv,
        k: parseFloat(o.instrument_name.split("-")[2]),
        ot: o.instrument_name.split("-")[3],
        dte: 2.0,
        oi: o.open_interest,
        u: o.underlying_price,
      })),
      up: 1654.0,
    });

    const vmResult = await testOracleProgramTally(
      Buffer.from(wasm),
      Buffer.from("tally"),
      [{ exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(reveal) }],
    );

    expect(vmResult.exitCode).toBe(0);
    const result = JSON.parse(Buffer.from(vmResult.result).toString("utf-8"));

    expect(result.cy).toBe("ETH");
    expect(result.spot).toBeGreaterThan(1600);
    expect(result.dvol).toBeCloseTo(57.8, 0);
    expect(result.rv).toBeCloseTo(48.7, 0);
    expect(result.vrp).toBeGreaterThan(0);        // IV > RV = positive premium
    expect(result.atm).toBeGreaterThan(40);        // ATM IV populated
    expect(result.regime).toBe("NORMAL");
    expect(result.rscore).toBeGreaterThan(25);
    expect(result.rscore).toBeLessThan(50);

    console.log("NORMAL surface:", JSON.stringify(result, null, 2));
  });

  it("should detect CRISIS regime when vol is high", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();

    const reveal = JSON.stringify({
      cy: "ETH",
      sp: [1654_130_000],
      sn: ["Pyth"],
      dv: 92.5,   // DVOL > 80 = crisis
      rv: 85.0,
      opts: OPTION_BOOK_CRISIS.result.map((o: any) => ({
        i: o.instrument_name,
        iv: o.mark_iv,
        k: parseFloat(o.instrument_name.split("-")[2]),
        ot: o.instrument_name.split("-")[3],
        dte: 2.0,
        oi: o.open_interest,
        u: o.underlying_price,
      })),
      up: 1654.0,
    });

    const vmResult = await testOracleProgramTally(
      Buffer.from(wasm),
      Buffer.from("tally"),
      [{ exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(reveal) }],
    );

    expect(vmResult.exitCode).toBe(0);
    const result = JSON.parse(Buffer.from(vmResult.result).toString("utf-8"));

    expect(result.regime).toBe("CRISIS");
    expect(result.rscore).toBeGreaterThanOrEqual(75);
    expect(result.dvol).toBeCloseTo(92.5, 0);

    console.log("CRISIS surface:", JSON.stringify(result, null, 2));
  });

  it("should compute positive skew (put premium) from option data", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();

    const reveal = JSON.stringify({
      cy: "ETH",
      sp: [1654_000_000],
      sn: ["Pyth"],
      dv: 57.0,
      rv: 50.0,
      opts: OPTION_BOOK_NORMAL.result.map((o: any) => ({
        i: o.instrument_name,
        iv: o.mark_iv,
        k: parseFloat(o.instrument_name.split("-")[2]),
        ot: o.instrument_name.split("-")[3],
        dte: 2.0,
        oi: o.open_interest,
        u: o.underlying_price,
      })),
      up: 1654.0,
    });

    const vmResult = await testOracleProgramTally(
      Buffer.from(wasm),
      Buffer.from("tally"),
      [{ exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(reveal) }],
    );

    expect(vmResult.exitCode).toBe(0);
    const result = JSON.parse(Buffer.from(vmResult.result).toString("utf-8"));

    // Put IV should be higher than call IV (positive skew = downside protection premium)
    expect(result.skew).toBeGreaterThan(0);
    expect(result.p25).toBeGreaterThan(result.c25);

    console.log("Skew analysis:", JSON.stringify(result, null, 2));
  });
});
