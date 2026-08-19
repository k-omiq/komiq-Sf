/**
 * Temporary passwords an admin hands to a person once (new account, reset). The
 * person picks their own at first sign-in, so these only need to be easy to read
 * out and type: 12 symbols from an alphabet without look-alikes, in three groups.
 */
export function tempPassword(): string {
	const alphabet = 'abcdefghjkmnpqrstuvwxyzABCDEFGHJKMNPQRSTUVWXYZ23456789';
	const bytes = crypto.getRandomValues(new Uint8Array(12));
	const raw = Array.from(bytes, (b) => alphabet[b % alphabet.length]).join('');
	return `${raw.slice(0, 4)}-${raw.slice(4, 8)}-${raw.slice(8)}`;
}

export async function copyText(text: string): Promise<boolean> {
	try {
		await navigator.clipboard.writeText(text);
		return true;
	} catch {
		return false;
	}
}
