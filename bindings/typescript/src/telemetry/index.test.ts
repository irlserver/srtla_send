import { afterEach, describe, expect, spyOn, test } from "bun:test";

import {
	SENDER_TELEMETRY_PATH_PREFIX,
	SENDER_TELEMETRY_STALE_MS,
	readTelemetry,
	senderTelemetryPath,
	telemetrySchema,
	watchTelemetry,
} from "./index.js";

const created: Array<string> = [];

function tmpPath(): string {
	const p = `/tmp/srtla-send-tel-test-${Date.now()}-${Math.random().toString(36).slice(2)}.json`;
	created.push(p);
	return p;
}

async function writeSnapshot(content: string): Promise<string> {
	const p = tmpPath();
	await Bun.write(p, content);
	return p;
}

afterEach(async () => {
	for (const p of created.splice(0)) {
		try {
			await Bun.file(p).delete?.();
		} catch {
			// best-effort cleanup
		}
	}
});

function freshSnapshot(): string {
	return JSON.stringify({
		schema_version: 1,
		last_updated_ms: Date.now(),
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
	});
}

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

describe("telemetrySchema", () => {
	test("readTelemetry_valid_snapshot_parses", async () => {
		const p = await writeSnapshot(freshSnapshot());
		const t = await readTelemetry(p);

		expect(t).not.toBeNull();
		if (t === null) return;
		expect(t.schema_version).toBe(1);
		expect(typeof t.last_updated_ms).toBe("number");
		const c = t.connections[0]!;
		expect(c.conn_id).toBe("0");
		expect(c.rtt_ms).toBe(42);
		expect(c.nak_count).toBe(3);
		expect(c.weight_percent).toBe(85);
		expect(c.window).toBe(8192);
		expect(c.in_flight).toBe(100);
		expect(c.bitrate_bps).toBe(2500000);
	});

	test("requires window and in_flight (frozen contract)", () => {
		const missingWindow = {
			schema_version: 1,
			last_updated_ms: Date.now(),
			connections: [
				{ conn_id: "0", rtt_ms: 1, nak_count: 0, weight_percent: 100, in_flight: 0, bitrate_bps: 0 },
			],
		};
		expect(telemetrySchema.safeParse(missingWindow).success).toBe(false);

		const missingInFlight = {
			schema_version: 1,
			last_updated_ms: Date.now(),
			connections: [
				{ conn_id: "0", rtt_ms: 1, nak_count: 0, weight_percent: 100, window: 0, bitrate_bps: 0 },
			],
		};
		expect(telemetrySchema.safeParse(missingInFlight).success).toBe(false);
	});
});

describe("readTelemetry", () => {
	test("readTelemetry_schema_version_2_rejected", async () => {
		const p = await writeSnapshot(
			JSON.stringify({ schema_version: 2, last_updated_ms: Date.now(), connections: [] }),
		);
		expect(await readTelemetry(p)).toBeNull();
	});

	test("missing schema_version is rejected (no silent strip)", async () => {
		const p = await writeSnapshot(JSON.stringify({ last_updated_ms: Date.now(), connections: [] }));
		expect(await readTelemetry(p)).toBeNull();
	});

	test("readTelemetry_absent_file_returns_null", async () => {
		const p = `/tmp/srtla-send-tel-absent-${Date.now()}-${Math.random().toString(36).slice(2)}.json`;
		expect(await readTelemetry(p)).toBeNull();
	});

	test("invalid JSON returns null (no throw)", async () => {
		const p = await writeSnapshot("{ this is not json");
		expect(await readTelemetry(p)).toBeNull();
	});

	test("idle snapshot (connections: []) returns the object, not null", async () => {
		const p = await writeSnapshot(
			JSON.stringify({ schema_version: 1, last_updated_ms: Date.now(), connections: [] }),
		);
		const t = await readTelemetry(p);
		expect(t).not.toBeNull();
		expect(t?.connections).toEqual([]);
	});

	test("old-but-valid snapshot still parses (staleness is the watcher's concern)", async () => {
		const p = await writeSnapshot(
			JSON.stringify({ schema_version: 1, last_updated_ms: Date.now() - 60000, connections: [] }),
		);
		expect(await readTelemetry(p)).not.toBeNull();
	});
});

describe("watchTelemetry", () => {
	test("reports stale=true for an absent file", async () => {
		const p = `/tmp/srtla-send-tel-watch-absent-${Date.now()}-${Math.random().toString(36).slice(2)}.json`;
		const updates: Array<{ data: unknown; stale: boolean }> = [];
		const handle = watchTelemetry(p, (u) => { updates.push(u); }, { intervalMs: 20 });
		await sleep(50);
		handle.stop();
		expect(updates.length).toBeGreaterThanOrEqual(1);
		expect(updates.every((u) => u.data === null && u.stale)).toBe(true);
	});

	test("reports stale=false for a fresh snapshot, stale=true past the threshold", async () => {
		const FIXED = 1_800_000_000_000;
		const nowSpy = spyOn(Date, "now").mockReturnValue(FIXED);
		try {
			const fresh = await writeSnapshot(
				JSON.stringify({ schema_version: 1, last_updated_ms: FIXED, connections: [] }),
			);
			const freshUpdates: Array<{ stale: boolean }> = [];
			const h1 = watchTelemetry(fresh, (u) => { freshUpdates.push(u); }, { intervalMs: 1000 });
			await sleep(20);
			h1.stop();
			expect(freshUpdates[0]?.stale).toBe(false);

			const stale = await writeSnapshot(
				JSON.stringify({
					schema_version: 1,
					last_updated_ms: FIXED - SENDER_TELEMETRY_STALE_MS - 1,
					connections: [],
				}),
			);
			const staleUpdates: Array<{ data: unknown; stale: boolean }> = [];
			const h2 = watchTelemetry(stale, (u) => { staleUpdates.push(u); }, { intervalMs: 1000 });
			await sleep(20);
			h2.stop();
			expect(staleUpdates[0]?.data).not.toBeNull();
			expect(staleUpdates[0]?.stale).toBe(true);
		} finally {
			nowSpy.mockRestore();
		}
	});

	test("stop() halts further callbacks", async () => {
		const p = await writeSnapshot(freshSnapshot());
		let calls = 0;
		const handle = watchTelemetry(p, () => { calls++; }, { intervalMs: 20 });
		await sleep(50);
		handle.stop();
		const afterStop = calls;
		await sleep(80);
		expect(calls).toBe(afterStop);
	});
});

describe("senderTelemetryPath", () => {
	test("builds the well-known path for a listen port", () => {
		expect(senderTelemetryPath(5000)).toBe(`${SENDER_TELEMETRY_PATH_PREFIX}5000.json`);
		expect(senderTelemetryPath(9000)).toBe("/tmp/srtla-send-stats-9000.json");
	});
});
