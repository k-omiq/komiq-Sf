let opIdCounter = 0;

/**
 * A progress save's own id (at most 64 characters, as the server allows). Older webviews
 * still need unique identities when randomUUID is unavailable.
 */
export function createOpId(): string {
	if (globalThis.crypto?.randomUUID) return globalThis.crypto.randomUUID();
	return `${Date.now().toString(36)}-${++opIdCounter}-${Math.random().toString(36).slice(2)}`;
}
