import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';

import { AnieProcess, decodeLine, encodeCommand, type Decoded } from '../src/main/anie.js';
import type { ProcessState, Push } from '../src/shared/domain.js';

const fakePath = path.resolve(import.meta.dirname, '..', '..', 'test', 'fake-anie.mjs');

const decodeCases: Array<[string, string, Decoded]> = [
	['hello is decoded with its version', '{"type":"hello","version":1}', { kind: 'hello', version: 1 }],
	[
		'status maps snake_case wire fields onto SessionStatus',
		'{"type":"status","provider":"openai","model":"gpt-4o","thinking":"medium","estimated_context_tokens":0,"context_window":128000,"cwd":"/tmp","session_id":"135630b0"}',
		{
			kind: 'event',
			event: {
				kind: 'status',
				status: {
					provider: 'openai',
					model: 'gpt-4o',
					thinking: 'medium',
					estimatedContextTokens: 0,
					contextWindow: 128000,
					cwd: '/tmp',
					sessionId: '135630b0',
				},
			},
		},
	],
	['empty system text is noise', '{"type":"system","text":""}', { kind: 'noise' }],
	[
		'non-empty system text is a notice',
		'{"type":"system","text":"Session: 135630b0"}',
		{ kind: 'event', event: { kind: 'notice', text: 'Session: 135630b0' } },
	],
	[
		'error line is a protocol_error',
		'{"type":"error","message":"invalid command: expected ident at line 1 column 2"}',
		{
			kind: 'event',
			event: { kind: 'protocol_error', message: 'invalid command: expected ident at line 1 column 2' },
		},
	],
	['agent_start is run_started', '{"type":"agent_start"}', { kind: 'event', event: { kind: 'run_started' } }],
	['agent_end is run_ended', '{"type":"agent_end"}', { kind: 'event', event: { kind: 'run_ended' } }],
	['text_delta is text', '{"type":"text_delta","text":"Hi"}', { kind: 'event', event: { kind: 'text', text: 'Hi' } }],
	[
		'tool_exec_start keeps the tool name and args',
		'{"type":"tool_exec_start","tool":"read","args":{"path":"a"}}',
		{ kind: 'event', event: { kind: 'tool_started', tool: 'read', args: { path: 'a' } } },
	],
	[
		'empty tool_exec_end tool becomes null',
		'{"type":"tool_exec_end","tool":"","is_error":true}',
		{ kind: 'event', event: { kind: 'tool_finished', tool: null, isError: true } },
	],
	[
		'assistant_error carries the message',
		'{"type":"assistant_error","message":"Authentication failed: x"}',
		{ kind: 'event', event: { kind: 'assistant_error', message: 'Authentication failed: x' } },
	],
	[
		'retry_scheduled maps snake_case delay and retry fields',
		'{"type":"retry_scheduled","attempt":1,"max_retries":3,"delay_ms":1000,"error":"Transport error: dns"}',
		{
			kind: 'event',
			event: { kind: 'retry_scheduled', attempt: 1, maxRetries: 3, delayMs: 1000, error: 'Transport error: dns' },
		},
	],
	[
		'compaction_start is compaction_started',
		'{"type":"compaction_start","phase":"pre_prompt"}',
		{ kind: 'event', event: { kind: 'compaction_started', phase: 'pre_prompt' } },
	],
	[
		'compaction_end is compaction_finished with token counts',
		'{"type":"compaction_end","phase":"mid_turn","summary":"s","tokens_before":12000,"tokens_after":4000}',
		{
			kind: 'event',
			event: {
				kind: 'compaction_finished',
				phase: 'mid_turn',
				summary: 's',
				tokensBefore: 12000,
				tokensAfter: 4000,
			},
		},
	],
	['transcript_replace is noise', '{"type":"transcript_replace","messages":[]}', { kind: 'noise' }],
	['a non-JSON line is malformed', 'not json', { kind: 'malformed', line: 'not json' }],
	['an unknown type is malformed', '{"type":"bogus"}', { kind: 'malformed', line: '{"type":"bogus"}' }],
	[
		'status with missing fields is malformed',
		'{"type":"status","provider":"x"}',
		{ kind: 'malformed', line: '{"type":"status","provider":"x"}' },
	],
];

for (const [name, line, expected] of decodeCases) {
	test(name, () => {
		assert.deepEqual(decodeLine(line), expected);
	});
}

test('encodeCommand emits one JSON line with a trailing newline for each wire command', () => {
	assert.equal(encodeCommand({ kind: 'prompt', text: 'hi' }), '{"type":"prompt","text":"hi"}\n');
	assert.equal(encodeCommand({ kind: 'abort' }), '{"type":"abort"}\n');
	assert.equal(encodeCommand({ kind: 'set_thinking', level: 'low' }), '{"type":"set_thinking","level":"low"}\n');
	assert.equal(encodeCommand({ kind: 'set_model', model: 'gpt-4o' }), '{"type":"set_model","model":"gpt-4o"}\n');
});

async function waitForProcess(
	pushes: Push[],
	predicate: (event: ProcessState) => boolean,
	timeoutMs: number,
	label: string,
): Promise<ProcessState> {
	const deadline = Date.now() + timeoutMs;
	for (;;) {
		for (const push of pushes) {
			if (push.channel === 'process' && predicate(push.event)) return push.event;
		}
		if (Date.now() >= deadline) {
			throw new Error(`${label}: ${JSON.stringify(pushes)}`);
		}
		await sleep(20);
	}
}

test('AnieProcess becomes ready after hello and stop closes stdin so the child records stdin-eof', async () => {
	const pushes: Push[] = [];
	const cwd = await mkdtemp(path.join(os.tmpdir(), 'anie-process-ready-'));
	const exitFile = path.join(cwd, 'exit.json');
	const anie = new AnieProcess((push) => pushes.push(push), {
		binary: fakePath,
		env: { ...process.env, FAKE_ANIE_EXIT_FILE: exitFile },
	});
	try {
		anie.start({ cwd });
		await waitForProcess(pushes, (event) => event.phase === 'ready', 5000, 'timed out waiting for ready');
		const statusDeadline = Date.now() + 2000;
		while (anie.lastStatus?.sessionId !== 'fake0001' && Date.now() < statusDeadline) {
			await sleep(20);
		}
		assert.equal(anie.lastStatus?.sessionId, 'fake0001');
		const exit = await anie.stop();
		assert.equal(exit.code, 0);
		assert.deepEqual(JSON.parse(await readFile(exitFile, 'utf8')), { reason: 'stdin-eof', cwd });
	} finally {
		await anie.stop();
	}
});

test('AnieProcess reports failed_to_start exited_before_hello when the child dies before hello', async () => {
	const pushes: Push[] = [];
	const cwd = await mkdtemp(path.join(os.tmpdir(), 'anie-process-nohello-'));
	const anie = new AnieProcess((push) => pushes.push(push), {
		binary: fakePath,
		env: { ...process.env, FAKE_ANIE_MODE: 'no-hello' },
	});
	try {
		anie.start({ cwd });
		const event = await waitForProcess(
			pushes,
			(candidate) => candidate.phase === 'failed_to_start',
			5000,
			'timed out waiting for failed_to_start',
		);
		if (event.phase !== 'failed_to_start') assert.fail(JSON.stringify(event));
		assert.equal(event.failure.kind, 'exited_before_hello');
		if (event.failure.kind !== 'exited_before_hello') assert.fail(JSON.stringify(event.failure));
		assert.ok(event.failure.exit.stderrTail.includes('home directory is not available'));
	} finally {
		await anie.stop();
	}
});

test('AnieProcess reports failed_to_start binary_not_found for a missing binary', async () => {
	const pushes: Push[] = [];
	const cwd = await mkdtemp(path.join(os.tmpdir(), 'anie-process-missing-'));
	const anie = new AnieProcess((push) => pushes.push(push), {
		env: { ...process.env, ANIE_DESKTOP_BIN: '/nonexistent/anie' },
	});
	try {
		anie.start({ cwd });
		const event = await waitForProcess(
			pushes,
			(candidate) => candidate.phase === 'failed_to_start',
			5000,
			'timed out waiting for failed_to_start',
		);
		if (event.phase !== 'failed_to_start') assert.fail(JSON.stringify(event));
		assert.deepEqual(event.failure, { kind: 'binary_not_found', searched: ['/nonexistent/anie'] });
	} finally {
		await anie.stop();
	}
});
