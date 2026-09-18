const { contextBridge, ipcRenderer } = require('electron');

import type { AnieBridge, Bootstrap, Command, Push } from './shared/domain.js';

const bridge: AnieBridge = {
	send: (command: Command) => ipcRenderer.invoke('command', command),
	bootstrap: (): Promise<Bootstrap> => ipcRenderer.invoke('bootstrap'),
	pickDirectory: (): Promise<string | null> => ipcRenderer.invoke('pick-directory'),
	onPush: (listener: (push: Push) => void) => {
		const handler = (_event: unknown, push: Push) => {
			listener(push);
		};
		ipcRenderer.on('push', handler);
		return () => {
			ipcRenderer.removeListener('push', handler);
		};
	},
};

contextBridge.exposeInMainWorld('anie', bridge);
