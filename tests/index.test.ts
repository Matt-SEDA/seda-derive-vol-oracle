import { afterEach, describe, it, expect, mock } from "bun:test";
import { file } from "bun";
import { testOracleProgramExecution, testOracleProgramTally } from "@seda-protocol/dev-tools";

const WASM_PATH = "target/wasm32-wasip1/release-wasm/derive-vol-oracle.wasm";
const fetchMock = mock();
afterEach(() => { fetchMock.mockRestore(); });

const PYTH_ETH = { parsed: [{ price: { price: "165413000000", conf: "5000000", expo: -8, publish_time: 1750000000 } }] };
const COINBASE_ETH = { data: { amount: "1654.50" } };
const KRAKEN_ETH = { result: { XETHZUSD: { c: ["1654.20", "1.0"] } } };
const DVOL = { result: { data: [[1750000000000, 55.2, 58.1, 54.0, 57.8]] } };
const HIST_RV = { result: [[1749900000000, 45.2], [1750000000000, 48.7]] };

function mockApis() {
  fetchMock.mockImplementation((...args: any[]) => {
    const u = String(args[0] || "");
    if (u.includes("hermes.pyth.network")) return new Response(JSON.stringify(PYTH_ETH));
    if (u.includes("coinbase")) return new Response(JSON.stringify(COINBASE_ETH));
    if (u.includes("kraken")) return new Response(JSON.stringify(KRAKEN_ETH));
    if (u.includes("get_volatility_index")) return new Response(JSON.stringify(DVOL));
    if (u.includes("get_historical_volatility")) return new Response(JSON.stringify(HIST_RV));
    return new Response(JSON.stringify(PYTH_ETH));
  });
}

describe("Vol Oracle - execution", () => {
  it("should fetch 3 spot sources + DVOL + RV", async () => {
    mockApis();
    const wasm = await file(WASM_PATH).arrayBuffer();
    const vm = await testOracleProgramExecution(Buffer.from(wasm), Buffer.from(JSON.stringify({
      currency: "ETH", pyth_feed_id: "0xff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace", spot_asset: "ETH-USD",
    })), fetchMock);
    expect(vm.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vm.result).toString("utf-8"));
    expect(r.cy).toBe("ETH");
    expect(r.sp.length).toBe(3);
    expect(r.dv).toBeGreaterThan(0);
    expect(r.rv).toBeGreaterThan(0);
    console.log("Exec:", JSON.stringify(r, null, 2));
  });
});

describe("Vol Oracle - tally", () => {
  it("NORMAL regime with positive VRP", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();
    const reveal = JSON.stringify({ cy: "ETH", sp: [1654_130_000, 1654_500_000, 1654_200_000], sn: ["Pyth","Coinbase","Kraken"], dv: 57.8, rv: 48.7 });
    const vm = await testOracleProgramTally(Buffer.from(wasm), Buffer.from("t"),
      [{ exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(reveal) }]);
    expect(vm.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vm.result).toString("utf-8"));
    expect(r.regime).toBe("NORMAL");
    expect(r.vrp).toBeGreaterThan(0);
    expect(r.spot).toBeGreaterThan(1650);
    console.log("NORMAL:", JSON.stringify(r, null, 2));
  });

  it("CRISIS regime when DVOL > 80", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();
    const reveal = JSON.stringify({ cy: "ETH", sp: [1654_000_000], sn: ["Pyth"], dv: 92.0, rv: 88.0 });
    const vm = await testOracleProgramTally(Buffer.from(wasm), Buffer.from("t"),
      [{ exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(reveal) }]);
    expect(vm.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vm.result).toString("utf-8"));
    expect(r.regime).toBe("CRISIS");
    expect(r.rscore).toBeGreaterThanOrEqual(75);
    console.log("CRISIS:", JSON.stringify(r, null, 2));
  });

  it("multi-executor consensus", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();
    const r1 = JSON.stringify({ cy: "ETH", sp: [1654_000_000, 1654_500_000], sn: ["Pyth","Coinbase"], dv: 57.0, rv: 49.0 });
    const r2 = JSON.stringify({ cy: "ETH", sp: [1654_200_000, 1654_400_000], sn: ["Pyth","Coinbase"], dv: 58.0, rv: 48.5 });
    const vm = await testOracleProgramTally(Buffer.from(wasm), Buffer.from("t"), [
      { exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(r1) },
      { exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(r2) },
    ]);
    expect(vm.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vm.result).toString("utf-8"));
    expect(r.ex).toBe(2);
    expect(r.dvol).toBeCloseTo(57.5, 0);
    console.log("Multi:", JSON.stringify(r, null, 2));
  });
});
