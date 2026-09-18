import { spawn, type ChildProcess } from 'node:child_process';
import { existsSync } from 'node:fs';
import path from 'node:path';

import {
	assertNever,
	type Command,
	type CompactionPhase,
	type ExitInfo,
	type ProcessState,
	type Push,
	type RunEvent,
	type SessionStatus,
} from '../shared/domain.js';

export const SUPPORTED_WIRE_VERSION = 1;

/** Commands that map onto an anie stdin line. `set_cwd` and `new_session` restart the child in main instead. */
export type WireCommand = Exclude<Command, { kind: 'set_cwd' } | { kind: 'new_session' }>;

export type StartOptions = { cwd: string };

export type AnieProcessOptions = {
	binary?: string | undefined;
	helloTimeoutMs?: number;
	stopTimeoutMs?: number;
	env?: NodeJS.ProcessEnv;
};

const STDERR_TAIL_BYTES = 4096;

const noExit: ExitInfo = { code: null, signal: null, stderrTail: '' };

export class AnieProcess {
	readonly #push: (push: Push) => void;
	readonly #opts: Required<AnieProcessOptions>;
	#state: ProcessState = { phase: 'exited', cwd: '', exit: noExit };
	#child: ChildProcess | null = null;
	#exited: Promise<ExitInfo> | null = null;
	#resolveExited: ((exit: ExitInfo) => void) | null = null;
	#lastExit: ExitInfo = noExit;
	#lastStatus: SessionStatus | null = null;
	#stderrTail = '';
	#partialLine = '';
	#helloTimer: NodeJS.Timeout | null = null;
	#restartChain: Promise<void> = Promise.resolve();

	constructor(push: (push: Push) => void, opts: AnieProcessOptions = {}) {
		this.#push = push;
		this.#opts = {
			binary: opts.binary,
			helloTimeoutMs: opts.helloTimeoutMs ?? 20_000,
			stopTimeoutMs: opts.stopTimeoutMs ?? 3_000,
			env: opts.env ?? process.env,
		};
	}

	get state(): ProcessState {
		return this.#state;
	}

	get lastStatus(): SessionStatus | null {
		return this.#lastStatus;
	}

	start({ cwd }: StartOptions): void {
		if (this.#child) throw new Error('AnieProcess.start: a child is still running; use restart()');
		this.#lastStatus = null;
		this.#stderrTail = '';
		this.#partialLine = '';

		const resolved =
			this.#opts.binary === undefined ? resolveBinary(this.#opts.env) : { binary: this.#opts.binary };
		if (!('binary' in resolved)) {
			this.#setState({ phase: 'failed_to_start', cwd, failure: { kind: 'binary_not_found', searched: resolved.searched } });
			return;
		}
		const binary = resolved.binary;

		const child = spawn(binary, ['--rpc', '-C', cwd], { env: this.#opts.env, stdio: ['pipe', 'pipe', 'pipe'] });
		this.#child = child;
		this.#exited = new Promise((resolve) => {
			this.#resolveExited = resolve;
		});

		child.stdout?.setEncoding('utf8');
		child.stdout?.on('data', (chunk: string) => this.#onStdout(chunk));
		child.stderr?.setEncoding('utf8');
		child.stderr?.on('data', (chunk: string) => {
			this.#stderrTail = (this.#stderrTail + chunk).slice(-STDERR_TAIL_BYTES);
		});
		// A write racing the child's death surfaces as EPIPE on stdin. The
		// exit handler already reports the death, so the stream error is
		// only noise that would otherwise crash main.
		child.stdin?.on('error', () => {});
		child.on('error', (error) => this.#onSpawnError(error));
		child.on('exit', (code, signal) => this.#onExit(code, signal));

		this.#helloTimer = setTimeout(() => {
			this.#helloTimer = null;
			if (this.#state.phase !== 'starting') return;
			this.#setState({
				phase: 'failed_to_start',
				cwd,
				failure: { kind: 'hello_timeout', afterMs: this.#opts.helloTimeoutMs, stderrTail: this.#stderrTail },
			});
			child.kill('SIGKILL');
		}, this.#opts.helloTimeoutMs);

		this.#setState({ phase: 'starting', cwd, binary });
	}

	restart(options: StartOptions): Promise<void> {
		const next = async (): Promise<void> => {
			await this.stop();
			this.start(options);
		};
		this.#restartChain = this.#restartChain.then(next, next);
		return this.#restartChain;
	}

	send(command: WireCommand): void {
		const child = this.#child;
		if (this.#state.phase !== 'ready' || !child?.stdin) {
			this.#push({
				channel: 'run',
				event: { kind: 'notice', text: `Dropped "${command.kind}": anie is ${this.#state.phase.replace(/_/g, ' ')}.` },
			});
			return;
		}
		child.stdin.write(encodeCommand(command));
	}

	stop(): Promise<ExitInfo> {
		const child = this.#child;
		const exited = this.#exited;
		if (!child || !exited) return Promise.resolve(this.#lastExit);
		if (this.#state.phase === 'starting' || this.#state.phase === 'ready') {
			this.#clearHelloTimer();
			this.#setState({ phase: 'stopping', cwd: this.#state.cwd });
			this.#endStdinThenKill(child, exited);
		}
		return exited;
	}

	// EOF first, then SIGKILL. anie maps no signals in RPC mode, so a signal
	// would skip its session flush exactly like SIGKILL does; only EOF
	// lets the controller cancel the run and flush before exiting.
	#endStdinThenKill(child: ChildProcess, exited: Promise<ExitInfo>): void {
		child.stdin?.end();
		const killer = setTimeout(() => child.kill('SIGKILL'), this.#opts.stopTimeoutMs);
		void exited.then(() => clearTimeout(killer));
	}

	#onStdout(chunk: string): void {
		const lines = (this.#partialLine + chunk).split('\n');
		this.#partialLine = lines.pop() ?? '';
		for (const line of lines) {
			if (line.trim() === '') continue;
			this.#onLine(line);
		}
	}

	#onLine(line: string): void {
		const decoded = decodeLine(line);
		switch (decoded.kind) {
			case 'hello':
				this.#onHello(decoded.version);
				return;
			case 'event':
				if (decoded.event.kind === 'status') this.#lastStatus = decoded.event.status;
				this.#push({ channel: 'run', event: decoded.event });
				return;
			case 'noise':
				return;
			case 'malformed':
				process.stderr.write(`anie-desktop: unrecognized anie stdout line: ${decoded.line}\n`);
				return;
			default:
				return assertNever(decoded, 'AnieProcess.#onLine');
		}
	}

	#onHello(version: number): void {
		if (this.#state.phase !== 'starting') return;
		this.#clearHelloTimer();
		const { cwd, binary } = this.#state;
		if (version !== SUPPORTED_WIRE_VERSION) {
			this.#setState({ phase: 'failed_to_start', cwd, failure: { kind: 'unsupported_wire_version', version } });
			if (this.#child && this.#exited) this.#endStdinThenKill(this.#child, this.#exited);
			return;
		}
		this.#setState({ phase: 'ready', cwd, binary });
	}

	#onSpawnError(error: Error): void {
		if (this.#state.phase === 'starting') {
			this.#clearHelloTimer();
			this.#setState({ phase: 'failed_to_start', cwd: this.#state.cwd, failure: { kind: 'spawn_error', message: error.message } });
		}
		this.#finishExit({ code: null, signal: null, stderrTail: this.#stderrTail });
	}

	#onExit(code: number | null, signal: NodeJS.Signals | null): void {
		if (this.#partialLine.trim() !== '') this.#onLine(this.#partialLine);
		this.#partialLine = '';
		const exit: ExitInfo = { code, signal, stderrTail: this.#stderrTail };
		this.#clearHelloTimer();
		switch (this.#state.phase) {
			case 'starting':
				this.#setState({ phase: 'failed_to_start', cwd: this.#state.cwd, failure: { kind: 'exited_before_hello', exit } });
				break;
			case 'ready':
			case 'stopping':
				this.#setState({ phase: 'exited', cwd: this.#state.cwd, exit });
				break;
			case 'exited':
			case 'failed_to_start':
				break;
			default:
				assertNever(this.#state, 'AnieProcess.#onExit');
		}
		this.#finishExit(exit);
	}

	#finishExit(exit: ExitInfo): void {
		if (!this.#child) return;
		this.#child = null;
		this.#lastExit = exit;
		this.#resolveExited?.(exit);
		this.#resolveExited = null;
	}

	#clearHelloTimer(): void {
		if (this.#helloTimer) clearTimeout(this.#helloTimer);
		this.#helloTimer = null;
	}

	#setState(next: ProcessState): void {
		this.#state = next;
		this.#push({ channel: 'process', event: next });
	}
}

export function resolveBinary(env: NodeJS.ProcessEnv = process.env): { binary: string } | { searched: string[] } {
	const explicit = env.ANIE_DESKTOP_BIN;
	if (explicit) return existsSync(explicit) ? { binary: explicit } : { searched: [explicit] };

	const repoRoot = path.resolve(import.meta.dirname, '..', '..', '..', '..');
	const candidates = [path.join(repoRoot, 'target', 'release', 'anie'), path.join(repoRoot, 'target', 'debug', 'anie')];
	for (const candidate of candidates) {
		if (existsSync(candidate)) return { binary: candidate };
	}
	for (const dir of (env.PATH ?? '').split(path.delimiter).filter(Boolean)) {
		const candidate = path.join(dir, 'anie');
		if (existsSync(candidate)) return { binary: candidate };
	}
	return { searched: [...candidates, 'anie on PATH'] };
}

export type Decoded =
	| { kind: 'hello'; version: number }
	| { kind: 'event'; event: RunEvent }
	| { kind: 'noise' }
	| { kind: 'malformed'; line: string };

type Record_ = Record<string, unknown>;

function isRecord(value: unknown): value is Record_ {
	return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function str(raw: Record_, key: string): string | null {
	const value = raw[key];
	return typeof value === 'string' ? value : null;
}

function num(raw: Record_, key: string): number | null {
	const value = raw[key];
	return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function bool(raw: Record_, key: string): boolean | null {
	const value = raw[key];
	return typeof value === 'boolean' ? value : null;
}

const COMPACTION_PHASES: readonly CompactionPhase[] = ['pre_prompt', 'mid_turn', 'reactive_overflow'];

function compactionPhase(raw: Record_): CompactionPhase | null {
	const value = raw.phase;
	return COMPACTION_PHASES.find((phase) => phase === value) ?? null;
}

const event = (event: RunEvent): Decoded => ({ kind: 'event', event });

/** Total over strings. Maps anie's `RpcEvent` tags onto `RunEvent`; anything else is `malformed`. */
export function decodeLine(line: string): Decoded {
	const malformed: Decoded = { kind: 'malformed', line };
	let raw: unknown;
	try {
		raw = JSON.parse(line);
	} catch {
		return malformed;
	}
	if (!isRecord(raw)) return malformed;

	switch (raw.type) {
		case 'hello': {
			const version = num(raw, 'version');
			return version === null ? malformed : { kind: 'hello', version };
		}
		case 'status': {
			const provider = str(raw, 'provider');
			const model = str(raw, 'model');
			const thinking = str(raw, 'thinking');
			const estimatedContextTokens = num(raw, 'estimated_context_tokens');
			const contextWindow = num(raw, 'context_window');
			const cwd = str(raw, 'cwd');
			const sessionId = str(raw, 'session_id');
			if (
				provider === null ||
				model === null ||
				thinking === null ||
				estimatedContextTokens === null ||
				contextWindow === null ||
				cwd === null ||
				sessionId === null
			) {
				return malformed;
			}
			return event({
				kind: 'status',
				status: { provider, model, thinking, estimatedContextTokens, contextWindow, cwd, sessionId },
			});
		}
		case 'system': {
			const text = str(raw, 'text');
			if (text === null) return malformed;
			return text === '' ? { kind: 'noise' } : event({ kind: 'notice', text });
		}
		case 'agent_start':
			return event({ kind: 'run_started' });
		case 'agent_end':
			return event({ kind: 'run_ended' });
		case 'text_delta': {
			const text = str(raw, 'text');
			return text === null ? malformed : event({ kind: 'text', text });
		}
		case 'tool_exec_start': {
			const tool = str(raw, 'tool');
			return tool === null ? malformed : event({ kind: 'tool_started', tool, args: raw.args ?? null });
		}
		case 'tool_exec_end': {
			const tool = str(raw, 'tool');
			const isError = bool(raw, 'is_error');
			if (tool === null || isError === null) return malformed;
			return event({ kind: 'tool_finished', tool: tool === '' ? null : tool, isError });
		}
		case 'assistant_error': {
			const message = str(raw, 'message');
			return message === null ? malformed : event({ kind: 'assistant_error', message });
		}
		case 'transcript_replace':
			// anie-specific (differs from anie's TUI, which repaints from this
			// event): it fires only after a retry or a compaction, so a
			// transcript rebuilt from it would show tool bodies inconsistently.
			return { kind: 'noise' };
		case 'compaction_start': {
			const phase = compactionPhase(raw);
			return phase === null ? malformed : event({ kind: 'compaction_started', phase });
		}
		case 'compaction_end': {
			const phase = compactionPhase(raw);
			const summary = str(raw, 'summary');
			const tokensBefore = num(raw, 'tokens_before');
			const tokensAfter = num(raw, 'tokens_after');
			if (phase === null || summary === null || tokensBefore === null || tokensAfter === null) return malformed;
			return event({ kind: 'compaction_finished', phase, summary, tokensBefore, tokensAfter });
		}
		case 'retry_scheduled': {
			const attempt = num(raw, 'attempt');
			const maxRetries = num(raw, 'max_retries');
			const delayMs = num(raw, 'delay_ms');
			const error = str(raw, 'error');
			if (attempt === null || maxRetries === null || delayMs === null || error === null) return malformed;
			return event({ kind: 'retry_scheduled', attempt, maxRetries, delayMs, error });
		}
		case 'error': {
			const message = str(raw, 'message');
			return message === null ? malformed : event({ kind: 'protocol_error', message });
		}
		default:
			return malformed;
	}
}

/** One anie stdin line, newline included. */
export function encodeCommand(command: WireCommand): string {
	switch (command.kind) {
		case 'prompt':
			return JSON.stringify({ type: 'prompt', text: command.text }) + '\n';
		case 'abort':
			return JSON.stringify({ type: 'abort' }) + '\n';
		case 'set_thinking':
			return JSON.stringify({ type: 'set_thinking', level: command.level }) + '\n';
		case 'set_model':
			return JSON.stringify({ type: 'set_model', model: command.model }) + '\n';
		default:
			return assertNever(command, 'encodeCommand');
	}
}
