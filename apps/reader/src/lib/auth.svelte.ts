/**
 * Client-side auth state.
 *
 * Holds the current signed-in user, persists the bearer token to localStorage,
 * and injects it into the `backend` seam so authenticated GraphQL requests
 * (postReview / postComment / …) carry it. On first load it restores the token
 * and validates it against the server via `session()`.
 *
 * Only the unified Komika backend has real auth; the Suwayomi adapter has none
 * (its `login`/`register` throw and it omits `setToken`), so the login screen
 * surfaces those errors gracefully.
 */
import type { RegisterInput, Session } from '@komika/api';
import { backend } from './context';

const TOKEN_KEY = 'komika-token';

type User = Session['user'];

/** Reactive auth state — read `auth.user` / `auth.ready` in components. */
export const auth = $state<{ user: User | null; ready: boolean }>({
	user: null,
	ready: false,
});

function readToken(): string | null {
	try {
		return localStorage.getItem(TOKEN_KEY);
	} catch {
		return null;
	}
}

function persist(token: string | null): void {
	try {
		if (token) localStorage.setItem(TOKEN_KEY, token);
		else localStorage.removeItem(TOKEN_KEY);
	} catch {
		/* private mode / quota — token just won't survive reload */
	}
	backend.setToken?.(token);
}

/** Restore + validate a persisted session. Safe to call more than once. */
export async function initAuth(): Promise<void> {
	if (auth.ready) return;
	const token = readToken();
	if (token) {
		backend.setToken?.(token);
		try {
			const session = await backend.session();
			// A concurrent login/logout may have swapped the token while session()
			// was in flight; if so, don't clobber that newer state with this stale
			// validation result.
			if (readToken() === token) {
				auth.user = session?.user ?? null;
				// Bind the offline write-queue to this identity (C1) once validated.
				backend.setCurrentUser?.(auth.user?.id ?? null);
				if (!session) persist(null); // token was stale/revoked
			}
		} catch {
			// backend unreachable — stay logged out, keep token. Only null the user
			// if the token we validated is still the current one.
			if (readToken() === token) auth.user = null;
		}
	}
	auth.ready = true;
}

/**
 * Re-validate the persisted session against the server. Clears the local user
 * when the server reports NO session (token expired/revoked) so gated screens
 * fall through to their signed-out state; leaves `auth.user` intact on a network
 * error so a transient outage doesn't spuriously sign the user out. Returns the
 * resolved user (or null). Cheap to call opportunistically (e.g. when a
 * per-user fetch comes back empty for a supposedly signed-in viewer).
 */
export async function revalidateSession(): Promise<User | null> {
	const token = readToken();
	if (!token) {
		auth.user = null;
		return null;
	}
	try {
		const session = await backend.session();
		// A concurrent login/logout may have swapped the token mid-flight; don't
		// clobber that newer state with this stale validation result.
		if (readToken() !== token) return auth.user;
		auth.user = session?.user ?? null;
		backend.setCurrentUser?.(auth.user?.id ?? null); // (C1) re-bind offline queue
		if (!session) persist(null); // token was stale/revoked
		return auth.user;
	} catch {
		return auth.user; // backend unreachable — keep the current state
	}
}

export async function login(username: string, password: string): Promise<void> {
	const session = await backend.login(username, password);
	persist(session.token);
	auth.user = session.user;
	// Bind the offline write-queue to this identity (C1) before any write can enqueue.
	backend.setCurrentUser?.(session.user.id);
}

export async function register(input: RegisterInput): Promise<void> {
	const session = await backend.register(input);
	persist(session.token);
	auth.user = session.user;
	backend.setCurrentUser?.(session.user.id); // (C1) bind offline queue
}

export async function logout(): Promise<void> {
	try {
		await backend.logout();
	} catch {
		/* even if the server call fails, drop the local session */
	}
	// Clear/scope the offline write-queue for this identity BEFORE the token is
	// dropped (C1), so a queued write can never replay under the next account.
	backend.setCurrentUser?.(null);
	persist(null);
	auth.user = null;
}
