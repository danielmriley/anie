import test from 'node:test';
import assert from 'node:assert/strict';

import type { RunEvent } from '../src/shared/domain.js';
import { initialSession, reduce } from '../src/shared/session.js';
import type { Effect, Session, SessionInput } from '../src/shared/session.js';

function run(session: Session, ...inputs: SessionInput[]): { session: Session; effects: Effect[] } {
	const effects: Effect[] = [];
	let current = session;
	for (const input of inputs) {
		const [next, produced] = reduce(current, input);
		current = next;
		effects.push(...produced);
	}
	return { session: current, effects };
}

function ev(event: RunEvent): SessionInput {
	return { kind: 'run_event', event };
}

test('idle prompt appends a user item and sends it', () => {
	const { session, effects } = run(initialSession, { kind: 'user_submitted', text: 'hello' });
	assert.deepEqual(session.items, [{ id: 1, kind: 'user', text: 'hello' }]);
	assert.deepEqual(session.phase, { kind: 'idle' });
	assert.deepEqual(effects, [{ kind: 'send', command: { kind: 'prompt', text: 'hello' } }]);
	assert.equal(session.busyHint, false);
});

test('mid-run prompt keeps the draft, sends nothing, and sets the busy hint', () => {
	const { session, effects } = run(
		initialSession,
		ev({ kind: 'run_started' }),
		{ kind: 'user_submitted', text: 'second prompt' },
	);
	assert.deepEqual(session.items, []);
	assert.deepEqual(session.phase, { kind: 'running', abortRequested: false, sawOutput: false });
	assert.deepEqual(effects, []);
	assert.equal(session.busyHint, true);
});

test('text deltas accumulate into one streaming assistant item that ends done on run_ended', () => {
	const { session, effects } = run(
		initialSession,
		ev({ kind: 'run_started' }),
		ev({ kind: 'text', text: 'Hello ' }),
		ev({ kind: 'text', text: 'from ' }),
		ev({ kind: 'text', text: 'fake anie.' }),
		ev({ kind: 'run_ended' }),
	);
	assert.deepEqual(session.items, [
		{ id: 1, kind: 'assistant', text: 'Hello from fake anie.', state: { kind: 'done' } },
	]);
	assert.deepEqual(session.phase, { kind: 'idle' });
	assert.deepEqual(effects, []);
});

test('assistant_error closes the run with an error item carrying the message', () => {
	const { session, effects } = run(
		initialSession,
		ev({ kind: 'run_started' }),
		ev({ kind: 'assistant_error', message: 'Authentication failed: fake API key is missing' }),
		ev({ kind: 'run_ended' }),
	);
	assert.deepEqual(session.items, [
		{
			id: 1,
			kind: 'assistant',
			text: '',
			state: { kind: 'error', message: 'Authentication failed: fake API key is missing' },
		},
	]);
	assert.deepEqual(session.phase, { kind: 'idle' });
	assert.deepEqual(effects, []);
});

test('tool_started opens a running tool item and tool_finished by name marks it done', () => {
	const { session, effects } = run(
		initialSession,
		ev({ kind: 'run_started' }),
		ev({ kind: 'tool_started', tool: 'read', args: { path: 'src/main.rs' } }),
		ev({ kind: 'tool_finished', tool: 'read', isError: false }),
	);
	assert.deepEqual(session.items, [
		{
			id: 1,
			kind: 'tool',
			name: 'read',
			args: { path: 'src/main.rs' },
			state: { kind: 'done', isError: false },
		},
	]);
	assert.deepEqual(session.phase, { kind: 'running', abortRequested: false, sawOutput: true });
	assert.deepEqual(effects, []);
});

test('tool_finished with null tool closes the oldest running tool', () => {
	const { session, effects } = run(
		initialSession,
		ev({ kind: 'run_started' }),
		ev({ kind: 'tool_started', tool: 'read', args: { path: 'a' } }),
		ev({ kind: 'tool_started', tool: 'bash', args: { cmd: 'ls' } }),
		ev({ kind: 'tool_finished', tool: null, isError: true }),
	);
	assert.deepEqual(session.items, [
		{ id: 1, kind: 'tool', name: 'read', args: { path: 'a' }, state: { kind: 'done', isError: true } },
		{ id: 2, kind: 'tool', name: 'bash', args: { cmd: 'ls' }, state: { kind: 'running' } },
	]);
	assert.deepEqual(session.phase, { kind: 'running', abortRequested: false, sawOutput: true });
	assert.deepEqual(effects, []);
});

test('run_ended with no output and no abort appends the no-response error item', () => {
	const { session, effects } = run(initialSession, ev({ kind: 'run_started' }), ev({ kind: 'run_ended' }));
	assert.deepEqual(session.items, [
		{ id: 1, kind: 'assistant', text: '', state: { kind: 'error', message: 'anie finished without a response' } },
	]);
	assert.deepEqual(session.phase, { kind: 'idle' });
	assert.deepEqual(effects, []);
});

test('abort while running sends abort and marks the item aborted on run_ended with partial text kept', () => {
	const { session, effects } = run(
		initialSession,
		ev({ kind: 'run_started' }),
		ev({ kind: 'text', text: 'partial response' }),
		{ kind: 'user_aborted' },
		ev({ kind: 'run_ended' }),
	);
	assert.deepEqual(session.items, [
		{ id: 1, kind: 'assistant', text: 'partial response', state: { kind: 'aborted' } },
	]);
	assert.deepEqual(session.phase, { kind: 'idle' });
	assert.deepEqual(effects, [{ kind: 'send', command: { kind: 'abort' } }]);
});

test('abort while retry_pending returns to idle and sends abort', () => {
	const { session, effects } = run(
		initialSession,
		ev({ kind: 'retry_scheduled', attempt: 1, maxRetries: 3, delayMs: 200, error: 'Transport error: dns' }),
		{ kind: 'user_aborted' },
	);
	assert.deepEqual(session.items, [
		{ id: 1, kind: 'retry', attempt: 1, maxRetries: 3, delayMs: 200, error: 'Transport error: dns' },
	]);
	assert.deepEqual(session.phase, { kind: 'idle' });
	assert.deepEqual(effects, [{ kind: 'send', command: { kind: 'abort' } }]);
});

test('retry_scheduled keeps the error item and adds a retry item then run_started resumes running', () => {
	const { session, effects } = run(
		initialSession,
		ev({ kind: 'run_started' }),
		ev({ kind: 'assistant_error', message: 'Transport error: dns' }),
		ev({ kind: 'run_ended' }),
		ev({ kind: 'retry_scheduled', attempt: 1, maxRetries: 3, delayMs: 200, error: 'Transport error: dns' }),
		ev({ kind: 'run_started' }),
	);
	assert.deepEqual(session.items, [
		{ id: 1, kind: 'assistant', text: '', state: { kind: 'error', message: 'Transport error: dns' } },
		{ id: 2, kind: 'retry', attempt: 1, maxRetries: 3, delayMs: 200, error: 'Transport error: dns' },
	]);
	assert.deepEqual(session.phase, { kind: 'running', abortRequested: false, sawOutput: false });
	assert.deepEqual(effects, []);
});

test('compaction_finished fills the open compaction item', () => {
	const { session, effects } = run(
		initialSession,
		ev({ kind: 'compaction_started', phase: 'pre_prompt' }),
		ev({
			kind: 'compaction_finished',
			phase: 'pre_prompt',
			summary: 's',
			tokensBefore: 12000,
			tokensAfter: 4000,
		}),
	);
	assert.deepEqual(session.items, [
		{
			id: 1,
			kind: 'compaction',
			phase: 'pre_prompt',
			result: { summary: 's', tokensBefore: 12000, tokensAfter: 4000 },
		},
	]);
	assert.deepEqual(session.phase, { kind: 'idle' });
	assert.deepEqual(effects, []);
});

test('process_started resets to the initial session', () => {
	const dirty = run(initialSession, { kind: 'user_submitted', text: 'hello' }).session;
	const { session, effects } = run(dirty, { kind: 'process_started' });
	assert.deepEqual(session, initialSession);
	assert.deepEqual(session.items, []);
	assert.deepEqual(session.phase, { kind: 'idle' });
	assert.deepEqual(effects, []);
});

test('protocol_error becomes a warn notice', () => {
	const { session, effects } = run(
		initialSession,
		ev({ kind: 'protocol_error', message: 'invalid command: expected ident at line 1 column 2' }),
	);
	assert.deepEqual(session.items, [
		{
			id: 1,
			kind: 'notice',
			text: 'anie rejected a command: invalid command: expected ident at line 1 column 2',
			tone: 'warn',
		},
	]);
	assert.deepEqual(session.phase, { kind: 'idle' });
	assert.deepEqual(effects, []);
});
