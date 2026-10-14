<script lang="ts">
	// The settings screen. Everything about the look and the chapter reader is kept on
	// this device and works signed out; the content filter and the block rules are the
	// account's own settings on the server, so they need a session.
	import { onDestroy, tick } from 'svelte';
	import { isTauri, type BlockRule, type ContentRating } from '@komika/api';
	import type { TagInfo } from '@komika/types';
	import Icon from '$lib/components/Icon.svelte';
	import Avatar from '$lib/components/Avatar.svelte';
	import Dropdown from '$lib/components/Dropdown.svelte';
	import ColorPicker from '$lib/components/ColorPicker.svelte';
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
	import { fetchLocked, instance, loadInstance } from '$lib/instance.svelte';
	import { connectScreen, engineSupported } from '$lib/server-connect.svelte';
	import {
		KOMIQ_ACCENT,
		PRESETS,
		SWATCHES,
		activePreset,
		applyPreset,
		readability,
		resetTheme,
		resolvedTheme,
		setMdRatings,
		setPreview,
		setTheme,
		swatch,
		theme,
		updateLook,
		type Backdrop,
		type Look,
		type Mode as ThemeMode,
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
	import { getBrowseTags } from '$lib/data/source';
	import { shelveTags } from '$lib/data/browse-filters';
	import {
		RATINGS,
		RATING_LABEL,
		RULE_PROBLEM_TEXT,
		canonicalRule,
		ruleKey,
		ruleProblem,
		ruleText,
		sameRule,
		uniqueRules,
	} from '$lib/data/block-rules';
	import { blockRulesChanged } from '$lib/block-rules.svelte';

	// --- Appearance -------------------------------------------------------------
	const THEME_MODES: { value: ThemePref; label: string }[] = [
		{ value: 'system', label: 'System' },
		{ value: 'light', label: 'Light' },
		{ value: 'dark', label: 'Dark' },
	];
	const BACKDROPS: { value: Backdrop; label: string }[] = [
		{ value: 'none', label: 'None' },
		{ value: 'solid', label: 'Colour' },
		{ value: 'gradient', label: 'Gradient' },
	];
	const LOOK_MODES: { value: ThemeMode; label: string }[] = [
		{ value: 'light', label: 'Light' },
		{ value: 'dark', label: 'Dark' },
	];

	/** The theme whose look is being edited. The app shows it while this screen is
	 * open, even when the other one is in use. */
	let mode = $state<ThemeMode>(resolvedTheme());
	function editMode(m: ThemeMode): void {
		mode = m;
		editing = null;
		setPreview(m);
	}
	function chooseTheme(pref: ThemePref): void {
		setTheme(pref);
		editMode(resolvedTheme(pref));
	}
	onDestroy(() => setPreview(null));

	const look = $derived(theme.looks[mode]);
	const preset = $derived(activePreset(mode));
	const readable = $derived(readability(look, mode));
	/** The accent is the reader's own pick, not one of the quick picks. */
	const customAccent = $derived(
		look.accent !== null && !SWATCHES.some((s) => s.hex === look.accent),
	);
	const edit = (patch: Partial<Look>) => updateLook(mode, patch);

	/** Which colour the inline picker edits, if it is open. */
	type Target = 'a' | 'b' | 'accent';
	let editing = $state<Target | null>(null);
	function toggleEditing(target: Target): void {
		editing = editing === target ? null : target;
	}
	function pick(target: Target, hex: string): void {
		edit(target === 'accent' ? { accent: hex } : { [target]: hex });
	}
	function setBackdrop(backdrop: Backdrop): void {
		edit({ backdrop });
		if (backdrop === 'none' || (backdrop === 'solid' && editing === 'b')) {
			editing = editing === 'accent' ? 'accent' : null;
		}
	}

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

	// Blocked content: the viewer's own block rules (see $lib/data/block-rules). Kept on
	// the server like the NSFW setting, so they need a session. A change saves at once:
	// the whole list goes up and the stored list comes back.
	const canBlock = !!backend.myBlockRules && !!backend.setMyBlockRules;
	let rules = $state.raw<BlockRule[]>([]);
	// 'old' is a server that predates the rules. The block is left out there.
	let rulesState = $state<'idle' | 'loading' | 'ready' | 'failed' | 'old'>('idle');
	let rulesAttempt = $state(0);
	let savingRules = $state(false);
	let rulesError = $state('');
	$effect(() => {
		void rulesAttempt; // "Try again" runs this once more
		if (!canBlock || !auth.user?.id) {
			rules = [];
			rulesState = 'idle';
			return;
		}
		let cancelled = false;
		rulesState = 'loading';
		backend.myBlockRules!().then(
			(list) => {
				if (cancelled) return;
				rules = uniqueRules(list);
				rulesState = 'ready';
			},
			(err) => {
				if (cancelled) return;
				const msg = err instanceof Error ? err.message : String(err);
				rulesState = msg.includes('Unknown field "myBlockRules"') ? 'old' : 'failed';
			},
		);
		return () => {
			cancelled = true;
		};
	});

	// The tag vocabulary, for the picker and for the names in a rule's text. Only the
	// real tag list is offered: the legacy genre fallback has no stable ids. With no
	// list the form still makes rating-only rules, and a rule shows its tag ids.
	let tagList = $state.raw<TagInfo[]>([]);
	let tagsReady = $state(false);
	let tagsAsked = false;
	$effect(() => {
		if (!canBlock || !auth.user?.id || tagsAsked) return;
		tagsAsked = true;
		void getBrowseTags().then((r) => {
			if (r.source === 'tags') tagList = r.tags;
			tagsReady = true;
		});
	});
	const tagNames = $derived(new Map(tagList.map((t) => [t.id, t.name])));
	const tagName = (id: string) => tagNames.get(id);

	// The rule being put together.
	let draftTags = $state<string[]>([]);
	let draftRatings = $state<ContentRating[]>([]);
	let tagNeedle = $state('');
	let showAllTags = $state(false);
	let ruleAdded = $state(false);
	const draftProblem = $derived(ruleProblem({ tags: draftTags, ratings: draftRatings }, rules));
	const draftNote = $derived(
		draftProblem && draftProblem !== 'empty'
			? RULE_PROBLEM_TEXT[draftProblem]
			: ruleAdded
				? 'Rule added.'
				: '',
	);
	// Grouped and A to Z like Browse's picker. A picked tag stays in the list whatever
	// the search says, so the draft can always be seen and undone.
	const tagShelves = $derived(
		shelveTags(tagList, tagNeedle, { includedTags: draftTags, excludedTags: [] }),
	);
	const browsingTags = $derived(showAllTags || tagNeedle.trim() !== '');
	const pickedTags = $derived(tagList.filter((t) => draftTags.includes(t.id)));

	let rulesEl = $state<HTMLElement | null>(null);
	let addEl = $state<HTMLElement | null>(null);
	const focusAddForm = () => addEl?.querySelector<HTMLElement>('input, button')?.focus();

	async function toggleDraftTag(id: string, chip: HTMLElement): Promise<void> {
		ruleAdded = false;
		draftTags = draftTags.includes(id) ? draftTags.filter((t) => t !== id) : [...draftTags, id];
		// A tag taken out of the draft can leave the list, and the focus with it.
		await tick();
		if (!chip.isConnected) focusAddForm();
	}
	function toggleDraftRating(rating: ContentRating): void {
		ruleAdded = false;
		draftRatings = draftRatings.includes(rating)
			? draftRatings.filter((r) => r !== rating)
			: [...draftRatings, rating];
	}

	async function saveRules(next: BlockRule[]): Promise<boolean> {
		if (savingRules || !backend.setMyBlockRules) return false;
		savingRules = true;
		rulesError = '';
		try {
			rules = uniqueRules(await backend.setMyBlockRules(next));
			// Lists fetched under the old rules must not come back.
			blockRulesChanged();
			return true;
		} catch (err) {
			rulesError = err instanceof Error ? err.message : 'Could not save your rules.';
			return false;
		} finally {
			savingRules = false;
		}
	}
	async function addRule(): Promise<void> {
		if (draftProblem) return;
		const sent = { tags: draftTags, ratings: draftRatings };
		if (!(await saveRules([...rules, canonicalRule(sent)]))) return;
		// The form stays usable while the save runs. A draft that was changed in that
		// time is the start of the next rule, and is kept.
		if (sameRule(sent, { tags: draftTags, ratings: draftRatings })) {
			draftTags = [];
			draftRatings = [];
			tagNeedle = '';
		}
		ruleAdded = true;
		// The button is disabled again with an empty draft; keep the focus in the form.
		await tick();
		focusAddForm();
	}
	async function removeRule(index: number): Promise<void> {
		ruleAdded = false;
		if (!(await saveRules(rules.filter((_, i) => i !== index)))) return;
		// The pressed button is gone: go to the rule that took its place, else the form.
		await tick();
		const left = rulesEl?.querySelectorAll<HTMLElement>('.rule-remove');
		if (left?.length) left[Math.min(index, left.length - 1)].focus();
		else focusAddForm();
	}

	// MangaDex's rating next to our own readers' rating. Kept on this device (it is part
	// of what a card looks like, see $lib/theme.svelte) and works signed out. A server
	// whose admin turned them off sends none, so the switch has nothing to change there.
	$effect(() => {
		void loadInstance();
	});
	const mdRatingsOffHere = $derived(instance.info?.showMangadexRatings === false);

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

{#snippet well(target: 'a' | 'b', name: string, hex: string)}
	<button
		type="button"
		class="well"
		class:open={editing === target}
		aria-expanded={editing === target}
		aria-controls="picker-panel"
		onclick={() => toggleEditing(target)}
	>
		<span class="well-chip" style:background={hex}></span>
		<span class="well-text">
			<span class="well-name">{name}</span>
			<span class="well-hex">{hex.slice(1).toUpperCase()}</span>
		</span>
		<span class="well-caret"><Icon name="chevron-down" size={16} /></span>
	</button>
{/snippet}

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

<!-- A chip always has its icon, a plus or a check: the state does not rest on the
     colour, and a chip that is pressed keeps its width, so its neighbours stay put. -->
{#snippet chip(
	label: string,
	on: boolean,
	onclick: (e: MouseEvent & { currentTarget: HTMLButtonElement }) => void,
)}
	<button type="button" class="chip k-hit" class:on aria-pressed={on} {onclick}>
		<Icon name={on ? 'check' : 'plus'} size={13} strokeWidth={2.6} />
		<span>{label}</span>
	</button>
{/snippet}

{#snippet tagChip(tag: TagInfo)}
	{@render chip(tag.name, draftTags.includes(tag.id), (e) =>
		toggleDraftTag(tag.id, e.currentTarget),
	)}
{/snippet}

<div class="wrap k-gutter">
	<h1>Settings</h1>

	<section class="sec" id="appearance" aria-labelledby="appearance-title">
		<h2 id="appearance-title">Appearance</h2>
		<div class="group">
			<div class="row">
				<div class="text">
					<span class="label" id="mode-label">Theme</span>
					<span class="desc">Which look the app uses. System follows this device.</span>
				</div>
				<div class="seg" role="radiogroup" aria-labelledby="mode-label">
					{#each THEME_MODES as m (m.value)}
						<button
							type="button"
							role="radio"
							aria-checked={theme.pref === m.value}
							class:on={theme.pref === m.value}
							onclick={() => chooseTheme(m.value)}>{m.label}</button
						>
					{/each}
				</div>
			</div>

			<div class="stack">
				<div class="text">
					<span class="label" id="looks-label">Your looks</span>
					<span class="desc">
						Light and dark each have their own colours. Pick one to change it.
					</span>
				</div>
				<div class="looks" role="radiogroup" aria-labelledby="looks-label">
					{#each LOOK_MODES as m (m.value)}
						{@const sw = swatch(theme.looks[m.value], m.value)}
						{@const on = mode === m.value}
						<button
							type="button"
							role="radio"
							class="look-tile"
							class:on
							aria-checked={on}
							onclick={() => editMode(m.value)}
						>
							<span class="look-fill" style:background={sw.fill} style:color={sw.text}>
								<span class="look-line"></span>
								<span class="look-line short"></span>
								<span class="look-accent" style:background={sw.accent}></span>
							</span>
							<span class="look-name">
								{m.label}
								{#if resolvedTheme() === m.value}<span class="look-use">In use</span>{/if}
							</span>
						</button>
					{/each}
				</div>
			</div>

			<div class="editor">
				<div class="stack">
					<div class="text">
						<span class="label" id="preset-label">Presets</span>
						<span class="desc">A background and an accent that go together.</span>
					</div>
					<div class="presets" role="radiogroup" aria-labelledby="preset-label">
						{#each PRESETS[mode] as p (p.id)}
							{@const sw = swatch(p.look, mode)}
							{@const on = preset === p.id}
							<button
								type="button"
								role="radio"
								class="preset"
								aria-checked={on}
								class:on
								onclick={() => applyPreset(mode, p.id)}
							>
								<span class="preset-fill" style:background={sw.fill}>
									<span class="preset-accent" style:background={sw.accent}></span>
									{#if on}
										<span class="preset-check">
											<Icon name="check" size={12} strokeWidth={3} />
										</span>
									{/if}
								</span>
								<span class="preset-name">{p.name}</span>
							</button>
						{/each}
					</div>
				</div>

				<div class="stack">
					<div class="row">
						<div class="text">
							<span class="label" id="backdrop-label">Background</span>
							<span class="desc">
								{look.backdrop === 'none'
									? 'The theme’s own neutral colour.'
									: 'The page is exactly this colour. Cards and controls are shades of it.'}
							</span>
						</div>
						<div class="seg" role="radiogroup" aria-labelledby="backdrop-label">
							{#each BACKDROPS as b (b.value)}
								<button
									type="button"
									role="radio"
									aria-checked={look.backdrop === b.value}
									class:on={look.backdrop === b.value}
									onclick={() => setBackdrop(b.value)}>{b.label}</button
								>
							{/each}
						</div>
					</div>

					{#if look.backdrop !== 'none'}
						<div class="tint">
							<div class="wells">
								{@render well('a', look.backdrop === 'gradient' ? 'From' : 'Colour', look.a)}
								{#if look.backdrop === 'gradient'}
									<button
										type="button"
										class="swap k-hit"
										aria-label="Swap the two colours"
										title="Swap"
										onclick={() => edit({ a: look.b, b: look.a })}
									>
										<Icon name="arrow-left-right" size={16} />
									</button>
									{@render well('b', 'To', look.b)}
								{/if}
							</div>
							{#if editing === 'a' || (editing === 'b' && look.backdrop === 'gradient')}
								<div class="picker-panel" id="picker-panel">
									{#key editing}
										{@const target = editing}
										<ColorPicker
											value={target === 'a' ? look.a : look.b}
											label={target === 'a' ? 'Background colour' : 'Gradient end colour'}
											swatches={SWATCHES}
											onchange={(hex) => pick(target, hex)}
										/>
									{/key}
								</div>
							{/if}
							{#if look.backdrop === 'gradient'}
								<div class="slider">
									<label class="text between" for="angle-range">
										<span class="label sub">Direction</span>
										<span class="value">
											<span class="compass" style:rotate="{look.angle - 90}deg" aria-hidden="true">
												<Icon name="arrow-right" size={14} strokeWidth={2.4} />
											</span>
											{look.angle}°
										</span>
									</label>
									<input
										id="angle-range"
										type="range"
										min="0"
										max="359"
										step="1"
										value={look.angle}
										oninput={(e) => edit({ angle: e.currentTarget.valueAsNumber })}
									/>
								</div>
							{/if}
						</div>
					{/if}
					{#if readable.text < 4.5}
						<p class="hint" role="status">
							Some text is hard to read on this background. Try a lighter or darker colour.
						</p>
					{/if}
				</div>

				<div class="stack">
					<div class="text">
						<span class="label" id="accent-label">Accent</span>
						<span class="desc">Links, highlights and the main buttons.</span>
					</div>
					<div class="accents" role="radiogroup" aria-labelledby="accent-label">
						<button
							type="button"
							role="radio"
							class="accent-dot k-hit"
							style:background={KOMIQ_ACCENT}
							aria-checked={look.accent === null}
							aria-label="komiq (default)"
							title="komiq (default)"
							class:on={look.accent === null}
							onclick={() => edit({ accent: null })}
						>
							{#if look.accent === null}<Icon name="check" size={14} strokeWidth={3} />{/if}
						</button>
						<span class="accent-gap" aria-hidden="true"></span>
						{#each SWATCHES.slice(1) as s (s.hex)}
							{@const on = look.accent === s.hex}
							<button
								type="button"
								role="radio"
								class="accent-dot k-hit"
								style:background={s.hex}
								aria-checked={on}
								aria-label={s.name}
								title={s.name}
								class:on
								onclick={() => edit({ accent: s.hex })}
							>
								{#if on}<Icon name="check" size={14} strokeWidth={3} />{/if}
							</button>
						{/each}
						<!-- Opens the picker, so a disclosure button rather than a radio. -->
						<button
							type="button"
							class="accent-dot custom k-hit"
							class:on={customAccent}
							class:open={editing === 'accent'}
							style:--own={customAccent ? look.accent : null}
							aria-expanded={editing === 'accent'}
							aria-controls="accent-picker"
							aria-label={customAccent ? 'Your own colour (in use)' : 'Your own colour'}
							title="Your own colour"
							onclick={() => {
								toggleEditing('accent');
								if (!customAccent && editing === 'accent') {
									edit({ accent: look.accent ?? KOMIQ_ACCENT });
								}
							}}
						>
							{#if customAccent}<Icon name="check" size={14} strokeWidth={3} />
							{:else}<Icon name="plus" size={14} strokeWidth={2.6} />{/if}
						</button>
					</div>
					{#if readable.accent < 3}
						<p class="hint" role="status">
							This accent is hard to see on the background. Links and highlights may be hard to
							spot.
						</p>
					{/if}
					{#if editing === 'accent'}
						<div class="picker-panel" id="accent-picker">
							<ColorPicker
								value={look.accent ?? KOMIQ_ACCENT}
								label="Accent colour"
								onchange={(hex) => pick('accent', hex)}
							/>
						</div>
					{/if}
				</div>
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
			<div class="row">
				<label class="text" for="md-ratings-switch">
					<span class="label">Show MangaDex ratings</span>
					<span class="desc" id="md-ratings-switch-desc">
						{#if mdRatingsOffHere}
							This server does not show MangaDex ratings.
						{:else}
							The yellow star is the rating from readers here, and the orange star is the rating
							from MangaDex. When this is off, you see the yellow star only. This choice is kept on
							this device.
						{/if}
					</span>
				</label>
				{@render toggle(
					'md-ratings-switch',
					'Show MangaDex ratings',
					theme.mdRatings && !mdRatingsOffHere,
					() => setMdRatings(!theme.mdRatings),
					mdRatingsOffHere,
				)}
			</div>
			{#if canBlock && rulesState !== 'old'}
				<div class="stack blocked">
					<div class="text">
						<span class="label" id="block-label">Blocked content</span>
						<span class="desc">
							Hide series by tag, by content rating, or by both. A series is hidden when it matches
							any rule.
							{#if auth.ready && !auth.user}Sign in to change this.{/if}
						</span>
					</div>
					{#if rulesState === 'failed'}
						<div class="row">
							<p class="error" role="alert">Could not load your rules.</p>
							<button type="button" class="btn k-hit" onclick={() => (rulesAttempt += 1)}>
								Try again
							</button>
						</div>
					{:else if rulesState === 'ready' && tagsReady}
						{#if rules.length}
							<ul class="rules" aria-labelledby="block-label" bind:this={rulesEl}>
								{#each rules as rule, i (ruleKey(rule))}
									{@const text = ruleText(rule, tagName)}
									<li class="rule">
										<span class="rule-text">{text}</span>
										<!-- Not disabled while a save runs: that would drop the focus. A
										     second press is ignored instead. -->
										<button
											type="button"
											class="rule-remove k-hit"
											aria-label={`Remove rule: ${text}`}
											title="Remove rule"
											onclick={() => removeRule(i)}
										>
											<Icon name="x" size={15} />
										</button>
									</li>
								{/each}
							</ul>
						{/if}
						{#if rulesError}<p class="error" role="alert">{rulesError}</p>{/if}
						<div class="add" role="group" aria-label="Add rule" bind:this={addEl}>
							{#if tagList.length}
								<div class="field">
									<span class="field-label" id="block-tags-label">Tags</span>
									<input
										type="search"
										class="tag-search"
										placeholder="Search tags"
										aria-label="Search tags"
										aria-controls="block-tags"
										autocomplete="off"
										bind:value={tagNeedle}
									/>
									<div
										class="tag-list"
										id="block-tags"
										role="group"
										aria-labelledby="block-tags-label"
										aria-describedby="block-tags-hint"
									>
										{#if browsingTags}
											{#each tagShelves as shelf (shelf.group)}
												<div class="shelf" role="group" aria-label={shelf.label}>
													<span class="shelf-label">{shelf.label}</span>
													<div class="chips">
														{#each shelf.tags as tag (tag.id)}{@render tagChip(tag)}{/each}
													</div>
												</div>
											{:else}
												<p class="desc">No tag matches this search.</p>
											{/each}
										{:else if pickedTags.length}
											<div class="chips">
												{#each pickedTags as tag (tag.id)}{@render tagChip(tag)}{/each}
											</div>
										{/if}
									</div>
									{#if !tagNeedle.trim()}
										<button
											type="button"
											class="more k-hit"
											aria-expanded={showAllTags}
											aria-controls="block-tags"
											onclick={() => (showAllTags = !showAllTags)}
										>
											{showAllTags ? 'Show fewer tags' : `Show all ${tagList.length} tags`}
											<Icon name="chevron-down" size={13} />
										</button>
									{/if}
									<span class="desc" id="block-tags-hint"
										>A series must have all of these tags.</span
									>
								</div>
							{/if}
							<div class="field">
								<span class="field-label" id="block-ratings-label">Content rating</span>
								<div
									class="chips"
									role="group"
									aria-labelledby="block-ratings-label"
									aria-describedby="block-ratings-hint"
								>
									{#each RATINGS as rating (rating)}
										{@render chip(RATING_LABEL[rating], draftRatings.includes(rating), () =>
											toggleDraftRating(rating),
										)}
									{/each}
								</div>
								<span class="desc" id="block-ratings-hint">Leave empty for any rating.</span>
							</div>
							<div class="add-foot">
								<button
									type="button"
									class="btn k-hit"
									disabled={!!draftProblem}
									aria-describedby="block-note"
									onclick={addRule}>Add rule</button
								>
								<span class="desc" id="block-note" role="status">{draftNote}</span>
							</div>
						</div>
					{/if}
				</div>
			{/if}
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
							: 'Theme, and the colours of your light and dark looks.'}
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

	/* ---- Your looks ---------------------------------------------------------- */
	.looks {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 8px;
	}
	.look-tile {
		display: flex;
		flex-direction: column;
		gap: 9px;
		padding: 6px 6px 10px;
		border: none;
		border-radius: 16px;
		background: transparent;
		color: var(--k-text-dim);
		font-family: inherit;
		text-align: left;
		cursor: pointer;
		transition:
			background var(--k-dur) var(--k-ease),
			color var(--k-dur) var(--k-ease);
	}
	.look-tile:hover {
		background: var(--k-hover-fill);
		color: var(--k-text-1);
	}
	.look-tile.on {
		background: var(--k-surface-5);
		color: var(--k-text);
	}
	/* A small page in that look: two lines of its text and its accent. */
	.look-fill {
		position: relative;
		display: flex;
		flex-direction: column;
		gap: 7px;
		height: 78px;
		padding: 16px 14px;
		border-radius: 12px;
		box-shadow: 0 1px 3px rgb(0 0 0 / 0.18);
	}
	.look-line {
		width: 58%;
		height: 6px;
		border-radius: 999px;
		background: currentColor;
		opacity: 0.8;
	}
	.look-line.short {
		width: 36%;
		opacity: 0.45;
	}
	.look-accent {
		position: absolute;
		right: 12px;
		bottom: 12px;
		width: 34px;
		height: 14px;
		border-radius: 999px;
	}
	.look-name {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 0 4px;
		font-size: 13.5px;
		font-weight: 600;
	}
	.look-use {
		padding: 2px 8px;
		border-radius: 999px;
		background: var(--k-surface-3);
		color: var(--k-text-dim);
		font-size: 11px;
		font-weight: 700;
	}
	/* The controls for the look picked above. */
	.editor {
		display: flex;
		flex-direction: column;
		gap: 22px;
	}
	.hint {
		margin: 0;
		font-size: 12.5px;
		line-height: 1.45;
		color: var(--k-warning);
	}

	/* ---- Presets ------------------------------------------------------------ */
	.presets {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(104px, 1fr));
		gap: 6px;
	}
	.preset {
		display: flex;
		flex-direction: column;
		gap: 8px;
		padding: 6px 6px 9px;
		border: none;
		border-radius: 14px;
		background: transparent;
		color: var(--k-text-dim);
		font-family: inherit;
		text-align: left;
		cursor: pointer;
		transition:
			background var(--k-dur) var(--k-ease),
			color var(--k-dur) var(--k-ease);
	}
	.preset:hover {
		background: var(--k-hover-fill);
		color: var(--k-text-1);
	}
	.preset.on {
		background: var(--k-surface-5);
		color: var(--k-text);
	}
	.preset-fill {
		position: relative;
		height: 52px;
		border-radius: 10px;
		transition: transform 220ms cubic-bezier(0.16, 1, 0.3, 1);
	}
	.preset:active .preset-fill {
		transform: scale(0.97);
	}
	.preset-accent {
		position: absolute;
		left: 9px;
		bottom: 9px;
		width: 22px;
		height: 8px;
		border-radius: 999px;
	}
	.preset-check {
		position: absolute;
		top: 7px;
		right: 7px;
		width: 20px;
		height: 20px;
		border-radius: 50%;
		display: grid;
		place-items: center;
		background: var(--k-primary);
		color: var(--k-on-primary);
	}
	.preset-name {
		padding: 0 4px;
		font-size: 12.5px;
		font-weight: 600;
		line-height: 1.2;
		overflow-wrap: anywhere;
	}

	/* ---- Background controls ------------------------------------------------- */
	.tint {
		display: flex;
		flex-direction: column;
		gap: 16px;
		padding: 16px;
		border-radius: 14px;
		background: var(--k-surface-3);
	}
	.wells {
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.well {
		flex: 1 1 0;
		min-width: 0;
		display: flex;
		align-items: center;
		gap: 10px;
		min-height: 52px;
		padding: 8px 10px 8px 8px;
		border: none;
		border-radius: 12px;
		background: var(--k-surface-5);
		color: var(--k-text-1);
		font-family: inherit;
		text-align: left;
		cursor: pointer;
		transition: background var(--k-dur) var(--k-ease);
	}
	.well:hover {
		background: var(--k-border-4);
	}
	.well-chip {
		flex: 0 0 auto;
		width: 36px;
		height: 36px;
		border-radius: 9px;
	}
	.well-text {
		flex: 1 1 auto;
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 1px;
	}
	.well-name {
		font-size: 13px;
		font-weight: 600;
	}
	.well-hex {
		font-family: var(--k-font-mono);
		font-size: 12px;
		color: var(--k-text-faint);
	}
	.well-caret {
		display: flex;
		color: var(--k-text-faint);
		transition: transform 220ms cubic-bezier(0.16, 1, 0.3, 1);
	}
	.well.open .well-caret {
		transform: rotate(180deg);
	}
	.swap {
		flex: 0 0 auto;
		width: 36px;
		height: 36px;
		border: none;
		border-radius: 50%;
		display: grid;
		place-items: center;
		background: transparent;
		color: var(--k-text-dim);
		cursor: pointer;
		transition:
			background var(--k-dur) var(--k-ease),
			color var(--k-dur) var(--k-ease);
	}
	.swap:hover {
		background: var(--k-hover-fill);
		color: var(--k-text);
	}
	.picker-panel {
		padding: 14px;
		border-radius: 14px;
		background: var(--k-surface-4);
		animation: panel-in 260ms cubic-bezier(0.16, 1, 0.3, 1);
	}
	@keyframes panel-in {
		from {
			opacity: 0;
			transform: translateY(-4px);
		}
	}
	.slider {
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.label.sub {
		font-size: 13px;
	}
	.value {
		display: inline-flex;
		align-items: center;
		gap: 6px;
	}
	.compass {
		display: inline-flex;
		color: var(--k-text-faint);
	}

	/* ---- Accent ---------------------------------------------------------------- */
	.accents {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 10px;
	}
	.accent-gap {
		width: 1px;
		height: 20px;
		margin: 0 2px;
		background: var(--k-border-2);
	}
	.accent-dot {
		width: 32px;
		height: 32px;
		padding: 0;
		border: none;
		border-radius: 50%;
		display: grid;
		place-items: center;
		color: #fff;
		cursor: pointer;
		transition:
			transform 220ms cubic-bezier(0.16, 1, 0.3, 1),
			box-shadow var(--k-dur) var(--k-ease);
	}
	.accent-dot:hover {
		transform: scale(1.1);
	}
	/* The chosen one gets a ring in the page colour, then one in the text colour, so
	   it shows on any swatch. */
	.accent-dot.on {
		box-shadow:
			0 0 0 2px var(--k-surface-2),
			0 0 0 4px var(--k-text-2);
	}
	.accent-dot :global(svg) {
		filter: drop-shadow(0 1px 1px rgb(0 0 0 / 0.4));
	}
	/* "Your own colour": a colour wheel until there is one, then that colour. */
	.accent-dot.custom {
		background: conic-gradient(
			from 90deg,
			hsl(0 85% 60%),
			hsl(60 85% 55%),
			hsl(120 70% 50%),
			hsl(180 75% 50%),
			hsl(240 80% 62%),
			hsl(300 75% 60%),
			hsl(360 85% 60%)
		);
	}
	.accent-dot.custom.on {
		background: var(--own);
	}
	.accent-dot.custom.open:not(.on) {
		box-shadow:
			0 0 0 2px var(--k-surface-2),
			0 0 0 4px var(--k-border-4);
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

	/* ---- Blocked content ---------------------------------------------------- */
	/* Soft fills and spacing only, like the rest of the page: no outlines. */
	.blocked .error {
		margin: 0;
	}
	.rules {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.rule {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 10px;
		min-height: 44px;
		padding: 4px 4px 4px 14px;
		border-radius: 10px;
		background: var(--k-surface-4);
	}
	.rule-text {
		min-width: 0;
		font-size: 13.5px;
		font-weight: 600;
		line-height: 1.35;
		color: var(--k-text-1);
		overflow-wrap: anywhere;
	}
	.rule-remove {
		flex: 0 0 auto;
		width: 36px;
		height: 36px;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		border: none;
		border-radius: 8px;
		background: transparent;
		color: var(--k-text-dim);
		cursor: pointer;
		transition:
			background var(--k-dur) var(--k-ease),
			color var(--k-dur) var(--k-ease);
	}
	.rule-remove:hover {
		background: var(--k-hover-fill);
		color: var(--k-text);
	}
	.add {
		display: flex;
		flex-direction: column;
		gap: 16px;
		margin-top: 4px;
	}
	.field {
		display: flex;
		flex-direction: column;
		gap: 8px;
		min-width: 0;
	}
	.field-label {
		font-size: 12.5px;
		font-weight: 700;
		color: var(--k-text-2);
	}
	.tag-search {
		width: 100%;
		min-height: 38px;
		padding: 0 12px;
		border: none;
		border-radius: 10px;
		background: var(--k-surface-5);
		color: var(--k-text);
		font-family: inherit;
		font-size: 13.5px;
	}
	.tag-search::placeholder {
		color: var(--k-text-dim);
		opacity: 1;
	}
	.tag-list {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.tag-list:empty {
		display: none;
	}
	.shelf {
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.shelf-label {
		font-size: 11.5px;
		font-weight: 600;
		color: var(--k-text-faint);
	}
	.chips {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.chip {
		display: inline-flex;
		align-items: center;
		gap: 5px;
		min-height: 34px;
		padding: 0 13px 0 10px;
		border: none;
		border-radius: 999px;
		background: var(--k-surface-5);
		color: var(--k-text-2);
		font-family: inherit;
		font-size: 13px;
		font-weight: 600;
		text-align: left;
		cursor: pointer;
		transition:
			background var(--k-dur) var(--k-ease),
			color var(--k-dur) var(--k-ease);
	}
	.chip :global(svg) {
		flex: 0 0 auto;
	}
	.chip:hover {
		background: var(--k-border-4);
		color: var(--k-text);
	}
	.chip.on {
		background: var(--k-primary);
		color: var(--k-on-primary);
	}
	.chip.on:hover {
		background: var(--k-primary-hover);
	}
	.more {
		align-self: flex-start;
		display: inline-flex;
		align-items: center;
		gap: 6px;
		min-height: 28px;
		padding: 0;
		border: none;
		background: transparent;
		color: var(--k-text-dim);
		font-family: inherit;
		font-size: 12.5px;
		font-weight: 600;
		cursor: pointer;
	}
	.more:hover {
		color: var(--k-text);
	}
	.more[aria-expanded='true'] :global(svg) {
		transform: rotate(180deg);
	}
	.add-foot {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px 12px;
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
		.presets {
			grid-template-columns: repeat(auto-fill, minmax(92px, 1fr));
		}
		.row:has(.seg) .seg {
			width: 100%;
		}
		.tint {
			padding: 12px;
		}
		.picker-panel {
			padding: 12px;
		}
		/* Seven to a row on a phone: the default and six picks, then the rest. */
		.accents {
			gap: 12px 10px;
		}
		.accent-gap {
			display: none;
		}
		.accent-dot {
			width: 34px;
			height: 34px;
		}
		/* Finger-sized chips, and a search field iOS does not zoom into. */
		.chip {
			min-height: 40px;
		}
		.tag-search {
			min-height: 44px;
			font-size: 16px;
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.picker-panel {
			animation: none;
		}
		.preset-fill,
		.accent-dot,
		.well-caret {
			transition: none;
		}
	}
</style>
