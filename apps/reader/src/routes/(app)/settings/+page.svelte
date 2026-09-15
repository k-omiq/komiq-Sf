<script lang="ts">
	// The settings screen. Everything about the look and the chapter reader is kept on
	// this device and works signed out; the content filter is the account's own setting
	// on the server, so it needs a session.
	import { isTauri } from '@komika/api';
	import Icon from '$lib/components/Icon.svelte';
	import Avatar from '$lib/components/Avatar.svelte';
	import Dropdown from '$lib/components/Dropdown.svelte';
	import { auth, logout } from '$lib/auth.svelte';
	import { backend } from '$lib/context';
	import {
		canChooseFetch,
		canChooseServer,
		chosenServer,
		config,
		fetchOnDevice,
		saveFetchOnDevice,
	} from '$lib/config';
	import { fetchLocked, loadInstance } from '$lib/instance.svelte';
	import { connectScreen, engineSupported } from '$lib/server-connect.svelte';
	import {
		CUSTOM_PALETTE,
		PALETTES,
		resetTheme,
		setCustomColor,
		setPalette,
		setPureBlack,
		setTheme,
		theme,
		type ThemePref,
	} from '$lib/theme.svelte';
	import {
		BARS,
		DEFAULT_READER_PREFS,
		DIRS,
		FITS,
		HEADERS,
		MODES,
		WIDTH_MAX,
		WIDTH_MIN,
		WIDTH_STEP,
		loadReaderPrefs,
		saveReaderPrefs,
		type ReaderPrefs,
	} from '$lib/reader-prefs';
	import { clearPreferredTranslators, translatorPrefs } from '$lib/data/translator-pref.svelte';

	// --- Appearance -------------------------------------------------------------
	const THEME_MODES: { value: ThemePref; label: string }[] = [
		{ value: 'system', label: 'System' },
		{ value: 'light', label: 'Light' },
		{ value: 'dark', label: 'Dark' },
	];

	// --- Reading ----------------------------------------------------------------
	// The same object the chapter reader restores on open (see $lib/reader-prefs).
	let reader = $state<ReaderPrefs>(loadReaderPrefs());
	function setReader(patch: Partial<ReaderPrefs>): void {
		reader = { ...reader, ...patch };
		saveReaderPrefs(reader);
	}

	// Where chapters are fetched (see `fetchOnDevice` in $lib/config). Offered only in
	// the native app, and only where it has an engine. A server that does not fetch for
	// apps leaves the switch on and greyed out.
	let engineHere = $state(false);
	$effect(() => {
		if (!canChooseFetch) return;
		void loadInstance();
		void engineSupported().then((supported) => (engineHere = supported));
	});
	const fetchIsLocked = $derived(fetchLocked());
	function toggleFetchOnDevice(): void {
		if (fetchIsLocked) return;
		saveFetchOnDevice(!fetchOnDevice);
		// Every client is built once at load: restart on the new setting.
		location.reload();
	}

	// --- Content ----------------------------------------------------------------
	// NSFW visibility preference (CATALOGUE.md §2). Reflects the signed-in user's
	// server-side setting; the toggle persists via `setShowNsfw` and updates auth.
	const showNsfw = $derived(auth.user?.showNsfw ?? false);
	let savingNsfw = $state(false);
	let nsfwError = $state('');
	async function toggleNsfw(): Promise<void> {
		if (!auth.user || savingNsfw || !backend.setShowNsfw) return;
		savingNsfw = true;
		nsfwError = '';
		try {
			const next = await backend.setShowNsfw(!auth.user.showNsfw);
			if (auth.user) auth.user.showNsfw = next;
		} catch (err) {
			nsfwError = err instanceof Error ? err.message : 'Could not update the setting.';
		} finally {
			savingNsfw = false;
		}
	}

	// --- Server -----------------------------------------------------------------
	function origin(url: string): string {
		try {
			return new URL(url).origin;
		} catch {
			return url;
		}
	}
	const serverAddress = chosenServer ?? origin(config.apiEndpoint);

	// --- Account ----------------------------------------------------------------
	let signOutWarning = $state(false);
	async function signOut(): Promise<void> {
		const r = await logout();
		signOutWarning = !r.revoked;
	}

	// --- This device ------------------------------------------------------------
	// Each reset says what it did in the row itself, until the next one.
	let done = $state<'reader' | 'look' | 'translators' | null>(null);
	const translatorCount = $derived(Object.keys(translatorPrefs.map).length);
	function resetReader(): void {
		// The docked menu is a choice made inside the reader, not a default: keep it.
		setReader({ ...DEFAULT_READER_PREFS, pinned: reader.pinned });
		done = 'reader';
	}
	function resetLook(): void {
		resetTheme();
		done = 'look';
	}
	function resetTranslators(): void {
		clearPreferredTranslators();
		done = 'translators';
	}

	// --- About ------------------------------------------------------------------
	let appVersion = $state('');
	$effect(() => {
		if (!isTauri()) return;
		void import('@tauri-apps/api/app')
			.then(({ getVersion }) => getVersion())
			.then((v) => (appVersion = v))
			.catch(() => {});
	});
</script>

<svelte:head>
	<title>Settings · komiq</title>
</svelte:head>

{#snippet toggle(
	id: string,
	label: string,
	on: boolean,
	onclick: () => void,
	disabled: boolean = false,
)}
	<button
		type="button"
		{id}
		class="switch k-hit"
		class:on
		role="switch"
		aria-checked={on}
		aria-label={label}
		aria-describedby="{id}-desc"
		{disabled}
		{onclick}
	>
		<span class="knob"></span>
	</button>
{/snippet}

<div class="wrap k-gutter">
	<h1>Settings</h1>

	<section class="sec" id="appearance" aria-labelledby="appearance-title">
		<h2 id="appearance-title">Appearance</h2>
		<div class="group">
			<div class="row">
				<div class="text">
					<span class="label" id="mode-label">Theme</span>
					<span class="desc">System follows the light or dark setting of this device.</span>
				</div>
				<div class="seg" role="radiogroup" aria-labelledby="mode-label">
					{#each THEME_MODES as m (m.value)}
						<button
							type="button"
							role="radio"
							aria-checked={theme.pref === m.value}
							class:on={theme.pref === m.value}
							onclick={() => setTheme(m.value)}>{m.label}</button
						>
					{/each}
				</div>
			</div>
			<div class="stack">
				<div class="text">
					<span class="label" id="palette-label">Colour palette</span>
					<span class="desc">Changes the accent colour and tints the whole app to match.</span>
				</div>
				<div class="palettes" role="radiogroup" aria-labelledby="palette-label">
					{#each PALETTES as p (p.id)}
						<button
							type="button"
							role="radio"
							class="palette"
							aria-label={p.name}
							aria-checked={theme.palette === p.id}
							class:on={theme.palette === p.id}
							onclick={() => setPalette(p.id)}
						>
							<span class="swatch" style:background={p.seed}>
								{#if theme.palette === p.id}<Icon name="check" size={15} />{/if}
							</span>
							<span class="palette-name">{p.name}</span>
						</button>
					{/each}
					<!-- The reader's own colour. The native colour input covers the whole tile,
					     so a tap anywhere on it opens the picker. -->
					<label class="palette custom" class:on={theme.palette === CUSTOM_PALETTE}>
						<span class="swatch" style:background={theme.custom}>
							{#if theme.palette === CUSTOM_PALETTE}<Icon name="check" size={15} />
							{:else}<Icon name="plus" size={15} />{/if}
						</span>
						<span class="palette-name">Custom</span>
						<input
							type="color"
							aria-label="Custom colour"
							value={theme.custom}
							oninput={(e) => setCustomColor(e.currentTarget.value)}
							onclick={() => setCustomColor(theme.custom)}
						/>
					</label>
				</div>
			</div>
			<div class="row">
				<label class="text" for="black-switch">
					<span class="label">Pure black</span>
					<span class="desc" id="black-switch-desc">
						A true black background in the dark theme. It saves power on an OLED screen.
					</span>
				</label>
				{@render toggle('black-switch', 'Pure black', theme.pureBlack, () =>
					setPureBlack(!theme.pureBlack),
				)}
			</div>
		</div>
	</section>

	<section class="sec" id="reading" aria-labelledby="reading-title">
		<h2 id="reading-title">Reading</h2>
		<p class="sec-note">
			How a chapter opens. You can also change these from the menu inside the reader.
		</p>
		<div class="group">
			<div class="row">
				<div class="text">
					<span class="label">Reading mode</span>
					<span class="desc">A webtoon always opens as a long strip.</span>
				</div>
				<Dropdown
					label="Reading mode"
					align="right"
					value={reader.mode}
					options={MODES}
					onchange={(mode) => setReader({ mode })}
				/>
			</div>
			<div class="row">
				<div class="text">
					<span class="label">Page fit</span>
					<span class="desc">For single and double pages.</span>
				</div>
				<Dropdown
					label="Page fit"
					align="right"
					value={reader.fit}
					options={FITS}
					onchange={(fit) => setReader({ fit })}
				/>
			</div>
			<div class="row">
				<div class="text">
					<span class="label">Reading direction</span>
					<span class="desc">Right to left is the usual direction for manga.</span>
				</div>
				<Dropdown
					label="Reading direction"
					align="right"
					value={reader.dir}
					options={DIRS}
					onchange={(dir) => setReader({ dir })}
				/>
			</div>
			<div class="stack">
				<label class="text between" for="width-range">
					<span class="label">Long strip width</span>
					<span class="value">{reader.width}px</span>
				</label>
				<input
					id="width-range"
					type="range"
					min={WIDTH_MIN}
					max={WIDTH_MAX}
					step={WIDTH_STEP}
					value={reader.width}
					oninput={(e) => setReader({ width: e.currentTarget.valueAsNumber })}
				/>
			</div>
			<div class="row">
				<label class="text" for="gaps-switch">
					<span class="label">Page gaps</span>
					<span class="desc" id="gaps-switch-desc">Space between the pages of a long strip.</span>
				</label>
				{@render toggle('gaps-switch', 'Page gaps', reader.gaps, () =>
					setReader({ gaps: !reader.gaps }),
				)}
			</div>
			<div class="row">
				<div class="text">
					<span class="label">Reader header</span>
				</div>
				<Dropdown
					label="Reader header"
					align="right"
					value={reader.header}
					options={HEADERS}
					onchange={(header) => setReader({ header })}
				/>
			</div>
			<div class="row">
				<div class="text">
					<span class="label">Page progress</span>
				</div>
				<Dropdown
					label="Page progress"
					align="right"
					value={reader.bar}
					options={BARS}
					onchange={(bar) => setReader({ bar })}
				/>
			</div>
			{#if engineHere}
				<div class="row">
					<label class="text" for="fetch-switch">
						<span class="label">Fetch chapters on this device</span>
						<span class="desc" id="fetch-switch-desc">
							{#if fetchIsLocked}
								This app gets chapter pages from the sources itself. This server does not fetch
								chapters for apps.
							{:else}
								This app gets chapter pages from the sources itself. When it is off, the server gets
								them. The app restarts when you change this.
							{/if}
						</span>
					</label>
					{@render toggle(
						'fetch-switch',
						'Fetch chapters on this device',
						fetchOnDevice || fetchIsLocked,
						toggleFetchOnDevice,
						fetchIsLocked,
					)}
				</div>
			{/if}
		</div>
	</section>

	<section class="sec" id="content" aria-labelledby="content-title">
		<h2 id="content-title">Content</h2>
		<div class="group">
			<div class="row">
				<label class="text" for="nsfw-switch">
					<span class="label">Show NSFW content</span>
					<span class="desc" id="nsfw-switch-desc">
						Include adult-rated series in browse, search, and updates.
						{#if auth.ready && !auth.user}Sign in to change this.{/if}
					</span>
				</label>
				{@render toggle(
					'nsfw-switch',
					'Show NSFW content',
					showNsfw,
					toggleNsfw,
					!auth.user || savingNsfw || !backend.setShowNsfw,
				)}
			</div>
			{#if nsfwError}<p class="error" role="alert">{nsfwError}</p>{/if}
		</div>
	</section>

	{#if canChooseServer}
		<section class="sec" id="server" aria-labelledby="server-title">
			<h2 id="server-title">Server</h2>
			<div class="group">
				<div class="row">
					<div class="text">
						<span class="label">Connected to</span>
						<span class="desc address">{serverAddress}</span>
					</div>
					<button type="button" class="btn k-hit" onclick={() => (connectScreen.open = true)}>
						Change server
					</button>
				</div>
			</div>
		</section>
	{/if}

	<section class="sec" id="account" aria-labelledby="account-title">
		<h2 id="account-title">Account</h2>
		<div class="group">
			{#if auth.user}
				<div class="row">
					<div class="who">
						<Avatar
							url={auth.user.avatarUrl}
							name={auth.user.username}
							colorKey={auth.user.id}
							size={40}
						/>
						<div class="text">
							<span class="label">{auth.user.username}</span>
							<span class="desc">Signed in{auth.user.isAdmin ? ' · Admin' : ''}</span>
						</div>
					</div>
					<button type="button" class="btn danger k-hit" onclick={signOut}>Sign out</button>
				</div>
				<div class="links">
					<a class="link" href="/profile">
						<span>Profile, photo &amp; bio</span><Icon name="chevron-right" size={16} />
					</a>
					<a class="link" href="/library">
						<span>Your library &amp; shelves</span><Icon name="chevron-right" size={16} />
					</a>
				</div>
			{:else if auth.ready}
				<div class="row">
					<div class="text">
						<span class="label">Not signed in</span>
						<span class="desc">Sign in to keep a library and your reading progress.</span>
					</div>
					<a class="btn primary k-hit" href="/login?redirect=%2Fsettings">Sign in</a>
				</div>
			{/if}
			{#if signOutWarning}
				<p class="error" role="status">
					Signed out on this device, but the server could not be reached to revoke the session. It
					may still be valid there.
				</p>
			{/if}
		</div>
	</section>

	<section class="sec" id="device" aria-labelledby="device-title">
		<h2 id="device-title">This device</h2>
		<p class="sec-note">These choices are kept on this device only.</p>
		<div class="group">
			<div class="row">
				<div class="text">
					<span class="label">Reading options</span>
					<span class="desc" role="status">
						{done === 'reader'
							? 'Done. The reader is back to its defaults.'
							: 'Mode, fit, direction, width and bars.'}
					</span>
				</div>
				<button type="button" class="btn k-hit" onclick={resetReader}>Reset</button>
			</div>
			<div class="row">
				<div class="text">
					<span class="label">Appearance</span>
					<span class="desc" role="status">
						{done === 'look'
							? 'Done. The app is back to its own colours.'
							: 'Theme, colour palette and pure black.'}
					</span>
				</div>
				<button type="button" class="btn k-hit" onclick={resetLook}>Reset</button>
			</div>
			<div class="row">
				<div class="text">
					<span class="label">Translator choices</span>
					<span class="desc" role="status">
						{#if done === 'translators'}
							Done. Every series uses its default translator again.
						{:else if translatorCount === 0}
							You have not picked a translator for any series.
						{:else}
							You picked a translator for {translatorCount} series.
						{/if}
					</span>
				</div>
				<button
					type="button"
					class="btn k-hit"
					disabled={translatorCount === 0}
					onclick={resetTranslators}>Forget</button
				>
			</div>
		</div>
	</section>

	<section class="sec" id="about" aria-labelledby="about-title">
		<h2 id="about-title">About</h2>
		<div class="group">
			<div class="links">
				<a class="link" href="/about">
					<span>About komiq</span><Icon name="chevron-right" size={16} />
				</a>
				<a class="link" href="/support">
					<span>Support</span><Icon name="chevron-right" size={16} />
				</a>
			</div>
			{#if appVersion}
				<div class="row">
					<span class="label">Version</span>
					<span class="value">{appVersion}</span>
				</div>
			{/if}
		</div>
	</section>
</div>

<style>
	/* One narrow column: a label, then a soft-filled group of rows. No outlines; the
	   fill and the spacing do the separating. */
	.wrap {
		max-width: 720px;
		margin: 0 auto;
		padding-top: 44px;
		padding-bottom: 72px;
	}
	h1 {
		font-size: 34px;
		line-height: 1.1;
		margin: 0;
		color: var(--k-text-bright);
	}
	.sec {
		padding-top: 36px;
		/* An incoming `/settings#content` link lands below the sticky header. */
		scroll-margin-top: calc(var(--k-header-h) + 8px);
	}
	h2 {
		font-family: var(--k-font-sans);
		font-size: 11.5px;
		font-weight: 700;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--k-text-faint);
		margin: 0 0 10px;
		padding: 0 4px;
	}
	.sec-note {
		margin: -4px 0 12px;
		padding: 0 4px;
		font-size: 13px;
		line-height: 1.45;
		color: var(--k-text-dim);
	}
	.group {
		display: flex;
		flex-direction: column;
		gap: 22px;
		padding: 20px;
		border-radius: 16px;
		background: var(--k-surface-2);
	}
	.group:empty {
		display: none;
	}
	.row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 16px;
	}
	.stack {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.text {
		display: flex;
		flex-direction: column;
		gap: 3px;
		min-width: 0;
	}
	label.text {
		cursor: pointer;
	}
	.text.between {
		flex-direction: row;
		align-items: baseline;
		justify-content: space-between;
		gap: 12px;
	}
	.label {
		font-weight: 600;
		font-size: 14px;
		color: var(--k-text-1);
	}
	.desc {
		font-size: 12.5px;
		line-height: 1.4;
		color: var(--k-text-faint);
	}
	.desc.address {
		font-family: var(--k-font-mono);
		overflow-wrap: anywhere;
	}
	.value {
		font-size: 13px;
		font-variant-numeric: tabular-nums;
		color: var(--k-text-dim);
	}
	.error {
		margin: -10px 0 0;
		font-size: 12.5px;
		color: var(--k-danger);
	}

	/* ---- Segmented control (theme mode) ------------------------------------- */
	.seg {
		flex: 0 0 auto;
		display: flex;
		padding: 3px;
		border-radius: 10px;
		background: var(--k-surface-5);
	}
	.seg button {
		min-height: 34px;
		padding: 0 14px;
		border: none;
		border-radius: 8px;
		background: transparent;
		color: var(--k-text-dim);
		font-family: inherit;
		font-size: 13px;
		font-weight: 600;
		cursor: pointer;
		transition:
			background var(--k-dur) var(--k-ease),
			color var(--k-dur) var(--k-ease);
	}
	.seg button:hover {
		color: var(--k-text);
	}
	.seg button.on {
		background: var(--k-primary);
		color: var(--k-on-primary);
	}

	/* ---- Palette tiles ------------------------------------------------------ */
	.palettes {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(92px, 1fr));
		gap: 8px;
	}
	.palette {
		position: relative;
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 8px;
		padding: 12px 6px 10px;
		border: none;
		border-radius: 12px;
		background: transparent;
		color: var(--k-text-dim);
		font-family: inherit;
		cursor: pointer;
		transition: background var(--k-dur) var(--k-ease);
	}
	.palette:hover {
		background: var(--k-hover-fill);
	}
	.palette.on {
		background: var(--k-surface-5);
		color: var(--k-text);
	}
	.swatch {
		width: 36px;
		height: 36px;
		border-radius: 50%;
		display: flex;
		align-items: center;
		justify-content: center;
		color: #fff;
	}
	.palette-name {
		font-size: 12px;
		font-weight: 600;
		line-height: 1.2;
		text-align: center;
	}
	.palette.custom input {
		position: absolute;
		inset: 0;
		width: 100%;
		height: 100%;
		opacity: 0;
		cursor: pointer;
	}
	.palette.custom:focus-within {
		outline: 2px solid var(--k-focus-ring);
		outline-offset: 2px;
	}

	/* ---- Switch ------------------------------------------------------------- */
	.switch {
		flex: 0 0 auto;
		width: 48px;
		height: 28px;
		border-radius: 999px;
		border: none;
		background: var(--k-border-4);
		padding: 0;
		cursor: pointer;
		position: relative;
		transition: background var(--k-dur) var(--k-ease);
	}
	.switch:disabled {
		opacity: 0.5;
		cursor: default;
	}
	.switch.on {
		background: var(--k-primary);
	}
	.switch .knob {
		position: absolute;
		top: 3px;
		left: 3px;
		width: 22px;
		height: 22px;
		border-radius: 50%;
		background: var(--k-on-primary);
		transition: transform var(--k-dur) var(--k-ease);
	}
	.switch:not(.on) .knob {
		background: #fff;
	}
	.switch.on .knob {
		transform: translateX(20px);
	}

	/* ---- Range -------------------------------------------------------------- */
	input[type='range'] {
		width: 100%;
		accent-color: var(--k-primary);
	}

	/* ---- Buttons and link rows ---------------------------------------------- */
	.btn {
		flex: 0 0 auto;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		min-height: 38px;
		padding: 0 16px;
		border: none;
		border-radius: 10px;
		background: var(--k-surface-5);
		color: var(--k-text-1);
		font-family: inherit;
		font-size: 13.5px;
		font-weight: 700;
		text-decoration: none;
		white-space: nowrap;
		cursor: pointer;
		transition: background var(--k-dur) var(--k-ease);
	}
	.btn:hover:not(:disabled) {
		background: var(--k-border-4);
	}
	.btn:disabled {
		opacity: 0.5;
		cursor: default;
	}
	.btn.primary {
		background: var(--k-primary);
		color: var(--k-on-primary);
	}
	.btn.primary:hover {
		background: var(--k-primary-hover);
	}
	.btn.danger {
		background: var(--k-danger-soft);
		color: var(--k-danger);
	}
	.who {
		display: flex;
		align-items: center;
		gap: 12px;
		min-width: 0;
	}
	.links {
		display: flex;
		flex-direction: column;
		margin: -8px -10px;
	}
	.link {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		min-height: 44px;
		padding: 0 10px;
		border-radius: 10px;
		color: var(--k-text-2);
		font-size: 14px;
		font-weight: 600;
		text-decoration: none;
	}
	.link:hover {
		background: var(--k-hover-fill);
		color: var(--k-text);
	}
	.link :global(svg) {
		flex: 0 0 auto;
		color: var(--k-text-fainter);
	}

	@media (max-width: 640px) {
		.wrap {
			padding-top: 24px;
			padding-bottom: 40px;
		}
		h1 {
			font-size: 28px;
		}
		.sec {
			padding-top: 28px;
		}
		.group {
			padding: 16px;
			gap: 20px;
		}
		/* A wide control drops under its label instead of squeezing it. */
		.row:has(.seg),
		.row:has(:global(.dd)) {
			flex-direction: column;
			align-items: stretch;
			gap: 10px;
		}
		.seg button {
			flex: 1;
		}
		.palettes {
			grid-template-columns: repeat(auto-fill, minmax(76px, 1fr));
		}
	}
</style>
