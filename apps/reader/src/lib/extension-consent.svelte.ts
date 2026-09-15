import type { ExtensionInstallAsk } from '@komika/api';
import { apiOrigin } from './config';

// A server can name an extension for the on-device engine to install. An extension is
// code that runs on this device, so the person is asked first (`ExtensionConsent.svelte`).
// The answer is kept per server and per repository: "Allow" for good, in localStorage;
// "Not now" until the app closes, so one chapter does not ask again on every page.

const ALLOWED_KEY = 'komika.allowedExtensionRepos';

type Pending = { key: string; ask: ExtensionInstallAsk; answer: (allowed: boolean) => void };

/** The questions waiting for the person; the dialog shows the first. */
export const extensionConsent = $state<{ pending: Pending[] }>({ pending: [] });

const refused = new Set<string>();
const asking = new Map<string, Promise<boolean>>();

function allowed(): string[] {
	try {
		const list: unknown = JSON.parse(localStorage.getItem(ALLOWED_KEY) ?? '[]');
		return Array.isArray(list) ? list.filter((k): k is string => typeof k === 'string') : [];
	} catch {
		return [];
	}
}

/** `confirmInstall` of the on-device backend. */
export function confirmExtensionInstall(ask: ExtensionInstallAsk): Promise<boolean> {
	// A package without a store comes from one the engine already has: keyed by itself.
	const key = `${apiOrigin}\n${ask.repoUrl ?? `package:${ask.pkgName}`}`;
	if (refused.has(key)) return Promise.resolve(false);
	if (allowed().includes(key)) return Promise.resolve(true);
	const open = asking.get(key);
	if (open) return open;
	const question = new Promise<boolean>((answer) => {
		extensionConsent.pending.push({ key, ask, answer });
	}).finally(() => asking.delete(key));
	asking.set(key, question);
	return question;
}

/** The person's answer to the first waiting question. */
export function answerExtensionInstall(allow: boolean): void {
	const first = extensionConsent.pending.shift();
	if (!first) return;
	if (allow) {
		try {
			localStorage.setItem(ALLOWED_KEY, JSON.stringify([...allowed(), first.key]));
		} catch {
			// Storage is off: the answer holds for this question only.
		}
	} else {
		refused.add(first.key);
	}
	first.answer(allow);
}
