import { flowHtml } from '$lib/server/content';

// The two ways a page image reaches a reader. Each hop is checked against the code
// it describes: apps/worker/src/index.ts and apps/reader/src-tauri/src/lib.rs.
const traces = {
	web: [
		{
			from: 'Reader in your browser',
			to: 'Image proxy',
			what: 'Asks for the page by its source address.',
		},
		{ from: 'Image proxy', to: 'Edge cache', what: 'Looks for a copy it fetched before.' },
		{
			from: 'Image proxy',
			to: 'Source',
			what: 'On a miss, fetches the image. The host must be on the allow list.',
		},
		{
			from: 'Image proxy',
			to: 'Reader in your browser',
			what: 'Returns the image and stores a copy at the edge.',
		},
	],
	native: [
		{
			from: 'Reader app',
			to: 'Rust core',
			what: 'Asks for the page with the fetch_image command.',
		},
		{
			from: 'Rust core',
			to: 'Source',
			what: 'Fetches the image directly. No proxy sits in between.',
		},
		{ from: 'Rust core', to: 'Reader app', what: 'Hands the bytes back to be shown.' },
	],
};

export function load() {
	return {
		traces: { web: flowHtml(traces.web), native: flowHtml(traces.native) },
	};
}
