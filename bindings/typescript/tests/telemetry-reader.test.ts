import { describe, expect, test } from "bun:test";

import {
	type Telemetry,
	connectionTelemetrySchema,
	readTelemetry,
	telemetrySchema,
} from "../src/telemetry/index.js";

// The reader is consumed registry-only as `@ceralive/srtla-send/telemetry`. These
// tests exercise it from a repo-local path (Rule D — no `../`-escaping above the
// repo root, no sibling checkout). The golden fixture is this package's own copy
// of real Rust-producer output (ADR-001), kept beside the tests.
const GOLDEN_FIXTURE_PATH = `${import.meta.dir}/fixtures/telemetry-golden.json`;

// A minimal schema-valid snapshot used as the mutation base for malformed cases:
// each negative test starts from this and breaks exactly one thing, so a failure
// pins the single field under test rather than an incidental defect.
function baseSnapshot(): Telemetry {
	return {
		schema_version: 1,
		last_updated_ms: 1749556546000,
		connections: [
			{
				conn_id: "0",
				rtt_ms: 42,
				nak_count: 3,
				weight_percent: 85,
				window: 8192,
				in_flight: 100,
				bitrate_bps: 2500000,
			},
		],
	};
}

async function writeAndRead(value: unknown): Promise<Telemetry | null> {
	const p = `/tmp/srtla-send-task8-${Date.now()}-${Math.random().toString(36).slice(2)}.json`;
	await Bun.write(p, typeof value === "string" ? value : JSON.stringify(value));
	try {
		return await readTelemetry(p);
	} finally {
		await Bun.file(p).delete?.();
	}
}

describe("valid golden fixture → typed ADR-001 shape", () => {
	test("parses every connection with the full per-link field set", async () => {
		const t = await readTelemetry(GOLDEN_FIXTURE_PATH);
		expect(t).not.toBeNull();
		if (t === null) return;

		// Top-level ADR-001 envelope.
		expect(t.schema_version).toBe(1);
		expect(t.last_updated_ms).toBe(1749556546000);
		expect(Array.isArray(t.connections)).toBe(true);
		expect(t.connections).toHaveLength(2);

		// Full per-link contract for both uplinks — conn_id, rtt_ms, nak_count,
		// weight_percent, window, in_flight, bitrate_bps are all REQUIRED.
		expect(t.connections).toEqual([
			{
				conn_id: "0",
				rtt_ms: 42,
				nak_count: 3,
				weight_percent: 85,
				window: 8192,
				in_flight: 100,
				bitrate_bps: 2500000,
			},
			{
				conn_id: "1",
				rtt_ms: 73,
				nak_count: 11,
				weight_percent: 55,
				window: 4096,
				in_flight: 240,
				bitrate_bps: 1200000,
			},
		]);
	});

	test("every required per-link key is present on each connection", async () => {
		const t = await readTelemetry(GOLDEN_FIXTURE_PATH);
		expect(t).not.toBeNull();
		if (t === null) return;
		const required = [
			"conn_id",
			"rtt_ms",
			"nak_count",
			"weight_percent",
			"window",
			"in_flight",
			"bitrate_bps",
		] as const;
		for (const c of t.connections) {
			for (const key of required) {
				expect(c).toHaveProperty(key);
			}
		}
	});

	test("bitrate_bps carries the post-×8 bits/s value, not raw wire bytes/s", async () => {
		// ADR-001 invariant: bitrate_bps = wire_bytes_per_sec × 8, applied exactly
		// once by the producer. Assert the fixture holds the converted value.
		const t = await readTelemetry(GOLDEN_FIXTURE_PATH);
		expect(t).not.toBeNull();
		if (t === null) return;
		expect(312500 * 8).toBe(2_500_000);
		expect(t.connections[0]?.bitrate_bps).toBe(2_500_000);
		expect(150000 * 8).toBe(1_200_000);
		expect(t.connections[1]?.bitrate_bps).toBe(1_200_000);
	});

	test("connectionTelemetrySchema accepts a single golden record", () => {
		const record = baseSnapshot().connections[0]!;
		expect(connectionTelemetrySchema.safeParse(record).success).toBe(true);
	});
});

describe("malformed input → graceful null (never an uncaught throw)", () => {
	test("non-JSON text returns null, does not throw", async () => {
		expect(await writeAndRead("{ this is not json")).toBeNull();
	});

	test("truncated JSON object returns null, does not throw", async () => {
		expect(await writeAndRead('{"schema_version":1,"last_updated_ms":1,')).toBeNull();
	});

	test("empty file returns null", async () => {
		expect(await writeAndRead("")).toBeNull();
	});

	test("JSON that is not an object (array/number/string/null) returns null", async () => {
		expect(await writeAndRead("[]")).toBeNull();
		expect(await writeAndRead("42")).toBeNull();
		expect(await writeAndRead('"a string"')).toBeNull();
		expect(await writeAndRead("null")).toBeNull();
	});

	test("absent file returns null", async () => {
		const p = `/tmp/srtla-send-task8-absent-${Date.now()}-${Math.random().toString(36).slice(2)}.json`;
		expect(await readTelemetry(p)).toBeNull();
	});

	test.each([
		"conn_id",
		"rtt_ms",
		"nak_count",
		"weight_percent",
		"window",
		"in_flight",
		"bitrate_bps",
	])("missing required per-link field %s → null", async (field) => {
		const snap = baseSnapshot();
		delete (snap.connections[0] as Record<string, unknown>)[field];
		expect(telemetrySchema.safeParse(snap).success).toBe(false);
		expect(await writeAndRead(snap)).toBeNull();
	});

	test("missing top-level connections array → null", async () => {
		expect(await writeAndRead({ schema_version: 1, last_updated_ms: 1 })).toBeNull();
	});

	test("connections present but not an array → null", async () => {
		expect(
			await writeAndRead({ schema_version: 1, last_updated_ms: 1, connections: {} }),
		).toBeNull();
	});

	test("wrong field types → null", async () => {
		const numericConnId = baseSnapshot();
		(numericConnId.connections[0] as Record<string, unknown>).conn_id = 0;
		expect(await writeAndRead(numericConnId)).toBeNull();

		const stringRtt = baseSnapshot();
		(stringRtt.connections[0] as Record<string, unknown>).rtt_ms = "42";
		expect(await writeAndRead(stringRtt)).toBeNull();

		const floatWindow = baseSnapshot();
		(floatWindow.connections[0] as Record<string, unknown>).window = 8192.5;
		expect(await writeAndRead(floatWindow)).toBeNull();
	});

	test("out-of-domain numeric values → null", async () => {
		const negRtt = baseSnapshot();
		negRtt.connections[0]!.rtt_ms = -1;
		expect(await writeAndRead(negRtt)).toBeNull();

		const negBitrate = baseSnapshot();
		negBitrate.connections[0]!.bitrate_bps = -10;
		expect(await writeAndRead(negBitrate)).toBeNull();

		const weightTooHigh = baseSnapshot();
		weightTooHigh.connections[0]!.weight_percent = 101;
		expect(await writeAndRead(weightTooHigh)).toBeNull();

		const negLastUpdated = baseSnapshot();
		negLastUpdated.last_updated_ms = -1;
		expect(await writeAndRead(negLastUpdated)).toBeNull();
	});
});

describe("schema_version mismatch → explicit handling (no silent strip)", () => {
	test("a future schema_version (2) is rejected, not silently accepted", async () => {
		const snap = { ...baseSnapshot(), schema_version: 2 };
		expect(telemetrySchema.safeParse(snap).success).toBe(false);
		expect(await writeAndRead(snap)).toBeNull();
	});

	test("schema_version 0 is rejected", async () => {
		const snap = { ...baseSnapshot(), schema_version: 0 };
		expect(await writeAndRead(snap)).toBeNull();
	});

	test("a missing schema_version is rejected (the C producer's tag-less shape)", async () => {
		const snap = baseSnapshot() as Partial<Telemetry>;
		delete snap.schema_version;
		expect(telemetrySchema.safeParse(snap).success).toBe(false);
		expect(await writeAndRead(snap)).toBeNull();
	});

	test("a non-numeric schema_version is rejected", async () => {
		const snap = { ...baseSnapshot(), schema_version: "1" };
		expect(await writeAndRead(snap)).toBeNull();
	});
});
