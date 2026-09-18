export const THINKING_LEVELS = ['off', 'minimal', 'low', 'medium', 'high'] as const;
export type ThinkingLevel = (typeof THINKING_LEVELS)[number];

export type SessionStatus = {
	provider: string;
	model: string;
	thinking: string;
	estimatedContextTokens: number;
	contextWindow: number;
	cwd: string;
	sessionId: string;
};

export type CompactionPhase = 'pre_prompt' | 'mid_turn' | 'reactive_overflow';

export type RunEvent =
	| { kind: 'status'; status: SessionStatus }
	| { kind: 'notice'; text: string }
	| { kind: 'run_started' }
	| { kind: 'text'; text: string }
	| { kind: 'tool_started'; tool: string; args: unknown }
	| { kind: 'tool_finished'; tool: string | null; isError: boolean }
	| { kind: 'assistant_error'; message: string }
	| { kind: 'compaction_started'; phase: CompactionPhase }
	| {
			kind: 'compaction_finished';
			phase: CompactionPhase;
			summary: string;
			tokensBefore: number;
			tokensAfter: number;
	  }
	| { kind: 'retry_scheduled'; attempt: number; maxRetries: number; delayMs: number; error: string }
	| { kind: 'run_ended' }
	| { kind: 'protocol_error'; message: string };

export type ExitInfo = { code: number | null; signal: string | null; stderrTail: string };

export type StartFailure =
	| { kind: 'binary_not_found'; searched: string[] }
	| { kind: 'spawn_error'; message: string }
	| { kind: 'exited_before_hello'; exit: ExitInfo }
	| { kind: 'hello_timeout'; afterMs: number; stderrTail: string }
	| { kind: 'unsupported_wire_version'; version: number };

export type ProcessState =
	| { phase: 'starting'; cwd: string; binary: string }
	| { phase: 'ready'; cwd: string; binary: string }
	| { phase: 'stopping'; cwd: string }
	| { phase: 'exited'; cwd: string; exit: ExitInfo }
	| { phase: 'failed_to_start'; cwd: string; failure: StartFailure };

export type Command =
	| { kind: 'prompt'; text: string }
	| { kind: 'abort' }
	| { kind: 'set_thinking'; level: ThinkingLevel }
	| { kind: 'set_model'; model: string }
	| { kind: 'set_cwd'; path: string }
	| { kind: 'new_session' };

export type Push =
	| { channel: 'process'; event: ProcessState }
	| { channel: 'run'; event: RunEvent };

export type Bootstrap = { process: ProcessState; status: SessionStatus | null };

export type AnieBridge = {
	send(command: Command): Promise<void>;
	bootstrap(): Promise<Bootstrap>;
	pickDirectory(): Promise<string | null>;
	onPush(listener: (push: Push) => void): () => void;
};

export function assertNever(value: never, where: string): never {
	throw new Error(`unhandled variant in ${where}: ${JSON.stringify(value)}`);
}
