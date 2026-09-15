// Shim for `@tauri-apps/api/core`'s `invoke`, mapping the Tauri commands the
// backends use onto a REAL booted Suwayomi engine over loopback HTTP. Node 26 `fetch`.
// `globalThis.__SUWA_DEVICE_PORT` (set by the harness) points the "device" at a second
// engine, so a read can be proven on an engine that has never seen the manga.
const base = () =>
	`http://127.0.0.1:${globalThis.__SUWA_DEVICE_PORT || process.env.SUWA_PORT || '4572'}`;

export async function invoke(cmd, args) {
	const BASE = base();
	// The app starts its engine on demand; here the orchestrator already booted it.
	if (cmd === 'engine_supported') return true;
	if (cmd === 'suwayomi_start') return null;
	if (cmd === 'suwayomi_status') {
		return { state: 'ready', version: 'v2.3.2243', lastError: null };
	}
	if (cmd === 'suwayomi_gql') {
		const res = await fetch(`${BASE}/api/graphql`, {
			method: 'POST',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify({ query: args.query, variables: args.variables }),
		});
		return await res.json(); // whole { data, errors }
	}
	if (cmd === 'suwayomi_image') {
		const res = await fetch(`${BASE}${args.path}`);
		if (!res.ok) throw new Error(`suwayomi_image ${args.path} -> HTTP ${res.status}`);
		return await res.arrayBuffer();
	}
	if (cmd === 'fetch_image') {
		const res = await fetch(args.url);
		if (!res.ok) throw new Error(`fetch_image ${args.url} -> HTTP ${res.status}`);
		return await res.arrayBuffer();
	}
	throw new Error(`tauri-core-stub: unhandled invoke(${cmd})`);
}
