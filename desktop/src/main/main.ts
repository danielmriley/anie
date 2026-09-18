import { app, BrowserWindow, dialog, ipcMain } from 'electron';
import { existsSync } from 'node:fs';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';

import { AnieProcess } from './anie.js';
import {
	THINKING_LEVELS,
	assertNever,
	type Command,
	type ThinkingLevel,
} from '../shared/domain.js';

const appRoot = path.join(import.meta.dirname, '..', '..', '..');

let win: BrowserWindow | null = null;
let cwd = '';
let quitting = false;

const anie = new AnieProcess((push) => {
	if (win && !win.isDestroyed()) win.webContents.send('push', push);
});

function settingsPath(): string {
	return path.join(app.getPath('userData'), 'settings.json');
}

async function readSavedCwd(): Promise<string | null> {
	try {
		const raw = await readFile(settingsPath(), 'utf8');
		const parsed = JSON.parse(raw) as { cwd?: unknown };
		return typeof parsed.cwd === 'string' ? parsed.cwd : null;
	} catch {
		return null;
	}
}

async function persistCwd(next: string): Promise<void> {
	await mkdir(path.dirname(settingsPath()), { recursive: true });
	await writeFile(settingsPath(), JSON.stringify({ cwd: next }), 'utf8');
}

async function resolveStartupCwd(): Promise<string> {
	const fromEnv = process.env.ANIE_DESKTOP_CWD;
	if (fromEnv) return fromEnv;
	const saved = await readSavedCwd();
	if (saved && existsSync(saved)) return saved;
	return app.getPath('home');
}

function isThinkingLevel(value: string): value is ThinkingLevel {
	return (THINKING_LEVELS as readonly string[]).includes(value);
}

function parseCommand(raw: unknown): Command | null {
	if (typeof raw !== 'object' || raw === null) return null;
	const kind = (raw as { kind?: unknown }).kind;
	if (kind === 'prompt') {
		const text = (raw as { text?: unknown }).text;
		return typeof text === 'string' ? { kind: 'prompt', text } : null;
	}
	if (kind === 'abort') return { kind: 'abort' };
	if (kind === 'set_thinking') {
		const level = (raw as { level?: unknown }).level;
		return typeof level === 'string' && isThinkingLevel(level) ? { kind: 'set_thinking', level } : null;
	}
	if (kind === 'set_model') {
		const model = (raw as { model?: unknown }).model;
		return typeof model === 'string' ? { kind: 'set_model', model } : null;
	}
	if (kind === 'set_cwd') {
		const p = (raw as { path?: unknown }).path;
		return typeof p === 'string' ? { kind: 'set_cwd', path: p } : null;
	}
	if (kind === 'new_session') return { kind: 'new_session' };
	return null;
}

function createWindow(): void {
	win = new BrowserWindow({
		width: 1100,
		height: 760,
		backgroundColor: '#121417',
		title: 'anie',
		webPreferences: {
			preload: path.join(import.meta.dirname, '..', 'preload.cjs'),
			contextIsolation: true,
			sandbox: true,
			nodeIntegration: false,
		},
	});

	win.webContents.on('will-navigate', (event) => {
		event.preventDefault();
	});
	win.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));

	win.on('closed', () => {
		win = null;
	});

	void win.loadFile(path.join(appRoot, 'index.html'));

	win.webContents.once('did-finish-load', () => {
		anie.start({ cwd });
	});

	ipcMain.handle('command', async (_event, raw: unknown) => {
		const command = parseCommand(raw);
		if (!command) return;
		switch (command.kind) {
			case 'prompt':
			case 'abort':
			case 'set_thinking':
			case 'set_model':
				anie.send(command);
				break;
			case 'set_cwd':
				await persistCwd(command.path);
				cwd = command.path;
				await anie.restart({ cwd });
				break;
			case 'new_session':
				await anie.restart({ cwd: anie.state.cwd || cwd });
				break;
			default:
				assertNever(command, 'command');
		}
	});

	ipcMain.handle('bootstrap', () => ({
		process: anie.state,
		status: anie.lastStatus,
	}));

	ipcMain.handle('pick-directory', async () => {
		if (!win) return null;
		const result = await dialog.showOpenDialog(win, {
			properties: ['openDirectory'],
			defaultPath: cwd,
		});
		return result.canceled ? null : (result.filePaths[0] ?? null);
	});
}

void app.whenReady().then(async () => {
	cwd = await resolveStartupCwd();
	createWindow();
});

app.on('before-quit', (event) => {
	if (!quitting) {
		quitting = true;
		event.preventDefault();
		void anie.stop().finally(() => {
			app.exit(0);
		});
	}
});

// A windowless anie child would keep holding the session lock, so the app
// quits on close even on macOS (deviation from the platform convention).
app.on('window-all-closed', () => {
	app.quit();
});
