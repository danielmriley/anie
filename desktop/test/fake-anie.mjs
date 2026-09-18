#!/usr/bin/env node

import { appendFileSync, writeFileSync } from 'node:fs';
import readline from 'node:readline';

const args = process.argv.slice(2);
const cFlag = args.indexOf('-C');
const cwd = cFlag >= 0 && args[cFlag + 1] !== undefined ? args[cFlag + 1] : process.cwd();

if (process.env.FAKE_ANIE_MODE === 'no-hello') {
	process.stderr.write('home directory is not available\n');
	process.exit(1);
}

const THINKING_LEVELS = ['off', 'minimal', 'low', 'medium', 'high'];
const MODELS = ['gpt-4o', 'o4-mini', 'claude-sonnet-4'];

let thinking = 'medium';
let model = 'gpt-4o';
let running = false;
let holding = false;
let abortResolve = null;
let inFlight = Promise.resolve();

function emit(obj) {
	process.stdout.write(JSON.stringify(obj) + '\n');
}

function delay(ms) {
	return new Promise((resolve) => setTimeout(resolve, ms));
}

function statusLine() {
	return {
		type: 'status',
		provider: 'openai',
		model,
		thinking,
		estimated_context_tokens: 0,
		context_window: 128000,
		cwd,
		session_id: 'fake0001',
	};
}

function track(promise) {
	inFlight = Promise.all([inFlight, promise]).then(() => undefined, () => undefined);
}

function finishExit() {
	const dest = process.env.FAKE_ANIE_EXIT_FILE;
	if (dest) {
		writeFileSync(dest, JSON.stringify({ reason: 'stdin-eof', cwd }));
	}
	process.exit(0);
}

emit({ type: 'hello', version: 1 });
emit(statusLine());
emit({ type: 'system', text: 'Session: fake0001' });

function handleAbort() {
	if (!holding) return;
	holding = false;
	running = false;
	emit({ type: 'agent_end' });
	abortResolve?.();
	abortResolve = null;
}

async function emitBetween(events, gapMs = 15) {
	for (let i = 0; i < events.length; i++) {
		const event = events[i];
		emit(event);
		if (event.type === 'agent_end') running = false;
		if (i < events.length - 1) await delay(gapMs);
	}
}

async function handlePrompt(text) {
	if (running) {
		emit({ type: 'system', text: 'A run is already active. Press Ctrl+C to abort it first.' });
		return;
	}
	running = true;

	if (text === 'stream') {
		await emitBetween([
			{ type: 'agent_start' },
			{ type: 'system', text: '' },
			{ type: 'system', text: '' },
			{ type: 'text_delta', text: 'Hello ' },
			{ type: 'text_delta', text: 'from ' },
			{ type: 'text_delta', text: 'fake anie.' },
			{ type: 'agent_end' },
		]);
		return;
	}

	if (text === 'tool') {
		await emitBetween([
			{ type: 'agent_start' },
			{ type: 'tool_exec_start', tool: 'read', args: { path: 'src/main.rs' } },
			{ type: 'tool_exec_end', tool: 'read', is_error: false },
			{ type: 'text_delta', text: 'Read 9 lines.' },
			{ type: 'agent_end' },
		]);
		return;
	}

	if (text === 'auth') {
		await emitBetween([
			{ type: 'agent_start' },
			{ type: 'system', text: '' },
			{ type: 'system', text: '' },
			{ type: 'system', text: '' },
			{ type: 'system', text: '' },
			{ type: 'assistant_error', message: 'Authentication failed: fake API key is missing' },
			{ type: 'system', text: '' },
			{ type: 'agent_end' },
		]);
		return;
	}

	if (text === 'hold') {
		holding = true;
		const aborted = new Promise((resolve) => {
			abortResolve = resolve;
		});
		emit({ type: 'agent_start' });
		await delay(15);
		if (holding) emit({ type: 'text_delta', text: 'partial response' });
		await aborted;
		return;
	}

	if (text === 'retry') {
		await emitBetween([
			{ type: 'agent_start' },
			{ type: 'assistant_error', message: 'Transport error: dns' },
			{ type: 'agent_end' },
		]);
		emit({
			type: 'retry_scheduled',
			attempt: 1,
			max_retries: 3,
			delay_ms: 200,
			error: 'Transport error: dns',
		});
		emit({ type: 'transcript_replace', messages: [] });
		emit(statusLine());
		await delay(200);
		running = true;
		await emitBetween([
			{ type: 'agent_start' },
			{ type: 'text_delta', text: 'recovered' },
			{ type: 'agent_end' },
		]);
		return;
	}

	await emitBetween([
		{ type: 'agent_start' },
		{ type: 'text_delta', text: `echo: ${text}` },
		{ type: 'agent_end' },
	]);
}

function handleCommand(command, rawLine) {
	if (typeof command !== 'object' || command === null || Array.isArray(command) || typeof command.type !== 'string') {
		emit({ type: 'error', message: 'invalid command: expected an object with a type' });
		return;
	}

	switch (command.type) {
		case 'get_state':
			emit(statusLine());
			emit({
				type: 'system',
				text: `Session: fake0001\nProvider: openai\nModel: ${model}\nThinking: ${thinking}`,
			});
			return;
		case 'set_thinking': {
			const requested = String(command.level ?? '');
			const level = THINKING_LEVELS.find((candidate) => candidate === requested.toLowerCase());
			if (level === undefined) {
				emit({
					type: 'system',
					text: `invalid thinking level '${requested}' (expected: off, low, medium, high)`,
				});
				return;
			}
			thinking = level;
			emit(statusLine());
			emit({ type: 'system', text: `Thinking level set to ${level}` });
			return;
		}
		case 'set_model': {
			const requested = String(command.model ?? '');
			if (!MODELS.includes(requested)) {
				emit({ type: 'system', text: `unknown model '${requested}'` });
				return;
			}
			model = requested;
			emit(statusLine());
			emit({ type: 'system', text: `Model set to openai:${requested}` });
			return;
		}
		case 'prompt': {
			if (process.env.FAKE_ANIE_PROMPT_LOG) {
				appendFileSync(process.env.FAKE_ANIE_PROMPT_LOG, rawLine + '\n');
			}
			track(handlePrompt(typeof command.text === 'string' ? command.text : ''));
			return;
		}
		case 'abort':
			handleAbort();
			return;
		default:
			emit({ type: 'error', message: `invalid command: unknown type ${command.type}` });
	}
}

const rl = readline.createInterface({ input: process.stdin });

rl.on('line', (line) => {
	if (line.trim() === '') return;
	try {
		handleCommand(JSON.parse(line), line);
	} catch (error) {
		const message = error instanceof Error ? error.message : String(error);
		emit({ type: 'error', message: `invalid command: ${message}` });
	}
});

rl.on('close', () => {
	if (holding) {
		finishExit();
		return;
	}
	void inFlight.then(finishExit, finishExit);
});
