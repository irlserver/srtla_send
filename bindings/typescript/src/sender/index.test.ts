import { describe, expect, test } from 'bun:test';

import { buildSrtlaSendArgs } from './index.js';

describe('buildSrtlaSendArgs', () => {
	test('buildSrtlaSendArgs_positional_order', () => {
		const args = buildSrtlaSendArgs({
			listenPort: 6000,
			srtlaHost: 'host',
			srtlaPort: 5000,
			ipsFile: 'ips',
			verbose: true,
			statsFile: '/p',
			statsFileInterval: 1000,
		});
		expect(args).toEqual([
			'6000',
			'host',
			'5000',
			'ips',
			'--verbose',
			'--stats-file',
			'/p',
			'--stats-file-interval',
			'1000',
		]);
	});

	test('buildSrtlaSendArgs_minimal', () => {
		const args = buildSrtlaSendArgs({
			listenPort: 6000,
			srtlaHost: 'host',
			srtlaPort: 5000,
			ipsFile: 'ips',
		});
		expect(args).toEqual(['6000', 'host', '5000', 'ips']);
	});

	test('buildSrtlaSendArgs_applies_defaults', () => {
		const args = buildSrtlaSendArgs({ srtlaHost: 'relay.example.com' });
		expect(args).toEqual(['5000', 'relay.example.com', '5001', '/tmp/srtla_ips']);
	});

	test('buildSrtlaSendArgs_verbose_only', () => {
		const args = buildSrtlaSendArgs({ srtlaHost: 'host', verbose: true });
		expect(args).toEqual(['5000', 'host', '5001', '/tmp/srtla_ips', '--verbose']);
	});

	test('buildSrtlaSendArgs_stats_file_emitted', () => {
		const args = buildSrtlaSendArgs({
			srtlaHost: 'host',
			statsFile: '/tmp/srtla-send-stats-5000.json',
		});
		const idx = args.indexOf('--stats-file');
		expect(idx).toBeGreaterThanOrEqual(0);
		expect(args[idx + 1]).toBe('/tmp/srtla-send-stats-5000.json');
	});

	test('buildSrtlaSendArgs_stats_file_omitted_when_unset', () => {
		const args = buildSrtlaSendArgs({ srtlaHost: 'host' });
		expect(args).not.toContain('--stats-file');
		expect(args).not.toContain('--stats-file-interval');
	});

	test('buildSrtlaSendArgs_stats_file_interval_emitted', () => {
		const args = buildSrtlaSendArgs({
			srtlaHost: 'host',
			statsFile: '/p',
			statsFileInterval: 2000,
		});
		const idx = args.indexOf('--stats-file-interval');
		expect(idx).toBeGreaterThanOrEqual(0);
		expect(args[idx + 1]).toBe('2000');
	});
});
