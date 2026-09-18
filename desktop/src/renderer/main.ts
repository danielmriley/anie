import { THINKING_LEVELS, assertNever, type AnieBridge, type ProcessState, type Push } from '../shared/domain.js';
import { initialSession, reduce, type Session, type SessionInput, type TranscriptItem } from '../shared/session.js';

declare global {
	interface Window {
		anie: AnieBridge;
	}
}

let session: Session = initialSession;
let processState: ProcessState = { phase: 'starting', cwd: '', binary: '' };
let prevProcessPhase = '';
let clearItemMap = false;

const itemElements = new Map<number, HTMLElement>();
let paintScheduled = false;
let userPinnedScroll = false;

const transcriptEl = document.createElement('div');
transcriptEl.dataset.testid = 'transcript';
transcriptEl.className = 'transcript';

const startFailureEl = document.createElement('div');
startFailureEl.dataset.testid = 'start-failure';
startFailureEl.className = 'start-failure';
startFailureEl.hidden = true;

const busyHintEl = document.createElement('div');
busyHintEl.dataset.testid = 'busy-hint';
busyHintEl.className = 'busy-hint';
busyHintEl.textContent = 'Stop the current run before sending another prompt.';
busyHintEl.hidden = true;

const promptInput = document.createElement('textarea');
promptInput.dataset.testid = 'prompt-input';
promptInput.rows = 3;
promptInput.placeholder = 'Message anie…';

const sendBtn = document.createElement('button');
sendBtn.type = 'submit';
sendBtn.dataset.testid = 'send';
sendBtn.textContent = 'Send';

const abortBtn = document.createElement('button');
abortBtn.type = 'button';
abortBtn.dataset.testid = 'abort';
abortBtn.textContent = 'Stop';

const composerForm = document.createElement('form');
composerForm.dataset.testid = 'composer';
composerForm.className = 'composer';
composerForm.append(promptInput, sendBtn, abortBtn);

const cwdBtn = document.createElement('button');
cwdBtn.type = 'button';
cwdBtn.dataset.testid = 'cwd';
cwdBtn.className = 'cwd-btn';

const modelInput = document.createElement('input');
modelInput.type = 'text';
modelInput.dataset.testid = 'model-input';
modelInput.className = 'model-input';

const thinkingSelect = document.createElement('select');
thinkingSelect.dataset.testid = 'thinking-select';
for (const level of THINKING_LEVELS) {
	const opt = document.createElement('option');
	opt.value = level;
	opt.textContent = level;
	thinkingSelect.append(opt);
}

const contextEl = document.createElement('span');
contextEl.dataset.testid = 'context';
contextEl.className = 'context';

const sessionIdEl = document.createElement('span');
sessionIdEl.dataset.testid = 'session-id';
sessionIdEl.className = 'session-id';

const processStateEl = document.createElement('span');
processStateEl.dataset.testid = 'process-state';
processStateEl.className = 'process-pill';

const newSessionBtn = document.createElement('button');
newSessionBtn.type = 'button';
newSessionBtn.dataset.testid = 'new-session';
newSessionBtn.textContent = 'New session';

const headerEl = document.createElement('header');
headerEl.className = 'header';
headerEl.append(cwdBtn, modelInput, thinkingSelect, contextEl, sessionIdEl, processStateEl, newSessionBtn);

const mainEl = document.createElement('main');
mainEl.className = 'main';
mainEl.append(startFailureEl, transcriptEl);

document.body.append(headerEl, mainEl, busyHintEl, composerForm);

transcriptEl.addEventListener('scroll', () => {
	const distance = transcriptEl.scrollHeight - transcriptEl.scrollTop - transcriptEl.clientHeight;
	userPinnedScroll = distance > 40;
});

function dispatch(input: SessionInput): void {
	const [next, effects] = reduce(session, input);
	session = next;
	for (const effect of effects) {
		void window.anie.send(effect.command);
	}
	schedulePaint();
}

function schedulePaint(): void {
	if (paintScheduled) return;
	paintScheduled = true;
	requestAnimationFrame(() => {
		paintScheduled = false;
		paint();
	});
}

function assistantStateAttr(state: TranscriptItem & { kind: 'assistant' }): string {
	switch (state.state.kind) {
		case 'streaming':
			return 'streaming';
		case 'done':
			return 'done';
		case 'error':
			return 'error';
		case 'aborted':
			return 'aborted';
		default:
			return assertNever(state.state, 'assistantStateAttr');
	}
}

function toolStateAttr(item: TranscriptItem & { kind: 'tool' }): string {
	if (item.state.kind === 'running') return 'running';
	return item.state.isError ? 'error' : 'done';
}

function createItemElement(item: TranscriptItem): HTMLElement {
	const el = document.createElement('article');
	el.className = `item item-${item.kind}`;
	el.dataset.itemKind = item.kind;

	switch (item.kind) {
		case 'user': {
			const pre = document.createElement('pre');
			pre.className = 'bubble user-bubble';
			pre.textContent = item.text;
			el.append(pre);
			break;
		}
		case 'assistant': {
			el.dataset.assistantState = assistantStateAttr(item);
			const textNode = document.createElement('span');
			textNode.className = 'assistant-text';
			textNode.textContent = item.text;
			el.append(textNode);
			if (item.state.kind === 'streaming') {
				const cursor = document.createElement('span');
				cursor.className = 'cursor';
				cursor.textContent = '▌';
				el.append(cursor);
			} else if (item.state.kind === 'error') {
				const err = document.createElement('div');
				err.className = 'assistant-error';
				err.textContent = item.state.message;
				el.append(err);
			} else if (item.state.kind === 'aborted') {
				const tag = document.createElement('span');
				tag.className = 'aborted-tag';
				tag.textContent = 'aborted';
				el.append(tag);
			}
			break;
		}
		case 'tool': {
			el.dataset.toolState = toolStateAttr(item);
			const title = document.createElement('div');
			title.className = 'tool-title';
			title.textContent = item.name;
			const args = document.createElement('pre');
			args.className = 'tool-args';
			args.textContent = JSON.stringify(item.args, null, 2);
			const tag = document.createElement('span');
			tag.className = 'tool-state-tag';
			tag.textContent = toolStateAttr(item);
			el.append(title, args, tag);
			break;
		}
		case 'notice': {
			el.classList.add(item.tone === 'warn' ? 'notice-warn' : 'notice-info');
			el.textContent = item.text;
			break;
		}
		case 'compaction': {
			const line = document.createElement('div');
			line.className = 'compaction-line';
			line.textContent = item.phase;
			if (item.result) {
				const tokens = document.createElement('span');
				tokens.textContent = ` ${item.result.tokensBefore} -> ${item.result.tokensAfter}`;
				line.append(tokens);
				const details = document.createElement('details');
				const summary = document.createElement('summary');
				summary.textContent = 'summary';
				const body = document.createElement('pre');
				body.textContent = item.result.summary;
				details.append(summary, body);
				el.append(line, details);
			} else {
				el.append(line);
			}
			break;
		}
		case 'retry': {
			el.textContent = retryText(item);
			break;
		}
		default:
			assertNever(item, 'createItemElement');
	}
	return el;
}

function retryText(item: TranscriptItem & { kind: 'retry' }): string {
	return `Retrying in ${item.delayMs} ms (attempt ${item.attempt} of ${item.maxRetries}): ${item.error}`;
}

function updateItemElement(el: HTMLElement, item: TranscriptItem): void {
	el.dataset.itemKind = item.kind;
	switch (item.kind) {
		case 'user': {
			const pre = el.querySelector('pre');
			if (pre) pre.textContent = item.text;
			break;
		}
		case 'assistant': {
			el.dataset.assistantState = assistantStateAttr(item);
			const textNode = el.querySelector('.assistant-text');
			if (textNode) textNode.textContent = item.text;
			let cursor = el.querySelector('.cursor');
			let err = el.querySelector('.assistant-error');
			let tag = el.querySelector('.aborted-tag');
			if (item.state.kind === 'streaming') {
				if (!cursor) {
					cursor = document.createElement('span');
					cursor.className = 'cursor';
					cursor.textContent = '▌';
					el.append(cursor);
				}
				err?.remove();
				tag?.remove();
			} else {
				cursor?.remove();
				if (item.state.kind === 'error') {
					if (!err) {
						err = document.createElement('div');
						err.className = 'assistant-error';
						el.append(err);
					}
					err.textContent = item.state.message;
					tag?.remove();
				} else if (item.state.kind === 'aborted') {
					err?.remove();
					if (!tag) {
						tag = document.createElement('span');
						tag.className = 'aborted-tag';
						el.append(tag);
					}
					tag.textContent = 'aborted';
				} else {
					err?.remove();
					tag?.remove();
				}
			}
			break;
		}
		case 'tool': {
			el.dataset.toolState = toolStateAttr(item);
			const title = el.querySelector('.tool-title');
			if (title) title.textContent = item.name;
			const args = el.querySelector('.tool-args');
			if (args) args.textContent = JSON.stringify(item.args, null, 2);
			const stateTag = el.querySelector('.tool-state-tag');
			if (stateTag) stateTag.textContent = toolStateAttr(item);
			break;
		}
		case 'notice':
			el.textContent = item.text;
			el.classList.toggle('notice-warn', item.tone === 'warn');
			el.classList.toggle('notice-info', item.tone === 'info');
			break;
		case 'compaction': {
			const fresh = createItemElement(item);
			el.replaceWith(fresh);
			itemElements.set(item.id, fresh);
			break;
		}
		case 'retry':
			el.textContent = retryText(item);
			break;
		default:
			assertNever(item, 'updateItemElement');
	}
}

function showStartFailure(): boolean {
	if (processState.phase === 'failed_to_start') return true;
	if (processState.phase === 'exited') {
		const code = processState.exit.code;
		return code !== null && code !== 0;
	}
	return false;
}

function renderStartFailure(): void {
	if (!showStartFailure()) {
		startFailureEl.hidden = true;
		return;
	}
	startFailureEl.hidden = false;
	startFailureEl.replaceChildren();
	const title = document.createElement('h2');
	title.textContent = 'Could not start anie';
	const kind = document.createElement('p');
	if (processState.phase === 'failed_to_start') {
		const failure = processState.failure;
		kind.textContent = failure.kind;
		const pre = document.createElement('pre');
		if (failure.kind === 'binary_not_found') {
			pre.textContent = failure.searched.join('\n');
		} else if (failure.kind === 'spawn_error') {
			pre.textContent = failure.message;
		} else if (failure.kind === 'exited_before_hello') {
			pre.textContent = failure.exit.stderrTail;
		} else if (failure.kind === 'hello_timeout') {
			pre.textContent = failure.stderrTail;
		} else if (failure.kind === 'unsupported_wire_version') {
			pre.textContent = `Wire version ${failure.version} is not supported.`;
		}
		const retry = document.createElement('button');
		retry.type = 'button';
		retry.textContent = 'Retry';
		retry.addEventListener('click', () => {
			void window.anie.send({ kind: 'new_session' });
		});
		startFailureEl.append(title, kind, pre, retry);
	} else if (processState.phase === 'exited') {
		kind.textContent = 'exited';
		const pre = document.createElement('pre');
		pre.textContent = processState.exit.stderrTail;
		const retry = document.createElement('button');
		retry.type = 'button';
		retry.textContent = 'Retry';
		retry.addEventListener('click', () => {
			void window.anie.send({ kind: 'new_session' });
		});
		startFailureEl.append(title, kind, pre, retry);
	}
}

function paint(): void {
	document.body.dataset.processPhase = processState.phase;
	document.body.dataset.runPhase = session.phase.kind;

	const status = session.status;
	cwdBtn.textContent = status?.cwd ?? processState.cwd ?? '';
	if (status && document.activeElement !== modelInput) {
		modelInput.value = status.model;
	}
	if (status) {
		const thinking = status.thinking;
		if ((THINKING_LEVELS as readonly string[]).includes(thinking)) {
			thinkingSelect.value = thinking;
		}
		sessionIdEl.textContent = status.sessionId;
		contextEl.textContent =
			status.contextWindow > 0 ? `${status.estimatedContextTokens} / ${status.contextWindow}` : '';
	} else {
		sessionIdEl.textContent = '';
		contextEl.textContent = '';
	}

	processStateEl.textContent = processState.phase;

	sendBtn.disabled = processState.phase !== 'ready' || session.phase.kind !== 'idle';
	abortBtn.disabled = session.phase.kind !== 'running' && session.phase.kind !== 'retry_pending';

	busyHintEl.hidden = !session.busyHint;

	renderStartFailure();

	if (clearItemMap) {
		itemElements.clear();
		transcriptEl.replaceChildren();
		clearItemMap = false;
	}

	const liveIds = new Set(session.items.map((i) => i.id));
	for (const [id, el] of itemElements) {
		if (!liveIds.has(id)) {
			el.remove();
			itemElements.delete(id);
		}
	}

	for (const item of session.items) {
		const existing = itemElements.get(item.id);
		if (!existing) {
			const el = createItemElement(item);
			itemElements.set(item.id, el);
			transcriptEl.append(el);
		} else {
			updateItemElement(existing, item);
		}
	}

	const streaming = session.items.some(
		(i) => i.kind === 'assistant' && i.state.kind === 'streaming',
	);
	if (streaming && !userPinnedScroll) {
		transcriptEl.scrollTop = transcriptEl.scrollHeight;
	}
}

let sawProcessPush = false;

function onPush(push: Push): void {
	if (push.channel === 'run') {
		dispatch({ kind: 'run_event', event: push.event });
		return;
	}
	const next = push.event;
	if (next.phase === 'starting' && prevProcessPhase !== 'starting') {
		clearItemMap = true;
		dispatch({ kind: 'process_started' });
	}
	prevProcessPhase = next.phase;
	processState = next;
	sawProcessPush = true;
	schedulePaint();
}

modelInput.addEventListener('keydown', (event) => {
	if (event.key === 'Enter') {
		event.preventDefault();
		const model = modelInput.value.trim();
		if (model) void window.anie.send({ kind: 'set_model', model });
	}
});

thinkingSelect.addEventListener('change', () => {
	const level = thinkingSelect.value;
	if ((THINKING_LEVELS as readonly string[]).includes(level)) {
		void window.anie.send({ kind: 'set_thinking', level: level as (typeof THINKING_LEVELS)[number] });
	}
});

cwdBtn.addEventListener('click', () => {
	void window.anie.pickDirectory().then((picked) => {
		if (picked) void window.anie.send({ kind: 'set_cwd', path: picked });
	});
});

newSessionBtn.addEventListener('click', () => {
	void window.anie.send({ kind: 'new_session' });
});

abortBtn.addEventListener('click', () => {
	dispatch({ kind: 'user_aborted' });
});

composerForm.addEventListener('submit', (event) => {
	event.preventDefault();
	const text = promptInput.value.trim();
	if (!text) return;
	dispatch({ kind: 'user_submitted', text });
	if (!session.busyHint) promptInput.value = '';
});

promptInput.addEventListener('keydown', (event) => {
	if (event.key === 'Enter' && !event.shiftKey) {
		event.preventDefault();
		composerForm.requestSubmit();
	}
	if (event.key === 'Escape') {
		dispatch({ kind: 'user_aborted' });
	}
});

async function boot(): Promise<void> {
	window.anie.onPush(onPush);
	const bootData = await window.anie.bootstrap();
	if (!sawProcessPush) {
		processState = bootData.process;
		prevProcessPhase = bootData.process.phase;
	}
	if (bootData.status && session.status === null) {
		dispatch({ kind: 'run_event', event: { kind: 'status', status: bootData.status } });
	}
	schedulePaint();
}

void boot();
