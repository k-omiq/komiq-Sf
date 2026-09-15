/**
 * Admin auth state. Same shape as the reader's store, but gated on `isAdmin`:
 * the console only admits admin accounts. Persists the bearer token and injects
 * it into the backend so the admin mutations carry it.
 */
import type { Session } from '@komika/api';
import { TOKEN_KEY } from './config';
import { backend } from './context';
import { setShowNsfw } from './data';

type User = Session['user'];

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
	try {
		if (token) localStorage.setItem(TOKEN_KEY, token);
		else localStorage.removeItem(TOKEN_KEY);
	} catch {
		/* private mode / quota */
	}
	backend.setToken?.(token);
}

/** Restore + validate a persisted admin session. */
export async function initAuth(): Promise<void> {
	if (auth.ready) return;
	const token = readToken();
	if (token) {
		backend.setToken?.(token);
		try {
			const session = await backend.session();
			// Only keep the session if it's actually an admin. A resolved-but-
			// invalid/non-admin token is cleared here; a genuine bad token also
			// resolves to null on the next successful round-trip.
			auth.user = session?.user?.isAdmin ? session.user : null;
			if (!auth.user) persist(null);
		} catch {
			// Transient failure (backend unreachable): stay logged out but keep
			// the token so a blip doesn't force a full re-login (server always
			// re-validates). Matches the reader's initAuth.
			auth.user = null;
		}
	}
	auth.ready = true;
}

/** Sign in with a session the server just issued (the setup wizard's owner account). */
export function adoptSession(session: Session): void {
	persist(session.token);
	auth.user = session.user;
}

export async function login(username: string, password: string): Promise<void> {
	const session = await backend.login(username, password);
	if (!session.user.isAdmin) {
		// Not an admin — drop the session we just created and refuse.
		backend.setToken?.(session.token);
		try {
			await backend.logout();
		} catch {
			/* ignore */
		}
		backend.setToken?.(null);
		throw new Error('This account does not have admin access.');
	}
	persist(session.token);
	temporaryPassword = session.user.mustChangePassword ? password : null;
	auth.user = session.user;
}

/**
 * Replace the signed-in admin's password (the forced first-sign-in change, or a
 * reset). `current` may be omitted right after sign-in: the password just typed is
 * used. Clears `mustChangePassword` on success.
 */
export async function changeOwnPassword(newPassword: string, current?: string): Promise<void> {
	if (!backend.changePassword) throw new Error('This server cannot change passwords.');
	const currentPassword = current ?? temporaryPassword;
	if (currentPassword === null) throw new Error('Enter your current password.');
	const user = await backend.changePassword(currentPassword, newPassword);
	temporaryPassword = null;
	if (auth.user) auth.user = { ...auth.user, ...user, mustChangePassword: false };
}

/**
 * Flip the signed-in admin's NSFW visibility and mirror it into `auth.user`.
 *
 * The console is gated by this flag server-side: source browsing, extension install
 * and source ingest all refuse an opted-out admin. (The console's own lists, Catalog
 * and Latest updates, are read NSFW-inclusive either way; see ADMIN_INCLUDE_NSFW.)
 * There is no other way to change it from here — without this the admin has to sign
 * into the reader with the same account to unblock their own console.
 */
export async function toggleNsfwVisibility(value: boolean): Promise<void> {
	const next = await setShowNsfw(value);
	if (auth.user) auth.user = { ...auth.user, showNsfw: next };
}

export async function logout(): Promise<{ revoked: boolean }> {
	// Server revocation is best-effort: the local session is ALWAYS dropped, even
	// when the request fails — but the caller learns the token may still be live
	// server-side, so the UI can warn instead of implying revocation succeeded.
	let revoked = true;
	try {
		await backend.logout();
	} catch {
		revoked = false;
	}
	persist(null);
	temporaryPassword = null;
	auth.user = null;
	return { revoked };
}
