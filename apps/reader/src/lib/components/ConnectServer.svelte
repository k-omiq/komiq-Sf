<script lang="ts">
	import { onMount } from 'svelte';
	import { chosenServer, needsServer } from '$lib/config';
	import {
		checkServer,
		connectScreen,
		engineSupported,
		serverOrigins,
		toServerOrigin,
		useServer,
	} from '$lib/server-connect.svelte';
	import AccountScreen from './AccountScreen.svelte';

	// Native app only: point the reader at a komiq server. The address is checked
	// (`/health`, then the server's own facts) before it is saved on this device; the
	// app then restarts on the sign-in page of that server, or on its "not set up" page.
	//
	// Where the app can fetch chapters itself (desktop), a second step asks who fetches
	// them: this device or the server. A server that does not fetch for apps
	// (`appsFetchThemselves`, the shared public server) leaves "The server" greyed out.
	//
	// On a phone the address can also be scanned from the QR code the admin shows
	// (setup, and Settings › Access). The scanner is the native barcode plugin, which
	// only the phone builds carry: everywhere else its first call fails and the
	// "Scan a QR code" button never appears.
	let address = $state(chosenServer ?? '');
	let busy = $state(false);
	let error = $state<string | null>(null);

	/** Set once the address checked out and the "who fetches" step is showing. */
	let fetchStep = $state<{ origin: string; locked: boolean } | null>(null);
	let onDevice = $state(false);

	// First run has nothing to go back to; "Change server" from sign-in does.
	const canCancel = !needsServer;

	type Scanner = typeof import('@tauri-apps/plugin-barcode-scanner');
	let scanner = $state<Scanner | null>(null);
	let scanning = $state(false);
	// The scan this screen is waiting on. Bumping it abandons that scan: whatever its
	// promise does later (or never does) is ignored.
	let scanRun = 0;
	let starting = false;
	/** The address in the field came from a QR code and has not been edited since. */
	let scanned = $state(false);

	onMount(() => {
		let gone = false;
		void (async () => {
			try {
				const plugin = await import('@tauri-apps/plugin-barcode-scanner');
				// Answers only where the plugin is registered (a phone).
				await plugin.checkPermissions();
				if (!gone) scanner = plugin;
			} catch {
				// Desktop app: no scanner. The address field is the whole screen.
			}
		})();
		return () => {
			gone = true;
			stopScan();
		};
	});

	async function connect(raw: string): Promise<void> {
		if (busy) return;
		error = null;
		const origins = serverOrigins(raw);
		if (!origins.length) {
			error = 'Enter an address like 192.168.1.20:8080 or komiq.example.com.';
			return;
		}
		busy = true;
		try {
			// The first address that answers as a komiq server; else the last one's reason.
			let checked: Awaited<ReturnType<typeof checkServer>> | undefined;
			let failure: unknown;
			for (const candidate of origins) {
				try {
					checked = await checkServer(candidate);
					break;
				} catch (err) {
					failure = err;
				}
			}
			if (!checked) throw failure;
			const { origin, info } = checked;
			if (!(await engineSupported())) {
				useServer(origin);
				return;
			}
			const locked = info.appsFetchThemselves === true;
			onDevice = locked;
			fetchStep = { origin, locked };
			busy = false;
		} catch (err) {
			error = err instanceof Error ? err.message : 'The server could not be checked.';
			busy = false;
		}
	}

	function submit(e: SubmitEvent): void {
		e.preventDefault();
		void connect(address);
	}

	/** The camera shows BEHIND the page, so the page goes see-through while scanning. */
	function setSeeThrough(on: boolean): void {
		document.documentElement.classList.toggle('komiq-scanning', on);
	}

	/**
	 * Leave the scan (Cancel, or the screen closing). The page is put back HERE, not
	 * when `scan()` settles: Android's `cancel()` drops the pending call without
	 * rejecting it, so that promise may never settle.
	 */
	function stopScan(): void {
		const live = scanning;
		scanRun++;
		starting = false;
		scanning = false;
		setSeeThrough(false);
		if (live) void scanner?.cancel().catch(() => {});
	}

	async function startScan(): Promise<void> {
		if (!scanner || starting || scanning || busy) return;
		starting = true;
		error = null;
		const run = ++scanRun;
		let allowed = false;
		let failed = false;
		try {
			let permission = await scanner.checkPermissions();
			if (permission !== 'granted' && permission !== 'denied') {
				permission = await scanner.requestPermissions();
			}
			allowed = permission === 'granted';
		} catch {
			failed = true;
		}
		// The screen closed, or was cancelled, while the permission was being asked.
		if (run !== scanRun) return;
		starting = false;
		if (!allowed) {
			error = failed
				? 'The camera could not be opened. Type the address instead.'
				: 'komiq is not allowed to use the camera. Allow it in the device settings, or type the address.';
			return;
		}
		scanning = true;
		setSeeThrough(true);
		let content: string | null = null;
		try {
			content = (await scanner.scan({ windowed: true, formats: [scanner.Format.QRCode] })).content;
		} catch {
			// The camera stopped: back to the form as it was.
		}
		if (run !== scanRun) return; // cancelled: `stopScan` already put the page back
		scanning = false;
		setSeeThrough(false);
		if (content === null) return;
		const origin = toServerOrigin(content);
		if (!origin) {
			error = 'That QR code is not a server address. Scan the one komiq shows in the admin.';
			return;
		}
		// Shown, not used yet: a QR code can point anywhere, and connecting signs this
		// device out of its current server. The person reads the address and taps Connect.
		// A code without `http(s)://` stays as it is, so Connect picks HTTPS for it the
		// way it does for a typed address.
		const text = content.trim();
		address = /^[a-z][a-z0-9+.-]*:\/\//i.test(text) ? origin : text.split(/[/?#]/)[0];
		scanned = true;
	}
</script>

<svelte:head><title>Connect to a server · komiq</title></svelte:head>

{#if scanning}
	<div class="scan" role="dialog" aria-modal="true" aria-label="Scan a QR code">
		<p class="scan-lead">Point the camera at the QR code komiq shows in the admin.</p>
		<div class="scan-frame" aria-hidden="true"></div>
		<button class="scan-cancel" type="button" onclick={stopScan}>Cancel</button>
	</div>
{:else if fetchStep}
	{@const step = fetchStep}
	<AccountScreen
		title="Who gets the chapters?"
		lead="Chapter pages come from the sources. Choose what fetches them."
	>
		<form
			onsubmit={(e) => {
				e.preventDefault();
				useServer(step.origin, onDevice);
			}}
		>
			<div class="choices" role="radiogroup" aria-label="Who gets the chapters">
				<button
					type="button"
					class="choice"
					role="radio"
					aria-checked={onDevice}
					onclick={() => (onDevice = true)}
				>
					<span class="choice-name">This device</span>
					<span class="choice-desc">This app gets the pages from the sources itself.</span>
				</button>
				<button
					type="button"
					class="choice"
					role="radio"
					aria-checked={!onDevice}
					aria-describedby={step.locked ? 'fetch-locked' : undefined}
					disabled={step.locked}
					onclick={() => (onDevice = false)}
				>
					<span class="choice-name">The server</span>
					<span class="choice-desc">The server gets the pages and sends them to this app.</span>
				</button>
			</div>
			{#if step.locked}
				<small id="fetch-locked" class="hint">This server does not fetch chapters for apps.</small>
			{:else}
				<small class="hint">You can change this later in Settings.</small>
			{/if}
			<button class="submit" type="submit">Continue</button>
		</form>
		{#snippet foot()}
			<button class="link" type="button" onclick={() => (fetchStep = null)}>Back</button>
		{/snippet}
	</AccountScreen>
{:else}
	<AccountScreen
		title="Connect to a server"
		lead="Enter the address of the komiq server you read on. You sign in next."
	>
		<form onsubmit={submit} novalidate aria-busy={busy}>
			<label>
				<span>Server address</span>
				<input
					type="text"
					name="server"
					inputmode="url"
					autocapitalize="none"
					autocomplete="url"
					spellcheck="false"
					placeholder="192.168.1.20:8080"
					bind:value={address}
					oninput={() => {
						error = null;
						scanned = false;
					}}
					aria-invalid={error ? true : undefined}
					aria-describedby="server-msg"
				/>
				{#if error}
					<span id="server-msg" class="field-error" role="alert">{error}</span>
				{:else if scanned}
					<small id="server-msg" class="hint" role="status"
						>Scanned. Check that this is your server, then tap Connect.</small
					>
				{:else}
					<small id="server-msg" class="hint"
						>The address you open komiq at in a browser. At home it is usually an IP address and a
						port.</small
					>
				{/if}
			</label>
			<button class="submit" type="submit" disabled={busy}>
				{busy ? 'Checking…' : 'Connect'}
			</button>
			{#if scanner}
				<button class="scan-open" type="button" disabled={busy} onclick={startScan}>
					Scan a QR code
				</button>
			{/if}
		</form>
		{#snippet foot()}
			{#if canCancel}
				<button class="link" type="button" onclick={() => (connectScreen.open = false)}
					>Cancel</button
				>
			{/if}
		{/snippet}
	</AccountScreen>
{/if}

<style>
	.hint {
		color: var(--k-text-dim);
		font-size: 12.5px;
		line-height: 1.45;
	}
	.choices {
		display: flex;
		flex-direction: column;
		gap: var(--k-space-2);
	}
	.choice {
		display: flex;
		flex-direction: column;
		gap: 2px;
		padding: var(--k-space-3) var(--k-space-4);
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius-md);
		background: transparent;
		color: var(--k-text);
		font-family: var(--k-font-sans);
		text-align: left;
		cursor: pointer;
	}
	.choice[aria-checked='true'] {
		border-color: var(--k-accent);
		background: var(--k-surface);
	}
	.choice:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	.choice-name {
		font-size: 14px;
		font-weight: 600;
	}
	.choice-desc {
		color: var(--k-text-dim);
		font-size: 12.5px;
		line-height: 1.45;
	}
	.scan-open {
		height: 44px;
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius-md);
		background: transparent;
		color: var(--k-text);
		font-family: var(--k-font-sans);
		font-size: 14px;
		font-weight: 600;
		cursor: pointer;
	}
	.scan-open:disabled {
		opacity: 0.6;
		cursor: default;
	}

	/* The native camera view sits behind the WebView while a scan runs. */
	:global(html.komiq-scanning),
	:global(html.komiq-scanning body) {
		background: transparent !important;
	}
	.scan {
		position: fixed;
		inset: 0;
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: space-between;
		gap: var(--k-space-6);
		padding: calc(env(safe-area-inset-top) + var(--k-space-8)) var(--k-space-5)
			calc(env(safe-area-inset-bottom) + var(--k-space-8));
	}
	.scan-lead {
		max-width: 320px;
		padding: var(--k-space-3) var(--k-space-4);
		border-radius: var(--k-radius-md);
		background: rgb(0 0 0 / 0.6);
		color: #fff;
		font-size: 14px;
		line-height: 1.45;
		text-align: center;
	}
	.scan-frame {
		width: min(68vw, 280px);
		aspect-ratio: 1;
		border: 2px solid rgb(255 255 255 / 0.9);
		border-radius: var(--k-radius-xl);
		box-shadow: 0 0 0 100vmax rgb(0 0 0 / 0.35);
	}
	.scan-cancel {
		min-width: 160px;
		height: 48px;
		border: 0;
		border-radius: var(--k-radius-md);
		background: rgb(0 0 0 / 0.7);
		color: #fff;
		font-family: var(--k-font-sans);
		font-size: 15px;
		font-weight: 600;
		cursor: pointer;
	}
</style>
