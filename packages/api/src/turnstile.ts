/**
 * Cloudflare Turnstile in the browser: the check a server asks for on sign-up, sign-in,
 * password changes and resets, comments, reviews and reports, when it reports a site key
 * ({@link Backend.turnstileSiteKey}). The server checks each token with Cloudflare
 * (`apps/server/src/turnstile.rs`).
 *
 * A {@link TurnstileCheck} renders one widget into a form. It starts the check at once
 * and mostly shows nothing (`interaction-only`), so a token is usually ready by the time
 * the form is sent. A token is single use: {@link TurnstileCheck.token} hands it out and
 * starts the next check, so a second try (a mistyped password) gets a fresh one.
 *
 * The native apps cannot run the widget: Cloudflare accepts it only on the hostnames the
 * widget lists, which are websites. They show {@link TURNSTILE_NATIVE_MESSAGE} instead.
 */
import type { Backend } from './backend.js';

/** Explicit rendering: the script renders nothing until {@link TurnstileCheck} asks. */
export const TURNSTILE_SCRIPT =
	'https://challenges.cloudflare.com/turnstile/v0/api.js?render=explicit';

/** What a native app shows on a form of a server that asks for the check. */
export const TURNSTILE_NATIVE_MESSAGE =
	'This server asks for a security check that only its website can show. Sign in and post on the website.';

const LOAD_FAILED =
	'The security check could not load. Check your connection, or allow challenges.cloudflare.com in your content blocker, then try again.';
const CHECK_FAILED = 'The security check did not pass. Try again.';
const UNSUPPORTED = 'This browser cannot run the security check.';
const TIMED_OUT = 'The security check took too long. Try again.';
const REMOVED = 'The security check was closed. Try again.';

/** Never wait longer than this for the script. */
const LOAD_TIMEOUT_MS = 15_000;
/** Never hold a form longer than this for a token (an interactive check included). */
const TOKEN_TIMEOUT_MS = 120_000;

/** The part of Cloudflare's `window.turnstile` this module uses. */
export interface TurnstileApi {
	render(container: HTMLElement, options: Record<string, unknown>): string;
	reset(widgetId: string): void;
	remove(widgetId: string): void;
}

type TurnstileWindow = { turnstile?: TurnstileApi };

let loading: Promise<TurnstileApi> | null = null;

/** Load Cloudflare's script once per page. A failed load is not kept: the next try loads again. */
function loadTurnstile(): Promise<TurnstileApi> {
	loading ??= new Promise<TurnstileApi>((resolve, reject) => {
		const w = window as unknown as TurnstileWindow;
		if (w.turnstile) {
			resolve(w.turnstile);
			return;
		}
		const script = document.createElement('script');
		const fail = () => {
			clearTimeout(timer);
			script.remove();
			loading = null;
			reject(new Error(LOAD_FAILED));
		};
		const timer = setTimeout(fail, LOAD_TIMEOUT_MS);
		script.src = TURNSTILE_SCRIPT;
		script.async = true;
		script.onload = () => {
			clearTimeout(timer);
			if (w.turnstile) resolve(w.turnstile);
			else fail();
		};
		script.onerror = fail;
		document.head.append(script);
	});
	return loading;
}

interface Waiter {
	resolve(token: string): void;
	reject(error: Error): void;
}

/** One widget in one form. Call {@link remove} when the form goes away. */
export class TurnstileCheck {
	#api: TurnstileApi | null = null;
	#widget: string | null = null;
	#token: string | null = null;
	/** The widget reported an error; the next {@link token} call starts it again. */
	#erred = false;
	/** The check cannot run here at all (the script did not load, or the browser is unsupported). */
	#broken: Error | null = null;
	#removed = false;
	#waiters: Waiter[] = [];

	constructor(container: HTMLElement, siteKey: string) {
		loadTurnstile().then(
			(api) => {
				if (this.#removed) return;
				this.#api = api;
				this.#widget = api.render(container, {
					sitekey: siteKey,
					appearance: 'interaction-only',
					theme: 'auto',
					// The token goes in the mutation, not in a form field.
					'response-field': false,
					callback: (token: string) => this.#issue(token),
					'expired-callback': () => {
						this.#token = null;
					},
					'error-callback': () => {
						this.#token = null;
						this.#erred = true;
						this.#settle(new Error(CHECK_FAILED));
						// Handled: Cloudflare then does not raise it in the console as well.
						return true;
					},
					'unsupported-callback': () => this.#break(new Error(UNSUPPORTED)),
				});
			},
			(error: Error) => this.#break(error),
		);
	}

	/**
	 * A fresh token for one request. Waits for the check when it has not finished (the
	 * visitor may have to tick the box). Rejects with a message the form shows.
	 */
	token(): Promise<string> {
		if (this.#broken) return Promise.reject(this.#broken);
		if (this.#removed) return Promise.reject(new Error(REMOVED));
		if (this.#token) {
			const token = this.#token;
			this.#token = null;
			this.#restart();
			return Promise.resolve(token);
		}
		if (this.#erred) {
			this.#erred = false;
			this.#restart();
		}
		return new Promise<string>((resolve, reject) => {
			const waiter: Waiter = {
				resolve: (token) => {
					clearTimeout(timer);
					resolve(token);
				},
				reject: (error) => {
					clearTimeout(timer);
					reject(error);
				},
			};
			const timer = setTimeout(() => {
				this.#waiters = this.#waiters.filter((w) => w !== waiter);
				reject(new Error(TIMED_OUT));
			}, TOKEN_TIMEOUT_MS);
			this.#waiters.push(waiter);
		});
	}

	remove(): void {
		this.#removed = true;
		if (this.#api && this.#widget !== null) this.#api.remove(this.#widget);
		this.#widget = null;
		this.#settle(new Error(REMOVED));
	}

	#issue(token: string): void {
		const waiter = this.#waiters.shift();
		if (!waiter) {
			this.#token = token;
			return;
		}
		waiter.resolve(token);
		// That token is spent: start the check for the next request.
		this.#restart();
	}

	#restart(): void {
		if (this.#api && this.#widget !== null) this.#api.reset(this.#widget);
	}

	#break(error: Error): void {
		this.#broken = error;
		this.#settle(error);
	}

	/** Reject every request waiting for a token. */
	#settle(error: Error): void {
		const waiters = this.#waiters;
		this.#waiters = [];
		for (const waiter of waiters) waiter.reject(error);
	}
}

/**
 * The site key of the server `backend` talks to, asked once: `null` when it asks for no
 * check, or cannot say (a server that predates Turnstile, or a backend without it). A
 * failed request is not kept, so the next form asks again instead of sending no token to
 * a server that wants one.
 */
export function turnstileKeyLoader(
	backend: Pick<Backend, 'turnstileSiteKey'>,
): () => Promise<string | null> {
	let pending: Promise<string | null> | null = null;
	return () => {
		pending ??= (backend.turnstileSiteKey?.() ?? Promise.resolve(null)).catch((error: unknown) => {
			pending = null;
			throw error;
		});
		return pending;
	};
}
