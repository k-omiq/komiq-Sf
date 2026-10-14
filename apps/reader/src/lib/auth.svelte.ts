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
import { viewerCacheChanged } from './viewer-cache.svelte';

/** Where the session token is kept on this device. */
export const TOKEN_KEY = 'komika-token';

type User = Session['user'];

/** Reactive auth state — read `auth.user` / `auth.ready` in components. */
export const auth = $state<{ user: User | null; ready: boolean }>({
	user: null,
	ready: false,
});

/**
 * The temporary password this tab just signed in with, kept in memory only (never
 * saved) while the server asks for a new one, so the "Choose a new password" screen
 * does not ask for it a second time. Gone on reload; the screen then asks.
 */
let temporaryPassword: string | null = null;
/** Auth intent/token transitions only; content preference changes do not supersede logout. */
let authOperation = 0;

/** Whether the forced password screen still needs the current password typed. */
export function needsCurrentPassword(): boolean {
	return temporaryPassword === null;
}

function readToken(): string | null {
	try {
		return localStorage.getItem(TOKEN_KEY);
	} catch {
		return null;
	}
}

function persist(token: string | null): void {
	authOperation += 1;
	try {
		if (token) localStorage.setItem(TOKEN_KEY, token);
		else localStorage.removeItem(TOKEN_KEY);
	} catch {
		/* private mode / quota — token just won't survive reload */
	}
	backend.setToken?.(token);
	viewerCacheChanged();
}

/** Restore + validate a persisted session. Safe to call more than once. */
export async function initAuth(): Promise<void> {
	if (auth.ready) return;
	const operation = authOperation;
	const token = readToken();
	if (token) {
		backend.setToken?.(token);
		try {
			const session = await backend.session();
			// A concurrent login/logout may have swapped the token while session()
			// was in flight; if so, don't clobber that newer state with this stale
			// validation result.
			if (readToken() === token && authOperation === operation) {
				const previous = auth.user;
				auth.user = session?.user ?? null;
				// Bind the offline write-queue to this identity (C1) once validated.
				backend.setCurrentUser?.(auth.user?.id ?? null);
				if (!session)
					persist(null); // token was stale/revoked
				else if (
					previous?.id !== auth.user?.id ||
					previous?.showNsfw !== auth.user?.showNsfw ||
					previous?.isAdmin !== auth.user?.isAdmin
				)
					viewerCacheChanged();
			}
		} catch {
			// backend unreachable — stay logged out, keep token. Only null the user
			// if the token we validated is still the current one.
			if (readToken() === token && authOperation === operation) auth.user = null;
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
	const operation = authOperation;
	const token = readToken();
	if (!token) {
		auth.user = null;
		backend.setCurrentUser?.(null);
		persist(null);
		return null;
	}
	try {
		const session = await backend.session();
		// A concurrent login/logout may have swapped the token mid-flight; don't
		// clobber that newer state with this stale validation result.
		if (readToken() !== token || authOperation !== operation) return auth.user;
		const previous = auth.user;
		auth.user = session?.user ?? null;
		backend.setCurrentUser?.(auth.user?.id ?? null); // (C1) re-bind offline queue
		if (!session)
			persist(null); // token was stale/revoked
		else if (
			previous?.id !== auth.user?.id ||
			previous?.showNsfw !== auth.user?.showNsfw ||
			previous?.isAdmin !== auth.user?.isAdmin
		)
			viewerCacheChanged();
		return auth.user;
	} catch {
		return auth.user; // backend unreachable — keep the current state
	}
}

export async function login(username: string, password: string): Promise<void> {
	const operation = ++authOperation;
	const session = await backend.login(username, password);
	if (authOperation !== operation) return;
	persist(session.token);
	temporaryPassword = session.user.mustChangePassword ? password : null;
	auth.user = session.user;
	// Bind the offline write-queue to this identity (C1) before any write can enqueue.
	backend.setCurrentUser?.(session.user.id);
}

/**
 * Replace the signed-in user's password (the forced change after an admin created
 * or reset the account). `current` may be omitted right after sign-in: the password
 * just typed is used. Clears `mustChangePassword` on success.
 */
export async function changeOwnPassword(newPassword: string, current?: string): Promise<void> {
	if (!backend.changePassword) throw new Error('This server cannot change passwords.');
	const currentPassword = current ?? temporaryPassword;
	if (currentPassword === null) throw new Error('Enter your current password.');
	const user = await backend.changePassword(currentPassword, newPassword);
	temporaryPassword = null;
	if (auth.user) auth.user = { ...auth.user, ...user, mustChangePassword: false };
}

export async function register(input: RegisterInput): Promise<void> {
	const operation = ++authOperation;
	const session = await backend.register(input);
	if (authOperation !== operation) return;
	persist(session.token);
	auth.user = session.user;
	backend.setCurrentUser?.(session.user.id); // (C1) bind offline queue
}

export async function logout(): Promise<{ revoked: boolean }> {
	const token = readToken();
	const operation = ++authOperation;
	// Server revocation is best-effort: the local session is ALWAYS dropped, even
	// when the request fails — but the caller learns the token may still be live
	// server-side, so the UI can warn instead of implying revocation succeeded.
	let revoked = true;
	try {
		await backend.logout();
	} catch {
		revoked = false;
	}
	// A late logout must not erase a session established while revocation waited.
	if (readToken() !== token || authOperation !== operation) return { revoked };
	// Clear/scope the offline write-queue for this identity BEFORE the token is
	// dropped (C1), so a queued write can never replay under the next account.
	backend.setCurrentUser?.(null);
	persist(null);
	temporaryPassword = null;
	auth.user = null;
	return { revoked };
}

/**
 * Save the signed-in viewer's NSFW preference and apply what the server stored.
 *
 * Ownership is the ACCOUNT, not the viewer-cache generation: an unrelated change while
 * the write waits (block rules, a translator pick, a renewed token for the same user)
 * does not undo a write the server already applied, so it must still reach `auth.user`
 * and invalidate results fetched under the old setting. Another account's session
 * never receives it. Resolves to the stored value, or `null` when it no longer applies.
 */
export async function saveShowNsfw(show: boolean): Promise<boolean | null> {
	const owner = auth.user?.id;
	if (!owner || !backend.setShowNsfw) return null;
	const saved = await backend.setShowNsfw(show);
	if (!auth.user || auth.user.id !== owner) return null;
	if (auth.user.showNsfw !== saved) {
		auth.user.showNsfw = saved;
		viewerCacheChanged();
	}
	return saved;
}
