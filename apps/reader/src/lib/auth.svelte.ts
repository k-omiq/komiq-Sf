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
import { createSessionKeeper, type RegisterInput, type Session } from '@komika/api';
import { backend } from './context';
import { viewerCacheChanged } from './viewer-cache.svelte';

/** Where the session token is kept on this device. */
export const TOKEN_KEY = 'komika-token';
/** The last user the server confirmed for the stored token. Shown on start-up until
 *  the server answers, so a failed check never looks like a sign-out. */
export const USER_KEY = 'komika-user';

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

/** Save the confirmed user for the stored token. `persist` drops it with the token. */
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
		/* private mode / quota — token just won't survive reload */
	}
	useToken(token);
	viewerCacheChanged();
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
		const previous = auth.user;
		auth.user = session?.user ?? null;
		// Bind the offline write-queue to this identity (C1) once validated.
		backend.setCurrentUser?.(auth.user?.id ?? null);
		if (!session) {
			persist(null); // token was expired/revoked
			return;
		}
		remember(session.user);
		if (
			previous?.id !== session.user.id ||
			previous?.showNsfw !== session.user.showNsfw ||
			previous?.isAdmin !== session.user.isAdmin
		)
			viewerCacheChanged();
	},
	cleared() {
		if (!auth.user) return;
		auth.user = null;
		backend.setCurrentUser?.(null);
		persist(null);
	},
});

/**
 * Restore + validate a persisted session. Safe to call more than once.
 *
 * The last user the server confirmed for this token is shown at once. Only the
 * server's answer that the session is gone signs the user out: when the check gets
 * no answer (outage, deploy, bad network) the user stays signed in and the keeper
 * asks again (see {@link watchSession}).
 */
export async function initAuth(): Promise<void> {
	if (auth.ready) return;
	const token = readToken();
	if (token) {
		useToken(token);
		const remembered = auth.user ? null : readRememberedUser();
		if (remembered) {
			auth.user = remembered;
			backend.setCurrentUser?.(remembered.id);
			viewerCacheChanged();
		}
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

/**
 * Re-validate the persisted session against the server. Clears the local user
 * when the server reports NO session (token expired/revoked) so gated screens
 * fall through to their signed-out state; leaves `auth.user` intact on a network
 * error so a transient outage doesn't spuriously sign the user out. Returns the
 * resolved user (or null). Cheap to call opportunistically (e.g. when a
 * per-user fetch comes back empty for a supposedly signed-in viewer).
 */
export async function revalidateSession(): Promise<User | null> {
	await keeper.check();
	return auth.user;
}

/** `turnstileToken`: from the form's `TurnstileWidget`, for a server that asks for one. */
export async function login(
	username: string,
	password: string,
	turnstileToken?: string,
): Promise<void> {
	const operation = ++authOperation;
	const session = await backend.login(username, password, turnstileToken);
	if (authOperation !== operation) return;
	persist(session.token);
	remember(session.user);
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

export async function register(input: RegisterInput): Promise<void> {
	const operation = ++authOperation;
	const session = await backend.register(input);
	if (authOperation !== operation) return;
	persist(session.token);
	remember(session.user);
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
		remember(auth.user);
		viewerCacheChanged();
	}
	return saved;
}
