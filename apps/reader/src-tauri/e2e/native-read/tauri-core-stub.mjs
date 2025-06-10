// Shim for `@tauri-apps/api/core`'s `invoke`, mapping the three Tauri commands the
// backends use onto a REAL booted Suwayomi engine over loopback HTTP. Node 26 `fetch`.
const PORT = process.env.SUWA_PORT || '4572';
const BASE = `http://127.0.0.1:${PORT}`;

export async function invoke(cmd, args) {
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
