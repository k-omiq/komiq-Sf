/**
 * Setup wizard state, shared by the step components in `./steps`.
 *
 * The step ORDER lives in `+page.svelte` (`STEPS`); each step only knows its own id.
 * Progress that is safe to keep (never a password or token) is saved, so closing the
 * app and coming back resumes on the same step with the same choices.
 */
import { goto } from '$app/navigation';
import { href } from '$lib/paths';

const PROGRESS_KEY = 'komiq-setup';

export type Signup = 'closed' | 'open';

export interface AddedUser {
	username: string;
	/** Shown once, in this visit only. Never saved. */
	password: string | null;
}

interface Saved {
	current: string;
	signup: Signup;
	added: string[];
	/** Free-form, per-step choices (e.g. `access`, `mangadex`). Never secrets. */
	extra: Record<string, unknown>;
}

function load(): Saved {
	const empty: Saved = { current: 'welcome', signup: 'closed', added: [], extra: {} };
	try {
		const raw = localStorage.getItem(PROGRESS_KEY);
		if (raw) return { ...empty, ...JSON.parse(raw) };
	} catch {
		/* fall through */
	}
	return empty;
}

const saved = load();

export const wizard = $state<{
	/** Ids and labels of the steps on THIS install, in order (set by the page). */
	order: { id: string; label: string }[];
	current: string;
	signup: Signup;
	added: AddedUser[];
	extra: Record<string, unknown>;
	/** A message the summary should repeat (e.g. a setting the server pins). */
	notices: string[];
}>({
	order: [],
	current: saved.current,
	signup: saved.signup,
	added: saved.added.map((username) => ({ username, password: null })),
	extra: saved.extra,
	notices: [],
});

export function persist(): void {
	try {
		localStorage.setItem(
			PROGRESS_KEY,
			JSON.stringify({
				current: wizard.current,
				signup: wizard.signup,
				added: wizard.added.map((u) => u.username),
				extra: wizard.extra,
			}),
		);
	} catch {
		/* private mode: progress lasts for this visit */
	}
}

/** 1-based position of a step, for the step bar. */
export function stepNumber(id: string): number {
	return wizard.order.findIndex((s) => s.id === id) + 1;
}

export function labels(): string[] {
	return wizard.order.map((s) => s.label);
}

function move(delta: number): void {
	const i = wizard.order.findIndex((s) => s.id === wizard.current);
	const next = wizard.order[i + delta];
	if (next) wizard.current = next.id;
}

export const next = () => move(1);
export const back = () => move(-1);

export function goToStep(id: string): void {
	if (wizard.order.some((s) => s.id === id)) wizard.current = id;
}

/** Leave the wizard for good and open `path` in the console. */
export function finishSetup(path: string): void {
	try {
		localStorage.removeItem(PROGRESS_KEY);
	} catch {
		/* ignore */
	}
	void goto(href(path), { replaceState: true });
}

export async function copyText(text: string): Promise<boolean> {
	try {
		await navigator.clipboard.writeText(text);
		return true;
	} catch {
		return false;
	}
}
