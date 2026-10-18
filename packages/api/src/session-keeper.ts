/**
 * Keeps a signed-in client's view of its session in step with the server.
 *
 * Only the server can end a session: an expired, revoked or banned token makes the
 * `session` query answer null. Anything else (a 504, a restart during a deploy, a
 * dropped phone connection) is "no answer", and must never look like a sign-out. So
 * after a check that got no answer the keeper asks again, backing off, and it also
 * asks when the window comes back into view, when the network returns, and when the
 * server refuses a request because the token no longer signs anyone in.
 */
import type { Session } from './backend.js';

/** What the server answers a request whose token is missing, expired or revoked. */
export const NOT_AUTHENTICATED = 'Not authenticated';

const rejectedListeners = new Set<() => void>();

/** A request that carried a token was refused with {@link NOT_AUTHENTICATED}. */
export function onAuthRejected(listener: () => void): () => void {
	rejectedListeners.add(listener);
	return () => {
		rejectedListeners.delete(listener);
	};
}

/** Called by a backend when the server refused the token it sent. */
export function notifyAuthRejected(): void {
	for (const listener of rejectedListeners) {
		try {
			listener();
		} catch (e) {
			console.warn('[komika] auth rejected listener failed:', e);
		}
	}
}

/** Waits before asking again after a check got no answer. The last one repeats. */
const RETRY_MS = [2_000, 5_000, 15_000, 30_000, 60_000, 120_000, 300_000];
/** Coming back into view or back online asks at most this often. */
const RECHECK_MS = 60_000;
/** A run of refused requests asks once, not once per request. */
const REJECTED_RECHECK_MS = 10_000;

export interface SessionKeeperOptions {
	/** Asks the server. Throws when no answer came back. */
	session(): Promise<Session | null>;
	readToken(): string | null;
	/** Changes on every login, logout and token swap. */
	operation(): number;
	/** The server answered for the stored token. `null`: it has no session for it. */
	apply(session: Session | null): void;
	/** No token is stored any more. */
	cleared(): void;
}

export interface SessionKeeper {
	/** Ask now. True when the server answered, false when it did not. Calls made
	 *  while a check is in flight share it. */
	check(): Promise<boolean>;
	/** Start the re-checks (retries, focus, network, refused requests). Returns the
	 *  function that stops them. */
	watch(): () => void;
}

export function createSessionKeeper(options: SessionKeeperOptions): SessionKeeper {
	let inFlight: Promise<boolean> | undefined;
	let lastAsked = 0;
	let failures = 0;
	let retry: ReturnType<typeof setTimeout> | undefined;
	/** Live `watch()` calls (a remount can overlap the old one). */
	let watching = 0;

	async function ask(): Promise<boolean> {
		for (;;) {
			const operation = options.operation();
			const token = options.readToken();
			if (!token) {
				options.cleared();
				return true;
			}
			let session: Session | null;
			try {
				session = await options.session();
			} catch {
				if (stale(token, operation)) continue;
				return false;
			}
			// A login, logout or token swap landed while the answer was on its way: it
			// was about another token, so ask again about the one stored now.
			if (stale(token, operation)) continue;
			options.apply(session);
			return true;
		}
	}

	function stale(token: string, operation: number): boolean {
		return options.readToken() !== token || options.operation() !== operation;
	}

	function scheduleRetry(): void {
		clearTimeout(retry);
		retry = undefined;
		if (watching === 0 || failures === 0) return;
		const wait = RETRY_MS[Math.min(failures - 1, RETRY_MS.length - 1)];
		retry = setTimeout(() => void check(), wait);
	}

	function check(): Promise<boolean> {
		inFlight ??= (async () => {
			lastAsked = Date.now();
			try {
				const answered = await ask();
				failures = answered ? 0 : failures + 1;
				scheduleRetry();
				return answered;
			} finally {
				inFlight = undefined;
			}
		})();
		return inFlight;
	}

	function watch(): () => void {
		if (typeof window === 'undefined' || typeof document === 'undefined') return () => {};
		watching += 1;
		const askIfDue = (minMs: number) => {
			if (Date.now() - lastAsked >= minMs) void check();
		};
		// After a failed check, coming back into view or online asks at once.
		const onVisible = () => {
			if (document.visibilityState !== 'hidden') askIfDue(failures > 0 ? 0 : RECHECK_MS);
		};
		const onOnline = () => askIfDue(failures > 0 ? 0 : RECHECK_MS);
		document.addEventListener('visibilitychange', onVisible);
		window.addEventListener('focus', onVisible);
		window.addEventListener('online', onOnline);
		const stopRejected = onAuthRejected(() => askIfDue(REJECTED_RECHECK_MS));
		scheduleRetry();
		let stopped = false;
		return () => {
			if (stopped) return;
			stopped = true;
			watching -= 1;
			if (watching === 0) {
				clearTimeout(retry);
				retry = undefined;
			}
			document.removeEventListener('visibilitychange', onVisible);
			window.removeEventListener('focus', onVisible);
			window.removeEventListener('online', onOnline);
			stopRejected();
		};
	}

	return { check, watch };
}
