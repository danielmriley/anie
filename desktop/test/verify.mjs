import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdir, mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const desktopRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const electronPath = createRequire(import.meta.url)('electron');
const fake = path.join(desktopRoot, 'test', 'fake-anie.mjs');
const realFlag = process.argv.includes('--real');
const shotsRoot = process.env.ANIE_DESKTOP_SHOTS ?? '/tmp/anie-desktop-verify';
const shots = realFlag ? path.join(shotsRoot, 'real') : shotsRoot;

class CheckError extends Error {
	constructor(message, lastValue) {
		super(message);
		this.lastValue = lastValue;
	}
}

function delay(ms) {
	return new Promise((resolve) => setTimeout(resolve, ms));
}

function withTimeout(promise, ms, message) {
	return new Promise((resolve, reject) => {
		const timer = setTimeout(() => reject(new Error(message)), ms);
		promise.then(
			(value) => {
				clearTimeout(timer);
				resolve(value);
			},
			(error) => {
				clearTimeout(timer);
				reject(error);
			},
		);
	});
}

async function reservePort() {
	const server = net.createServer();
	await new Promise((resolve, reject) => {
		server.once('error', reject);
		server.listen(0, '127.0.0.1', resolve);
	});
	const address = server.address();
	if (address === null || typeof address !== 'object') {
		throw new Error('failed to reserve a TCP port');
	}
	const port = address.port;
	await new Promise((resolve, reject) => {
		server.close((error) => (error === undefined ? resolve() : reject(error)));
	});
	return port;
}

async function waitForPage(debugPort, timeoutMs = 20000) {
	const deadline = Date.now() + timeoutMs;
	let last = null;
	while (Date.now() < deadline) {
		try {
			const response = await fetch(`http://127.0.0.1:${debugPort}/json/list`);
			const pages = await response.json();
			last = pages;
			const page = pages.find((candidate) => candidate.type === 'page');
			if (page !== undefined) return page;
		} catch (error) {
			last = error instanceof Error ? error.message : error;
		}
		await delay(50);
	}
	throw new CheckError('CDP page target did not appear', last);
}

class Cdp {
	static async connect(url) {
		const socket = new WebSocket(url);
		await new Promise((resolve, reject) => {
			socket.addEventListener('open', resolve, { once: true });
			socket.addEventListener('error', reject, { once: true });
		});
		return new Cdp(socket);
	}

	constructor(socket) {
		this.socket = socket;
		this.nextId = 1;
		this.pending = new Map();
		socket.addEventListener('message', (event) => {
			const message = JSON.parse(String(event.data));
			if (message.id === undefined) return;
			const pending = this.pending.get(message.id);
			if (pending === undefined) return;
			this.pending.delete(message.id);
			if (message.error !== undefined) pending.reject(new Error(message.error.message));
			else pending.resolve(message.result);
		});
	}

	send(method, params = {}) {
		const id = this.nextId;
		this.nextId += 1;
		return new Promise((resolve, reject) => {
			this.pending.set(id, { resolve, reject });
			this.socket.send(JSON.stringify({ id, method, params }));
		});
	}

	async evaluate(expression) {
		const result = await this.send('Runtime.evaluate', {
			expression,
			awaitPromise: true,
			returnByValue: true,
		});
		if (result.exceptionDetails !== undefined) {
			const details = result.exceptionDetails;
			const text = details.exception?.description ?? details.text ?? 'Runtime.evaluate failed';
			throw new Error(text);
		}
		return result.result.value;
	}

	close() {
		if (this.socket.readyState === WebSocket.OPEN || this.socket.readyState === WebSocket.CONNECTING) {
			this.socket.close();
		}
	}
}

function resolveRealBinary() {
	const explicit = process.env.ANIE_DESKTOP_BIN;
	if (explicit) return explicit;
	const candidates = ['/workspace/target/release/anie', '/workspace/target/debug/anie'];
	for (const candidate of candidates) {
		if (existsSync(candidate)) return candidate;
	}
	throw new Error(`anie binary not found (searched ${candidates.join(', ')})`);
}

function killIfAlive(child) {
	if (child !== null && child.exitCode === null && child.signalCode === null) {
		child.kill('SIGKILL');
	}
}

function spawnElectron(port, env) {
	const args = [`--remote-debugging-port=${port}`, '--disable-gpu'];
	if (process.env.CI !== undefined || process.getuid?.() === 0) {
		args.push('--no-sandbox');
	}
	args.push(desktopRoot);
	let diagnostics = '';
	const electron = spawn(electronPath, args, {
		cwd: desktopRoot,
		env,
		stdio: ['ignore', 'pipe', 'pipe'],
	});
	for (const stream of [electron.stdout, electron.stderr]) {
		stream.setEncoding('utf8');
		stream.on('data', (chunk) => {
			diagnostics = `${diagnostics}${chunk}`.slice(-65536);
		});
	}
	const processClosed = new Promise((resolve, reject) => {
		electron.once('error', reject);
		electron.once('close', (code, signal) => resolve({ code, signal }));
	});
	return {
		electron,
		processClosed,
		diagnostics: () => diagnostics,
	};
}

async function main() {
	await mkdir(shots, { recursive: true });
	const tmp = await mkdtemp(path.join(os.tmpdir(), 'anie-desktop-verify-'));
	const cwd = path.join(tmp, 'cwd');
	await mkdir(cwd);
	const exitFile = path.join(tmp, 'exit.json');
	const promptLog = path.join(tmp, 'prompts.log');
	const tempHome = path.join(tmp, 'home');
	if (realFlag) await mkdir(tempHome);

	let checks = 0;
	let electron = null;
	let cdp = null;
	let lastDiagnostics = () => '';
	let shotIndex = 0;

	function check(condition, message) {
		if (!condition) throw new CheckError(message, condition);
		checks += 1;
	}

	async function evaluate(expr) {
		if (cdp === null) throw new Error('CDP is not connected');
		return cdp.evaluate(expr);
	}

	async function waitFor(expr, predicate, label, timeoutMs = 10000) {
		const deadline = Date.now() + timeoutMs;
		let last;
		while (Date.now() < deadline) {
			try {
				last = await evaluate(expr);
				if (predicate(last)) {
					checks += 1;
					return last;
				}
			} catch (error) {
				last = error instanceof Error ? error.message : error;
			}
			await delay(50);
		}
		throw new CheckError(label, last);
	}

	async function submit(text) {
		await evaluate(`(() => {
			const input = document.querySelector('[data-testid="prompt-input"]');
			input.value = ${JSON.stringify(text)};
			input.dispatchEvent(new Event("input", { bubbles: true }));
			document.querySelector('[data-testid="composer"]').requestSubmit();
		})()`);
	}

	async function shot(name) {
		if (cdp === null) throw new Error('CDP is not connected');
		shotIndex += 1;
		const result = await cdp.send('Page.captureScreenshot', { format: 'png' });
		const file = path.join(shots, `${String(shotIndex).padStart(2, '0')}-${name}.png`);
		await writeFile(file, Buffer.from(result.data, 'base64'));
	}

	async function connect(port) {
		const page = await waitForPage(port);
		const client = await Cdp.connect(page.webSocketDebuggerUrl);
		await client.send('Runtime.enable');
		await client.send('Page.enable');
		return client;
	}

	try {
		if (realFlag) {
			await runReal();
		} else {
			await runFake();
		}
		console.log(`PASS ${checks} checks, screenshots in ${shots}`);
	} catch (error) {
		const message = error instanceof Error ? error.message : String(error);
		console.error(`FAIL ${message}`);
		if (error instanceof CheckError) {
			console.error(`last value: ${JSON.stringify(error.lastValue)}`);
		}
		console.error(lastDiagnostics());
		process.exitCode = 1;
	} finally {
		cdp?.close();
		killIfAlive(electron);
	}

	async function runFake() {
		const port = await reservePort();
		const launched = spawnElectron(port, {
			...process.env,
			ANIE_DESKTOP_BIN: fake,
			ANIE_DESKTOP_CWD: cwd,
			FAKE_ANIE_EXIT_FILE: exitFile,
			FAKE_ANIE_PROMPT_LOG: promptLog,
			ELECTRON_DISABLE_SECURITY_WARNINGS: 'true',
		});
		electron = launched.electron;
		lastDiagnostics = launched.diagnostics;
		cdp = await connect(port);

		await waitFor(
			'document.body.dataset.processPhase',
			(value) => value === 'ready',
			'processPhase is ready',
		);
		const shownCwd = await evaluate('document.querySelector(\'[data-testid="cwd"]\')?.textContent');
		check(shownCwd === cwd, 'cwd header matches the temp cwd');
		await shot('ready');

		await submit('stream');
		await waitFor(
			`[...document.querySelectorAll('[data-item-kind="assistant"][data-assistant-state="done"]')].map((n) => n.textContent)`,
			(texts) => Array.isArray(texts) && texts.some((text) => typeof text === 'string' && text.includes('Hello from fake anie.')),
			'streamed assistant text includes Hello from fake anie.',
		);
		await shot('stream');

		await submit('tool');
		await waitFor(
			`(() => {
				const el = document.querySelector('[data-item-kind="tool"][data-tool-state="done"]');
				return el ? el.textContent : null;
			})()`,
			(text) => typeof text === 'string' && text.includes('read'),
			'tool item is done and names read',
		);
		await shot('tool');

		await submit('auth');
		await waitFor(
			`[...document.querySelectorAll('[data-assistant-state="error"]')].map((n) => n.textContent)`,
			(texts) => Array.isArray(texts) && texts.some((text) => typeof text === 'string' && text.includes('Authentication failed: fake API key is missing')),
			'auth error shows Authentication failed: fake API key is missing',
		);
		await shot('auth-error');

		await submit('hold');
		await waitFor(
			'document.body.dataset.runPhase',
			(value) => value === 'running',
			'hold runPhase is running',
		);
		await evaluate(`(() => {
			const input = document.querySelector('[data-testid="prompt-input"]');
			input.value = "second prompt";
			input.dispatchEvent(new Event("input", { bubbles: true }));
			document.querySelector('[data-testid="composer"]').requestSubmit();
		})()`);
		await waitFor(
			`(() => {
				const el = document.querySelector('[data-testid="busy-hint"]');
				if (!el) return { visible: false, draft: document.querySelector('[data-testid="prompt-input"]')?.value ?? null };
				const style = getComputedStyle(el);
				const hidden = el.hidden || style.display === "none" || style.visibility === "hidden";
				const visible = el.offsetParent !== null || (Boolean(el.textContent) && !hidden);
				return { visible, draft: document.querySelector('[data-testid="prompt-input"]')?.value ?? null, text: el.textContent };
			})()`,
			(value) => value !== null && typeof value === 'object' && value.visible === true,
			'busy-hint is visible after a mid-run submit',
		);
		const draft = await evaluate('document.querySelector(\'[data-testid="prompt-input"]\')?.value');
		check(draft === 'second prompt', 'mid-run draft stays in the textarea');
		const logRaw = await readFile(promptLog, 'utf8');
		const promptTexts = logRaw
			.split('\n')
			.filter((line) => line.length > 0)
			.map((line) => {
				try {
					return JSON.parse(line).text;
				} catch {
					return line;
				}
			});
		check(promptTexts.filter((text) => text === 'hold').length === 1, 'fake received exactly one hold prompt');
		check(!promptTexts.includes('second prompt'), 'fake did not receive the second prompt');
		await shot('busy-hint');

		await evaluate('document.querySelector(\'[data-testid="abort"]\').click()');
		await waitFor(
			'document.body.dataset.runPhase',
			(value) => value === 'idle',
			'abort returns runPhase to idle',
		);
		await waitFor(
			`[...document.querySelectorAll('[data-assistant-state="aborted"]')].map((n) => n.textContent)`,
			(texts) => Array.isArray(texts) && texts.some((text) => typeof text === 'string' && text.includes('partial response')),
			'aborted item keeps partial response',
		);
		await shot('aborted');

		await submit('retry');
		await waitFor(
			'Boolean(document.querySelector(\'[data-item-kind="retry"]\'))',
			(value) => value === true,
			'retry item appears',
		);
		await waitFor(
			`[...document.querySelectorAll('[data-assistant-state="done"]')].map((n) => n.textContent)`,
			(texts) => Array.isArray(texts) && texts.some((text) => typeof text === 'string' && text.includes('recovered')),
			'retry recovered assistant text',
		);
		const retryError = await evaluate(
			`[...document.querySelectorAll('[data-assistant-state="error"]')].map((n) => n.textContent)`,
		);
		check(
			Array.isArray(retryError) && retryError.some((text) => typeof text === 'string' && text.includes('Transport error: dns')),
			'retry keeps the Transport error: dns item',
		);
		await shot('retry');

		await evaluate(`(() => {
			const el = document.querySelector('[data-testid="thinking-select"]');
			el.value = "low";
			el.dispatchEvent(new Event("change", { bubbles: true }));
		})()`);
		await waitFor(
			'document.querySelector(\'[data-testid="thinking-select"]\')?.value',
			(value) => value === 'low',
			'thinking-select value is low',
		);
		await waitFor(
			`[...document.querySelectorAll('[data-item-kind="notice"]')].map((n) => n.textContent)`,
			(texts) => Array.isArray(texts) && texts.some((text) => typeof text === 'string' && text.includes('Thinking level set to low')),
			'notice says Thinking level set to low',
		);
		await shot('thinking');

		await evaluate('window.close()');
		cdp.close();
		cdp = null;
		const closed = await withTimeout(launched.processClosed, 10000, 'Electron did not exit after window.close()');
		check(closed.code === 0, `Electron exited with code ${closed.code}`);
		const exitRecord = JSON.parse(await readFile(exitFile, 'utf8'));
		const expectedExit = { reason: 'stdin-eof', cwd };
		if (JSON.stringify(exitRecord) !== JSON.stringify(expectedExit)) {
			throw new CheckError('exit file deep-equals { reason: stdin-eof, cwd }', exitRecord);
		}
		checks += 1;
		electron = null;

		const failPort = await reservePort();
		const failLaunch = spawnElectron(failPort, {
			...process.env,
			ANIE_DESKTOP_BIN: fake,
			ANIE_DESKTOP_CWD: cwd,
			FAKE_ANIE_EXIT_FILE: path.join(tmp, 'exit-fail.json'),
			FAKE_ANIE_PROMPT_LOG: path.join(tmp, 'prompts-fail.log'),
			FAKE_ANIE_MODE: 'no-hello',
			ELECTRON_DISABLE_SECURITY_WARNINGS: 'true',
		});
		electron = failLaunch.electron;
		lastDiagnostics = failLaunch.diagnostics;
		cdp = await connect(failPort);
		await waitFor(
			'document.querySelector(\'[data-testid="start-failure"]\')?.textContent',
			(text) => typeof text === 'string' && text.includes('home directory is not available'),
			'start-failure shows home directory is not available',
		);
		await shot('start-failure');
		await evaluate('window.close()');
		cdp.close();
		cdp = null;
		const failClosed = await withTimeout(failLaunch.processClosed, 10000, 'Electron did not exit after start-failure window.close()');
		check(failClosed.code === 0, `no-hello Electron exited with code ${failClosed.code}`);
		electron = null;
	}

	async function runReal() {
		const binary = resolveRealBinary();
		const port = await reservePort();
		// X11 looks up .Xauthority under HOME, so the override that isolates
		// anie's ~/.anie must keep pointing Electron at the real cookie file.
		const launched = spawnElectron(port, {
			...process.env,
			ANIE_DESKTOP_BIN: binary,
			ANIE_DESKTOP_CWD: cwd,
			HOME: tempHome,
			XAUTHORITY: process.env.XAUTHORITY ?? path.join(os.homedir(), '.Xauthority'),
			ELECTRON_DISABLE_SECURITY_WARNINGS: 'true',
		});
		electron = launched.electron;
		lastDiagnostics = launched.diagnostics;
		cdp = await connect(port);

		await waitFor(
			'document.body.dataset.processPhase',
			(value) => value === 'ready',
			'real processPhase is ready',
			25000,
		);
		const sessionId = await evaluate('document.querySelector(\'[data-testid="session-id"]\')?.textContent');
		check(typeof sessionId === 'string' && /^[0-9a-f]{8}$/.test(sessionId), 'session-id is eight hex chars');
		const model = await evaluate('document.querySelector(\'[data-testid="model-input"]\')?.value');
		check(model === 'gpt-4o', 'model-input value is gpt-4o');
		await shot('real-ready');

		await submit('Say hi in one word.');
		await waitFor(
			`[...document.querySelectorAll('[data-assistant-state="error"]')].map((n) => n.textContent)`,
			(texts) => Array.isArray(texts) && texts.some((text) => typeof text === 'string' && text.includes('Authentication failed')),
			'real prompt yields Authentication failed',
			30000,
		);
		await shot('real-auth-error');

		await evaluate(`(() => {
			const el = document.querySelector('[data-testid="thinking-select"]');
			el.value = "low";
			el.dispatchEvent(new Event("change", { bubbles: true }));
		})()`);
		await waitFor(
			`[...document.querySelectorAll('[data-item-kind="notice"]')].map((n) => n.textContent)`,
			(texts) => Array.isArray(texts) && texts.some((text) => typeof text === 'string' && text.includes('Thinking level set to low')),
			'real notice says Thinking level set to low',
		);
		await shot('real-thinking');

		await evaluate('window.close()');
		cdp.close();
		cdp = null;
		const closed = await withTimeout(launched.processClosed, 10000, 'real Electron did not exit after window.close()');
		check(closed.code === 0, `real Electron exited with code ${closed.code}`);
		electron = null;
		const sessionsDir = path.join(tempHome, '.anie', 'sessions');
		const entries = await readdir(sessionsDir).catch(() => []);
		const jsonl = entries.filter((name) => name.endsWith('.jsonl')).map((name) => path.join(sessionsDir, name));
		check(jsonl.length > 0, 'a session jsonl exists under the temp HOME');
		const firstLine = (await readFile(jsonl[0], 'utf8')).split('\n')[0] ?? '';
		check(firstLine.includes('"type":"session"'), 'session jsonl first line contains "type":"session"');
	}
}

await main();
