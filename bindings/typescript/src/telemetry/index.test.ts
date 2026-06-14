import { afterEach, describe, expect, spyOn, test } from 'bun:test';

import {
	readTelemetry,
	SENDER_TELEMETRY_PATH_PREFIX,
	SENDER_TELEMETRY_STALE_MS,
	senderTelemetryPath,
	telemetrySchema,
	watchTelemetry,
} from './index.js';

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
				conn_id: '0',
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

describe('telemetrySchema', () => {
	test('readTelemetry_valid_snapshot_parses', async () => {
		const p = await writeSnapshot(freshSnapshot());
		const t = await readTelemetry(p);

		expect(t).not.toBeNull();
		if (t === null) return;
		expect(t.schema_version).toBe(1);
		expect(typeof t.last_updated_ms).toBe('number');
		const c = t.connections[0]!;
		expect(c.conn_id).toBe('0');
		expect(c.rtt_ms).toBe(42);
		expect(c.nak_count).toBe(3);
		expect(c.weight_percent).toBe(85);
		expect(c.window).toBe(8192);
		expect(c.in_flight).toBe(100);
		expect(c.bitrate_bps).toBe(2500000);
	});

	test('requires window and in_flight (frozen contract)', () => {
		const missingWindow = {
			schema_version: 1,
			last_updated_ms: Date.now(),
			connections: [
				{
					conn_id: '0',
					rtt_ms: 1,
					nak_count: 0,
					weight_percent: 100,
					in_flight: 0,
					bitrate_bps: 0,
				},
			],
		};
		expect(telemetrySchema.safeParse(missingWindow).success).toBe(false);

		const missingInFlight = {
			schema_version: 1,
			last_updated_ms: Date.now(),
			connections: [
				{ conn_id: '0', rtt_ms: 1, nak_count: 0, weight_percent: 100, window: 0, bitrate_bps: 0 },
			],
		};
		expect(telemetrySchema.safeParse(missingInFlight).success).toBe(false);
	});
});

describe('readTelemetry', () => {
	test('readTelemetry_schema_version_2_rejected', async () => {
		const p = await writeSnapshot(
			JSON.stringify({ schema_version: 2, last_updated_ms: Date.now(), connections: [] }),
		);
		expect(await readTelemetry(p)).toBeNull();
	});

	test('missing schema_version is rejected (no silent strip)', async () => {
		const p = await writeSnapshot(JSON.stringify({ last_updated_ms: Date.now(), connections: [] }));
		expect(await readTelemetry(p)).toBeNull();
	});

	test('readTelemetry_absent_file_returns_null', async () => {
		const p = `/tmp/srtla-send-tel-absent-${Date.now()}-${Math.random().toString(36).slice(2)}.json`;
		expect(await readTelemetry(p)).toBeNull();
	});

	test('invalid JSON returns null (no throw)', async () => {
		const p = await writeSnapshot('{ this is not json');
		expect(await readTelemetry(p)).toBeNull();
	});

	test('idle snapshot (connections: []) returns the object, not null', async () => {
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

describe('watchTelemetry', () => {
	test('reports stale=true for an absent file', async () => {
		const p = `/tmp/srtla-send-tel-watch-absent-${Date.now()}-${Math.random().toString(36).slice(2)}.json`;
		const updates: Array<{ data: unknown; stale: boolean }> = [];
		const handle = watchTelemetry(
			p,
			(u) => {
				updates.push(u);
			},
			{ intervalMs: 20 },
		);
		await sleep(50);
		handle.stop();
		expect(updates.length).toBeGreaterThanOrEqual(1);
		expect(updates.every((u) => u.data === null && u.stale)).toBe(true);
	});

	test('reports stale=false for a fresh snapshot, stale=true past the threshold', async () => {
		const FIXED = 1_800_000_000_000;
		const nowSpy = spyOn(Date, 'now').mockReturnValue(FIXED);
		try {
			const fresh = await writeSnapshot(
				JSON.stringify({ schema_version: 1, last_updated_ms: FIXED, connections: [] }),
			);
			const freshUpdates: Array<{ stale: boolean }> = [];
			const h1 = watchTelemetry(
				fresh,
				(u) => {
					freshUpdates.push(u);
				},
				{ intervalMs: 1000 },
			);
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
			const h2 = watchTelemetry(
				stale,
				(u) => {
					staleUpdates.push(u);
				},
				{ intervalMs: 1000 },
			);
			await sleep(20);
			h2.stop();
			expect(staleUpdates[0]?.data).not.toBeNull();
			expect(staleUpdates[0]?.stale).toBe(true);
		} finally {
			nowSpy.mockRestore();
		}
	});

	test('stop() halts further callbacks', async () => {
		const p = await writeSnapshot(freshSnapshot());
		let calls = 0;
		const handle = watchTelemetry(
			p,
			() => {
				calls++;
			},
			{ intervalMs: 20 },
		);
		await sleep(50);
		handle.stop();
		const afterStop = calls;
		await sleep(80);
		expect(calls).toBe(afterStop);
	});
});

describe('senderTelemetryPath', () => {
	test('builds the well-known path for a listen port', () => {
		expect(senderTelemetryPath(5000)).toBe(`${SENDER_TELEMETRY_PATH_PREFIX}5000.json`);
		expect(senderTelemetryPath(9000)).toBe('/tmp/srtla-send-stats-9000.json');
	});
});

// Real Rust-producer output (Task 10) copied into this package's own fixtures so
// the test never reads above its repo root (Rule D). The reader must round-trip it.
const GOLDEN_FIXTURE_PATH = `${import.meta.dir}/../../tests/fixtures/telemetry-golden.json`;

describe('round-trip golden fixture (T21)', () => {
	test('round_trip_golden_fixture', async () => {
		const t = await readTelemetry(GOLDEN_FIXTURE_PATH);
		expect(t).not.toBeNull();
		if (t === null) return;

		expect(t.schema_version).toBe(1);
		expect(t.last_updated_ms).toBe(1749556546000);
		expect(t.connections).toEqual([
			{
				conn_id: '0',
				rtt_ms: 42,
				nak_count: 3,
				weight_percent: 85,
				window: 8192,
				in_flight: 100,
				bitrate_bps: 2500000,
			},
			{
				conn_id: '1',
				rtt_ms: 73,
				nak_count: 11,
				weight_percent: 55,
				window: 4096,
				in_flight: 240,
				bitrate_bps: 1200000,
			},
		]);
	});

	test('bitrate_x8_invariant', async () => {
		// bitrate_bps = wire_bytes_per_sec × 8, applied exactly once by the producer.
		// conn0's 2_500_000 bps is 312_500 B/s × 8; assert the fixture carries the
		// post-conversion value, not the raw byte rate.
		const t = await readTelemetry(GOLDEN_FIXTURE_PATH);
		expect(t).not.toBeNull();
		if (t === null) return;
		expect(312500 * 8).toBe(2_500_000);
		expect(t.connections[0]?.bitrate_bps).toBe(2_500_000);
		// conn1: 150_000 B/s × 8 = 1_200_000 bps.
		expect(150000 * 8).toBe(1_200_000);
		expect(t.connections[1]?.bitrate_bps).toBe(1_200_000);
	});

	test('golden fixture is newline-free single object (atomic-publish shape)', async () => {
		const raw = await Bun.file(GOLDEN_FIXTURE_PATH).text();
		expect(raw.includes('\n')).toBe(false);
		expect(JSON.parse(raw)).toBeInstanceOf(Object);
	});
});

describe('watch states (T21)', () => {
	test('watch_state_fresh', async () => {
		const FIXED = 1_800_000_000_000;
		const nowSpy = spyOn(Date, 'now').mockReturnValue(FIXED);
		try {
			const p = await writeSnapshot(
				JSON.stringify({
					schema_version: 1,
					last_updated_ms: FIXED,
					connections: [
						{
							conn_id: '0',
							rtt_ms: 42,
							nak_count: 3,
							weight_percent: 85,
							window: 8192,
							in_flight: 100,
							bitrate_bps: 2500000,
						},
					],
				}),
			);
			const updates: Array<{ data: unknown; stale: boolean }> = [];
			const handle = watchTelemetry(
				p,
				(u) => {
					updates.push(u);
				},
				{ intervalMs: 1000 },
			);
			await sleep(20);
			handle.stop();
			expect(updates[0]?.data).not.toBeNull();
			expect(updates[0]?.stale).toBe(false);
		} finally {
			nowSpy.mockRestore();
		}
	});

	test('watch_state_stale', async () => {
		const FIXED = 1_800_000_000_000;
		const nowSpy = spyOn(Date, 'now').mockReturnValue(FIXED);
		try {
			const p = await writeSnapshot(
				JSON.stringify({
					schema_version: 1,
					last_updated_ms: FIXED - SENDER_TELEMETRY_STALE_MS - 1,
					connections: [
						{
							conn_id: '0',
							rtt_ms: 42,
							nak_count: 3,
							weight_percent: 85,
							window: 8192,
							in_flight: 100,
							bitrate_bps: 2500000,
						},
					],
				}),
			);
			const updates: Array<{ data: unknown; stale: boolean }> = [];
			const handle = watchTelemetry(
				p,
				(u) => {
					updates.push(u);
				},
				{ intervalMs: 1000 },
			);
			await sleep(20);
			handle.stop();
			expect(updates[0]?.data).not.toBeNull();
			expect(updates[0]?.stale).toBe(true);
		} finally {
			nowSpy.mockRestore();
		}
	});

	test('watch_state_null', async () => {
		const p = `/tmp/srtla-send-tel-watch-null-${Date.now()}-${Math.random().toString(36).slice(2)}.json`;
		expect(await readTelemetry(p)).toBeNull();

		const updates: Array<{ data: unknown; stale: boolean }> = [];
		const handle = watchTelemetry(
			p,
			(u) => {
				updates.push(u);
			},
			{ intervalMs: 20 },
		);
		await sleep(50);
		handle.stop();
		expect(updates.length).toBeGreaterThanOrEqual(1);
		expect(updates.every((u) => u.data === null && u.stale)).toBe(true);
	});
});

describe('schema edge cases (T21)', () => {
	test('schema_version_2_rejected', async () => {
		const p = await writeSnapshot(
			JSON.stringify({ schema_version: 2, last_updated_ms: Date.now(), connections: [] }),
		);
		expect(await readTelemetry(p)).toBeNull();
		expect(
			telemetrySchema.safeParse({ schema_version: 2, last_updated_ms: 0, connections: [] }).success,
		).toBe(false);
	});

	test('missing_required_field_rejected', async () => {
		const missingWindow = {
			schema_version: 1,
			last_updated_ms: Date.now(),
			connections: [
				{
					conn_id: '0',
					rtt_ms: 42,
					nak_count: 3,
					weight_percent: 85,
					in_flight: 100,
					bitrate_bps: 2500000,
				},
			],
		};
		expect(telemetrySchema.safeParse(missingWindow).success).toBe(false);
		const p = await writeSnapshot(JSON.stringify(missingWindow));
		expect(await readTelemetry(p)).toBeNull();
	});

	test('extra_fields_stripped_or_rejected', async () => {
		// Zod's default object semantics strip unknown keys: the parse succeeds and the
		// extra `iface` field (a conn_id→iface enrichment a future producer might add)
		// does not leak into the typed result.
		const withExtra = {
			schema_version: 1,
			last_updated_ms: Date.now(),
			connections: [
				{
					conn_id: '0',
					rtt_ms: 42,
					nak_count: 3,
					weight_percent: 85,
					window: 8192,
					in_flight: 100,
					bitrate_bps: 2500000,
					iface: 'usb0',
				},
			],
		};
		const parsed = telemetrySchema.safeParse(withExtra);
		expect(parsed.success).toBe(true);
		if (!parsed.success) return;
		expect(parsed.data.connections[0]).not.toHaveProperty('iface');

		const p = await writeSnapshot(JSON.stringify(withExtra));
		const t = await readTelemetry(p);
		expect(t).not.toBeNull();
		expect(t?.connections[0]).not.toHaveProperty('iface');
	});
});
