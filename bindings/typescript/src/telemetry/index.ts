import { z } from 'zod';

/**
 * Sender telemetry reader for `srtla-send-rs` (ADR-001, Option A — JSON stats file).
 *
 * The Rust sender (`src/telemetry_file.rs`) publishes a per-uplink snapshot to a
 * stats file via atomic `rename(2)` when started with `--stats-file`. This module
 * is the additive, Bun-native consumer side: it reads that file with `Bun.file()`
 * and validates it against the frozen ADR-001 schema.
 *
 * Export NAMES mirror `@ceralive/srtla`'s `./telemetry` subpath exactly (B2 lock):
 * `readTelemetry`, `watchTelemetry`, `senderTelemetryPath`, `telemetrySchema`,
 * `connectionTelemetrySchema`, `SENDER_TELEMETRY_STALE_MS`,
 * `SENDER_TELEMETRY_PATH_PREFIX`. The schema itself is intentionally STRICTER than
 * `@ceralive/srtla`'s: it validates `schema_version` as a literal `1` (fail-loud on
 * a producer/consumer version mismatch instead of silently stripping the tag) and
 * keeps `window` + `in_flight` mandatory. No Node filesystem or process plumbing.
 */

/**
 * Snapshot age (now − `last_updated_ms`) past which the snapshot is no longer
 * live. Fixed by ADR-001 and matched to the Rust producer's
 * `SENDER_TELEMETRY_STALE_MS`. {@link watchTelemetry} surfaces this as a `stale`
 * flag; {@link readTelemetry} returns the parsed snapshot regardless of age so the
 * caller decides what to do with an old-but-valid document.
 */
export const SENDER_TELEMETRY_STALE_MS = 5000;

/**
 * Well-known path prefix, mirroring the producer's stats-file convention and
 * `@ceralive/srtla`'s `SENDER_TELEMETRY_PATH_PREFIX`. The live file is
 * `<prefix><listen_port>.json`.
 */
export const SENDER_TELEMETRY_PATH_PREFIX = '/tmp/srtla-send-stats-';

/** Default live stats path for a listen port (the Rust producer computes the same). */
export function senderTelemetryPath(listenPort: number): string {
	return `${SENDER_TELEMETRY_PATH_PREFIX}${listenPort}.json`;
}

/**
 * One per-connection record. Field names/units mirror the Rust producer's
 * serialized `ConnRecord` (`src/telemetry_file.rs`). `window` and `in_flight` are
 * REQUIRED — they are part of the frozen telemetry contract and must never be
 * dropped from this schema.
 */
export const connectionTelemetrySchema = z.object({
	/** 0-based uplink index in IP-list order, stringified (stable until SIGHUP reorder). */
	conn_id: z.string(),
	/** Kalman-smoothed round-trip time, milliseconds. */
	rtt_ms: z.number().int().min(0),
	/** Cumulative NAKs attributed to this uplink. */
	nak_count: z.number().int().min(0),
	/** This link's normalized share of selection weight, 0–100. */
	weight_percent: z.number().int().min(0).max(100),
	/** Congestion-window size (required by the frozen contract). */
	window: z.number().int(),
	/** In-flight (sent-but-unacknowledged) packet count (required by the frozen contract). */
	in_flight: z.number().int(),
	/**
	 * Send rate in **bits per second**.
	 *
	 * INVARIANT: `bitrate_bps = wire_bytes_per_sec × 8`. The producer applies the
	 * mandated ×8 bytes/s → bits/s conversion exactly once, at JSON serialization
	 * (`src/telemetry_file.rs::build_telemetry_json`); the raw wire-bytes/s value
	 * never appears on the wire. Consumers must treat this field as bits/s.
	 */
	bitrate_bps: z.number().int().min(0),
});

/**
 * One snapshot object (never NDJSON — the file holds exactly one object).
 *
 * `schema_version` is validated as the literal `1`: a future producer bump fails
 * the parse loudly (`readTelemetry` → `null`) rather than being silently stripped,
 * so a consumer can never misread a re-versioned document as valid.
 */
export const telemetrySchema = z.object({
	schema_version: z.literal(1),
	last_updated_ms: z.number().int().min(0),
	connections: z.array(connectionTelemetrySchema),
});

export type ConnectionTelemetry = z.output<typeof connectionTelemetrySchema>;
export type Telemetry = z.output<typeof telemetrySchema>;

/**
 * Read and validate the sender telemetry snapshot at `path`.
 *
 * Returns `null` (never throws) when the file is absent, unparseable, or fails
 * schema validation — including a `schema_version` mismatch. A schema-valid
 * snapshot is returned as a typed {@link Telemetry} object regardless of age; the
 * "running but idle" `connections: []` case is a valid snapshot, not `null`.
 * Staleness is intentionally NOT folded into this result — see
 * {@link watchTelemetry}, which reports it as a separate `stale` flag.
 *
 * All I/O is Bun-native (`Bun.file`); no Node filesystem or process plumbing.
 */
export async function readTelemetry(path: string): Promise<Telemetry | null> {
	try {
		const file = Bun.file(path);
		if (!(await file.exists())) {
			return null;
		}
		const parsed = telemetrySchema.safeParse(JSON.parse(await file.text()));
		return parsed.success ? parsed.data : null;
	} catch {
		// Malformed/truncated read (atomic rename should prevent it, but guard
		// defensively) — telemetry is best-effort, never fatal.
		return null;
	}
}

/** Payload handed to a {@link watchTelemetry} callback on every tick. */
export interface TelemetryUpdate {
	/** Current snapshot, or `null` if absent/unparseable/schema-invalid. */
	data: Telemetry | null;
	/**
	 * `true` when there is no live snapshot: either `data` is `null`, or its
	 * `last_updated_ms` is older than {@link SENDER_TELEMETRY_STALE_MS}.
	 */
	stale: boolean;
}

export interface WatchTelemetryOptions {
	/** Poll cadence in milliseconds. Default 1000ms (the producer write cadence). */
	intervalMs?: number;
}

export interface WatchTelemetryHandle {
	/** Stop polling. Idempotent; no callback fires after this resolves. */
	stop: () => void;
}

/**
 * Poll `path` on a fixed cadence, invoking `cb` with the current
 * {@link TelemetryUpdate} each tick.
 *
 * Fires once immediately so a consumer gets current state without waiting a full
 * interval, then repeats every `intervalMs` (default 1000ms). The `stale` flag is
 * computed per tick from `last_updated_ms`; an absent/invalid file yields
 * `{ data: null, stale: true }`. Call `stop()` to halt; a read already in flight
 * will not invoke `cb` afterward.
 */
export function watchTelemetry(
	path: string,
	cb: (update: TelemetryUpdate) => void,
	opts: WatchTelemetryOptions = {},
): WatchTelemetryHandle {
	const intervalMs = opts.intervalMs ?? 1000;
	let stopped = false;

	const tick = async (): Promise<void> => {
		const data = await readTelemetry(path);
		if (stopped) {
			return;
		}
		const stale = data === null || Date.now() - data.last_updated_ms > SENDER_TELEMETRY_STALE_MS;
		cb({ data, stale });
	};

	void tick();
	const timer = setInterval(() => {
		void tick();
	}, intervalMs);

	return {
		stop: () => {
			stopped = true;
			clearInterval(timer);
		},
	};
}
