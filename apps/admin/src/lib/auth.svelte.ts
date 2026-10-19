/**
 * Admin auth state. Same shape as the reader's store, but gated on `isAdmin`:
 * the console only admits admin accounts. Persists the bearer token and injects
 * it into the backend so the admin mutations carry it.
 */
import { createSessionKeeper, type Session } from '@komika/api';
import { TOKEN_KEY } from './config';
import { backend } from './context';
import { setShowNsfw } from './data';

type User = Session['user'];

/** The last admin the server confirmed for the stored token. Shown on start-up until
 *  the server answers, so a failed check never looks like a sign-out. */
const USER_KEY = 'komika-admin-user';

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
/** Bumped by every login, logout and token swap. */
let authOperation = 0;
/** The token this tab's backend sends. Another tab can store a different one. */
let tabToken: string | null = null;

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

function useToken(token: string | null): void {
	tabToken = token;
	backend.setToken?.(token);
}

function readRememberedUser(): User | null {
	try {
		const saved = localStorage.getItem(USER_KEY);
		return saved ? (JSON.parse(saved) as User) : null;
	} catch {
		return null;
	}
}

/** Save the confirmed admin for the stored token. `persist` drops it with the token. */
function remember(user: User): void {
	// A tab still on an older token must not pair its user with another tab's token.
	if (readToken() !== tabToken) return;
	try {
		localStorage.setItem(USER_KEY, JSON.stringify(user));
	} catch {
		/* private mode / quota — start-up just waits for the server */
	}
}

function persist(token: string | null): void {
	authOperation += 1;
	try {
		// The saved user belongs to the old token; whoever sets a new one saves its user.
		localStorage.removeItem(USER_KEY);
		if (token) localStorage.setItem(TOKEN_KEY, token);
		else localStorage.removeItem(TOKEN_KEY);
	} catch {
		/* private mode / quota */
	}
	useToken(token);
}

/** Asks the server about the stored token: on start-up, after failures, on focus. */
const keeper = createSessionKeeper({
	session: () => {
		// Ask about the stored token, which another tab may have replaced.
		useToken(readToken());
		return backend.session();
	},
	readToken,
	operation: () => authOperation,
	apply(session) {
		// Only an admin session is kept; a non-admin or dead token is cleared.
		auth.user = session?.user?.isAdmin ? session.user : null;
		if (auth.user) remember(auth.user);
		else persist(null);
	},
	cleared() {
		auth.user = null;
	},
});

/**
 * Restore + validate a persisted admin session. The last admin the server confirmed
 * is shown at once; only the server's answer that the session is gone signs out. A
 * check with no answer (outage, deploy) keeps the admin in and is retried.
 */
export async function initAuth(): Promise<void> {
	if (auth.ready) return;
	const token = readToken();
	if (token) {
		useToken(token);
		auth.user ??= readRememberedUser();
		await keeper.check();
	}
	auth.ready = true;
}

/**
 * Re-check the session after a failed start-up check (backing off), when the window
 * comes back into view or online, and when the server refuses a request as not signed
 * in. Returns the function that stops it. Call once from the root layout.
 */
export function watchSession(): () => void {
	return keeper.watch();
}

/** Sign in with a session the server just issued (the setup wizard's owner account). */
export function adoptSession(session: Session): void {
	persist(session.token);
	remember(session.user);
	auth.user = session.user;
}

/** `turnstileToken`: from the form's `TurnstileWidget`, for a server that asks for one. */
export async function login(
	username: string,
	password: string,
	turnstileToken?: string,
): Promise<void> {
	const session = await backend.login(username, password, turnstileToken);
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
	remember(session.user);
	temporaryPassword = session.user.mustChangePassword ? password : null;
	auth.user = session.user;
}

/**
 * Replace the signed-in admin's password (the forced first-sign-in change, or a
 * reset). `current` may be omitted right after sign-in: the password just typed is
 * used. Clears `mustChangePassword` on success.
 */
export async function changeOwnPassword(
	newPassword: string,
	current?: string,
	turnstileToken?: string,
): Promise<void> {
	if (!backend.changePassword) throw new Error('This server cannot change passwords.');
	const currentPassword = current ?? temporaryPassword;
	if (currentPassword === null) throw new Error('Enter your current password.');
	const user = await backend.changePassword(currentPassword, newPassword, turnstileToken);
	temporaryPassword = null;
	if (auth.user) {
		auth.user = { ...auth.user, ...user, mustChangePassword: false };
		remember(auth.user);
	}
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
	if (auth.user) {
		auth.user = { ...auth.user, showNsfw: next };
		remember(auth.user);
	}
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
