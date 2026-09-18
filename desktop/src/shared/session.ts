import { assertNever, type Command, type CompactionPhase, type RunEvent, type SessionStatus } from './domain.js';

export type RunPhase =
	| { kind: 'idle' }
	| { kind: 'running'; abortRequested: boolean; sawOutput: boolean }
	| { kind: 'retry_pending'; attempt: number; maxRetries: number; delayMs: number; error: string };

export type AssistantState =
	| { kind: 'streaming' }
	| { kind: 'done' }
	| { kind: 'error'; message: string }
	| { kind: 'aborted' };

export type ToolState = { kind: 'running' } | { kind: 'done'; isError: boolean };

export type CompactionResult = { summary: string; tokensBefore: number; tokensAfter: number };

export type TranscriptItem =
	| { id: number; kind: 'user'; text: string }
	| { id: number; kind: 'assistant'; text: string; state: AssistantState }
	| { id: number; kind: 'tool'; name: string; args: unknown; state: ToolState }
	| { id: number; kind: 'notice'; text: string; tone: 'info' | 'warn' }
	| { id: number; kind: 'compaction'; phase: CompactionPhase; result: CompactionResult | null }
	| { id: number; kind: 'retry'; attempt: number; maxRetries: number; delayMs: number; error: string };

export type Session = {
	status: SessionStatus | null;
	phase: RunPhase;
	items: TranscriptItem[];
	nextId: number;
	busyHint: boolean;
};

export type SessionInput =
	| { kind: 'run_event'; event: RunEvent }
	| { kind: 'process_started' }
	| { kind: 'user_submitted'; text: string }
	| { kind: 'user_aborted' };

export type Effect = { kind: 'send'; command: Command };

export const initialSession: Session = {
	status: null,
	phase: { kind: 'idle' },
	items: [],
	nextId: 1,
	busyHint: false,
};

/** The sentence shown when a run ends with no text, no tool call, and no error event. */
export const NO_RESPONSE_MESSAGE = 'anie finished without a response';

const none: Effect[] = [];

export function reduce(session: Session, input: SessionInput): [Session, Effect[]] {
	switch (input.kind) {
		case 'run_event':
			return [reduceRunEvent(session, input.event), none];
		case 'process_started':
			return [initialSession, none];
		case 'user_submitted':
			return submitPrompt(session, input.text);
		case 'user_aborted':
			return abort(session);
		default:
			return assertNever(input, 'reduce');
	}
}

// anie-specific (differs from anie's TUI, which queues follow-ups): the
// RPC surface rejects a mid-run prompt and has no queue, and a host-side
// queue would race the controller's retry decision after `agent_end`.
// The draft stays in the composer and a hint says why.
function submitPrompt(session: Session, text: string): [Session, Effect[]] {
	if (session.phase.kind !== 'idle') return [{ ...session, busyHint: true }, none];
	const next = appendItem({ ...session, busyHint: false }, { kind: 'user', text });
	return [next, [{ kind: 'send', command: { kind: 'prompt', text } }]];
}

function abort(session: Session): [Session, Effect[]] {
	const sendAbort: Effect[] = [{ kind: 'send', command: { kind: 'abort' } }];
	switch (session.phase.kind) {
		case 'idle':
			return [session, none];
		case 'running':
			return [{ ...session, phase: { ...session.phase, abortRequested: true } }, sendAbort];
		case 'retry_pending':
			return [{ ...session, phase: { kind: 'idle' }, busyHint: false }, sendAbort];
		default:
			return assertNever(session.phase, 'abort');
	}
}

function reduceRunEvent(session: Session, event: RunEvent): Session {
	switch (event.kind) {
		case 'status':
			return { ...session, status: event.status };
		case 'notice':
			return appendItem(session, { kind: 'notice', text: event.text, tone: 'info' });
		case 'protocol_error':
			return appendItem(session, { kind: 'notice', text: `anie rejected a command: ${event.message}`, tone: 'warn' });
		case 'run_started':
			return { ...session, phase: { kind: 'running', abortRequested: false, sawOutput: false } };
		case 'text':
			return markOutput(appendText(session, event.text));
		case 'assistant_error':
			return markOutput(closeStreaming(session, { kind: 'error', message: event.message }, true));
		case 'tool_started':
			return markOutput(
				appendItem(closeStreaming(session, { kind: 'done' }, false), {
					kind: 'tool',
					name: event.tool,
					args: event.args,
					state: { kind: 'running' },
				}),
			);
		case 'tool_finished':
			return finishTool(session, event.tool, event.isError);
		case 'compaction_started':
			return appendItem(session, { kind: 'compaction', phase: event.phase, result: null });
		case 'compaction_finished':
			return finishCompaction(session, event.phase, {
				summary: event.summary,
				tokensBefore: event.tokensBefore,
				tokensAfter: event.tokensAfter,
			});
		case 'retry_scheduled': {
			const { attempt, maxRetries, delayMs, error } = event;
			return appendItem(
				{ ...session, phase: { kind: 'retry_pending', attempt, maxRetries, delayMs, error } },
				{ kind: 'retry', attempt, maxRetries, delayMs, error },
			);
		}
		case 'run_ended':
			return endRun(session);
		default:
			return assertNever(event, 'reduceRunEvent');
	}
}

function appendItem(session: Session, item: DistributiveOmit<TranscriptItem, 'id'>): Session {
	return {
		...session,
		items: [...session.items, { ...item, id: session.nextId }],
		nextId: session.nextId + 1,
	};
}

type DistributiveOmit<T, K extends keyof T> = T extends unknown ? Omit<T, K> : never;

function streamingIndex(items: TranscriptItem[]): number {
	for (let i = items.length - 1; i >= 0; i--) {
		const item = items[i];
		if (item?.kind === 'assistant' && item.state.kind === 'streaming') return i;
	}
	return -1;
}

function replaceItem(session: Session, index: number, item: TranscriptItem): Session {
	return { ...session, items: session.items.map((existing, i) => (i === index ? item : existing)) };
}

function appendText(session: Session, text: string): Session {
	const index = streamingIndex(session.items);
	if (index === -1) return appendItem(session, { kind: 'assistant', text, state: { kind: 'streaming' } });
	const item = session.items[index];
	if (item?.kind !== 'assistant') return session;
	return replaceItem(session, index, { ...item, text: item.text + text });
}

function closeStreaming(session: Session, state: Exclude<AssistantState, { kind: 'streaming' }>, openIfAbsent: boolean): Session {
	const index = streamingIndex(session.items);
	if (index === -1) return openIfAbsent ? appendItem(session, { kind: 'assistant', text: '', state }) : session;
	const item = session.items[index];
	if (item?.kind !== 'assistant') return session;
	return replaceItem(session, index, { ...item, state });
}

function markOutput(session: Session): Session {
	if (session.phase.kind !== 'running' || session.phase.sawOutput) return session;
	return { ...session, phase: { ...session.phase, sawOutput: true } };
}

function finishTool(session: Session, tool: string | null, isError: boolean): Session {
	const index = session.items.findIndex(
		(item) => item.kind === 'tool' && item.state.kind === 'running' && (tool === null || item.name === tool),
	);
	const item = session.items[index];
	if (item?.kind !== 'tool') return session;
	return replaceItem(session, index, { ...item, state: { kind: 'done', isError } });
}

function finishCompaction(session: Session, phase: CompactionPhase, result: CompactionResult): Session {
	for (let i = session.items.length - 1; i >= 0; i--) {
		const item = session.items[i];
		if (item?.kind === 'compaction' && item.phase === phase && item.result === null) {
			return replaceItem(session, i, { ...item, result });
		}
	}
	return appendItem(session, { kind: 'compaction', phase, result });
}

function endRun(session: Session): Session {
	const idle: Session = { ...session, phase: { kind: 'idle' }, busyHint: false };
	if (session.phase.kind !== 'running') return idle;
	const { abortRequested, sawOutput } = session.phase;
	const closed = closeStreaming(idle, abortRequested ? { kind: 'aborted' } : { kind: 'done' }, false);
	// anie-specific (only against a binary older than `assistant_error`):
	// a give-up used to reach the wire as `agent_end` with nothing before it.
	if (!sawOutput && !abortRequested) {
		return appendItem(closed, { kind: 'assistant', text: '', state: { kind: 'error', message: NO_RESPONSE_MESSAGE } });
	}
	return closed;
}
