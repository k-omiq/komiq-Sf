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

/**
 * Copy `text` to the clipboard. `navigator.clipboard` exists on HTTPS and localhost
 * only, and a console on a home network is often open over plain HTTP. There, or when
 * the browser refuses, the text is copied the old way: from a hidden field. False only
 * when both ways fail.
 */
export async function copyText(text: string): Promise<boolean> {
	try {
		await navigator.clipboard.writeText(text);
		return true;
	} catch {
		// No clipboard API here, or it refused: the hidden field below.
	}
	const focused = document.activeElement;
	const field = document.createElement('textarea');
	field.value = text;
	field.readOnly = true;
	field.style.cssText = 'position:fixed;top:0;left:0;opacity:0';
	document.body.append(field);
	try {
		field.focus();
		field.select();
		field.setSelectionRange(0, text.length);
		return document.execCommand('copy');
	} catch {
		return false;
	} finally {
		field.remove();
		if (focused instanceof HTMLElement) focused.focus();
	}
}
