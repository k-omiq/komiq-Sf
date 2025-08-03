import type { Handle } from '@sveltejs/kit';

// Workers Assets headers do not cover dynamically rendered HTML. Apply these
// to SvelteKit responses as well; SvelteKit supplies the bootstrap-safe CSP.
export const handle: Handle = async ({ event, resolve }) => {
	const response = await resolve(event);
	response.headers.set('X-Content-Type-Options', 'nosniff');
	response.headers.set('X-Frame-Options', 'DENY');
	response.headers.set('Referrer-Policy', 'strict-origin-when-cross-origin');
	response.headers.set(
		'Permissions-Policy',
		'camera=(), microphone=(), geolocation=(), payment=(), usb=()',
	);
	return response;
};
