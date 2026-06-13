import { describe, expect, test } from "bun:test";

import { buildSrtlaSendArgs } from "./index.js";

describe("buildSrtlaSendArgs", () => {
	test("buildSrtlaSendArgs_positional_order", () => {
		const args = buildSrtlaSendArgs({
			listenPort: 6000,
			srtlaHost: "host",
			srtlaPort: 5000,
			ipsFile: "ips",
			verbose: true,
			statsFile: "/p",
			statsFileInterval: 1000,
		});
		expect(args).toEqual([
			"6000",
			"host",
			"5000",
			"ips",
			"--verbose",
			"--stats-file",
			"/p",
			"--stats-file-interval",
			"1000",
		]);
	});

	test("buildSrtlaSendArgs_minimal", () => {
		const args = buildSrtlaSendArgs({
			listenPort: 6000,
			srtlaHost: "host",
			srtlaPort: 5000,
			ipsFile: "ips",
		});
		expect(args).toEqual(["6000", "host", "5000", "ips"]);
	});
});
