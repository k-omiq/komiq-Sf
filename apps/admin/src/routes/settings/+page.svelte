<script lang="ts">
	import { tick } from 'svelte';
	import { href } from '$lib/paths';
	import type { ExtensionAutoUpdate, ExtensionInfo, SourceInfo, TagInfo } from '@komika/types';
	import type {
		BlockRule,
		ContentRating,
		ExtensionRepo,
		MangadexSetting,
		SourceRateLimit,
	} from '@komika/api';
	import { auth, toggleNsfwVisibility } from '$lib/auth.svelte';
	import {
		MAX_RULE_TAGS,
		RATINGS,
		RATING_LABEL,
		checkDraft,
		ruleText,
		sameRule,
		tagShelves,
		tidyRule,
	} from '$lib/block-rules';
	import { config, serverAddress, target } from '$lib/config';
	import { confirm } from '$lib/confirm.svelte';
	import {
		addExtensionRepo,
		isEngineUnreachable,
		loadExtensionRepos,
		loadMangadexSetting,
		loadSiteBlockRules,
		loadSourceRateLimits,
		loadSources,
		loadTags,
		removeExtensionRepo,
		removeSourceRateLimit,
		setMangadexEnabled,
		setShowMangadexRatings,
		setSiteBlockRules,
		setSourceRateLimit,
		consolidateDuplicates,
		loadExtensionAutoUpdate,
		loadExtensions,
		materializeCatalogueCovers,
		persistCatalogue,
		runExtensionUpdates,
		setRegistrationOpen,
	} from '$lib/data';
	import { instance, loadInstance } from '$lib/instance.svelte';
	import { setTheme, theme, type ThemePref } from '$lib/theme.svelte';
	import { toast } from '$lib/toast.svelte';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Segmented from '$lib/components/Segmented.svelte';
	import Switch from '$lib/components/Switch.svelte';
	import AccessSettings from './AccessSettings.svelte';

	/**
	 * The block rules: `loading` until the first answer. `failed`: that answer was an
	 * error, and the section offers to ask again. `off`: an older server, so the Blocked
	 * content section is hidden. Declared here because the section links read it.
	 */
	let rulesState = $state<'loading' | 'ready' | 'failed' | 'off'>('loading');

	const sections = $derived([
		{ id: 'appearance', label: 'Appearance' },
		{ id: 'content', label: 'Content' },
		// Not on a server that predates block rules.
		...(rulesState !== 'off' ? [{ id: 'blocked', label: 'Blocked content' }] : []),
		// Only on a server that reports its settings (`instance`).
		...(instance.info
			? [
					{ id: 'access', label: 'Access' },
					{ id: 'accounts', label: 'Accounts' },
				]
			: []),
		{ id: 'sources', label: 'Sources' },
		{ id: 'updates', label: 'Extension updates' },
		{ id: 'maintenance', label: 'Maintenance' },
		{ id: 'about', label: 'About' },
	]);

	function shortDate(iso: string | null): string {
		if (!iso) return 'Never';
		const d = new Date(iso);
		return Number.isNaN(d.getTime()) ? 'Never' : d.toISOString().slice(0, 16).replace('T', ' ');
	}

	// ---- Content: the admin's own show_nsfw flag ------------------------------
	// An ACCOUNT preference, not a console filter: with it off the server refuses
	// source browsing, extension install and source ingest for this account. The
	// console's own lists (Catalog, Latest updates) read with a per-request
	// `includeNsfw` override, so mis-flagged works stay reachable either way.
	let nsfwBusy = $state(false);

	async function onToggleNsfw(next: boolean): Promise<void> {
		if (!auth.user || nsfwBusy) return;
		nsfwBusy = true;
		try {
			await toggleNsfwVisibility(next);
			toast.success(next ? 'NSFW content is on.' : 'NSFW content is off.');
		} catch (err) {
			toast.fromError(err, 'Failed to change NSFW visibility.');
		} finally {
			nsfwBusy = false;
		}
	}

	// ---- Blocked content: the site-wide block rules -------------------------------
	// A series that matches a rule is left out of every reader list and cannot be opened,
	// for every reader. The server keeps one list and takes it whole, so adding or
	// removing a rule sends the full list, at once. `rulesState` is at the top of the file.
	let blockRules = $state<BlockRule[]>([]);
	/** The save in flight: `add`, or the place in the list of the rule being removed. */
	let savingRule = $state<'add' | number | null>(null);
	let ruleList = $state<HTMLUListElement | null>(null);
	let ruleForm = $state<HTMLFormElement | null>(null);

	/**
	 * Reads and saves share this counter, and only the newest of them writes the list. A
	 * read that began before a save holds the list from before it: shown, it would drop
	 * the saved rule from the screen, and the next save, which sends the whole list,
	 * would delete it on the server.
	 */
	let rulesSeq = 0;

	async function refreshBlockRules(): Promise<void> {
		const seq = ++rulesSeq;
		try {
			const rules = await loadSiteBlockRules();
			if (seq !== rulesSeq) return;
			// An answer that carries no list comes from a server without block rules.
			if (!Array.isArray(rules)) {
				if (rulesState !== 'ready') rulesState = 'off';
				return;
			}
			blockRules = rules;
			rulesState = 'ready';
		} catch (err) {
			if (seq !== rulesSeq) return;
			// A later load that fails keeps the list on screen: it is the last one the
			// server gave.
			if (rulesState === 'ready') return;
			// Only a server (or backend) without the rules hides the section. Any other
			// error can pass, so the section stays and offers to ask again.
			const message = err instanceof Error ? err.message : '';
			const unsupported =
				message.includes('Unknown field "siteBlockRules"') ||
				message.includes('unavailable on this backend');
			rulesState = unsupported ? 'off' : 'failed';
		}
	}

	function retryBlockRules(): void {
		rulesState = 'loading';
		void refreshBlockRules();
	}

	// The tags a rule can name. The list follows the admin's own NSFW setting, so it is
	// read again when that setting changes. It is not asked for on a server without
	// block rules.
	let tagList = $state<TagInfo[]>([]);
	let tagsState = $state<'loading' | 'ready' | 'failed'>('loading');
	/** Only the newest request writes: two reads overlap when the setting is switched twice. */
	let tagsSeq = 0;

	async function refreshTags(): Promise<void> {
		const seq = ++tagsSeq;
		try {
			const tags = await loadTags();
			if (seq !== tagsSeq) return;
			tagList = tags;
			tagsState = 'ready';
			// A choice the new list does not offer would leave the picker blank.
			if (tagPick && !tags.some((t) => t.id === tagPick)) tagPick = '';
		} catch {
			if (seq !== tagsSeq) return;
			tagList = [];
			tagsState = 'failed';
		}
	}

	$effect(() => {
		if (rulesState !== 'ready' || !auth.user) return;
		void auth.user.showNsfw;
		void refreshTags();
	});

	const tagNames = $derived(new Map(tagList.map((t) => [t.id, t.name])));
	const tagName = (id: string): string | undefined => tagNames.get(id);

	// The rule being put together. It is saved only by "Add rule".
	let draftTags = $state<string[]>([]);
	let draftRatings = $state<ContentRating[]>([]);
	let tagPick = $state('');
	const shelves = $derived(tagShelves(tagList, draftTags));
	const tagsFull = $derived(draftTags.length >= MAX_RULE_TAGS);
	const draftCheck = $derived(checkDraft({ tags: draftTags, ratings: draftRatings }, blockRules));

	function addDraftTag(): void {
		if (!tagPick || tagsFull || draftTags.includes(tagPick)) return;
		draftTags = [...draftTags, tagPick];
		tagPick = '';
	}

	function removeDraftTag(id: string): void {
		draftTags = draftTags.filter((t) => t !== id);
	}

	/** Send the whole list. True when the server stored it. */
	async function saveBlockRules(next: BlockRule[]): Promise<boolean> {
		// Outdates a read that is still in flight: see `rulesSeq`.
		rulesSeq++;
		try {
			blockRules = await setSiteBlockRules(next);
			toast.success('Blocked content saved.');
			return true;
		} catch (err) {
			toast.fromError(err, 'The rules could not be saved.');
			await refreshBlockRules();
			return false;
		}
	}

	async function onAddRule(ev: SubmitEvent): Promise<void> {
		ev.preventDefault();
		if (savingRule !== null || rulesState !== 'ready' || !draftCheck.ok) return;
		savingRule = 'add';
		try {
			const sent = { tags: draftTags, ratings: draftRatings };
			if (await saveBlockRules([...blockRules, tidyRule(sent)])) {
				// The form stays usable while the save runs. A draft that was changed in
				// that time is the start of the next rule, and is kept.
				if (sameRule(sent, { tags: draftTags, ratings: draftRatings })) {
					draftTags = [];
					draftRatings = [];
					tagPick = '';
				}
			}
		} finally {
			savingRule = null;
		}
	}

	async function onRemoveRule(i: number): Promise<void> {
		if (savingRule !== null) return;
		const rule = blockRules[i];
		const ok = await confirm({
			title: 'Remove this rule?',
			message: `${ruleText(rule, tagName)}.\n\nSeries that only this rule hides are shown to every reader again.`,
			confirmLabel: 'Remove rule',
			danger: true,
		});
		if (!ok || savingRule !== null) return;
		// Found again by what it says, not by place or identity: the list can have been
		// read again while the question was open. Gone already means nothing to save.
		const at = blockRules.findIndex((r) => sameRule(r, rule));
		if (at < 0) return;
		savingRule = at;
		try {
			if (!(await saveBlockRules(blockRules.filter((_, n) => n !== at)))) return;
		} finally {
			savingRule = null;
		}
		// The button that was pressed is disabled while it saves, and can be gone after:
		// focus is then on nothing. Put it on the rule now in that place, else on the last
		// rule, else on the form, so a keyboard does not start again from the top of the page.
		await tick();
		if (document.activeElement !== document.body) return;
		const buttons = ruleList?.querySelectorAll('button') ?? [];
		const target =
			buttons[Math.min(at, buttons.length - 1)] ??
			ruleForm?.querySelector<HTMLElement>('select:enabled, input:enabled');
		target?.focus();
	}

	// ---- Accounts: the sign-up switch -------------------------------------------
	// Read from the public `instance` facts (self-host builds already loaded them at
	// boot; the hosted console asks here). A server that predates them hides the
	// section. The server refuses the change when its environment pins the value.
	$effect(() => {
		if (!instance.ready) void loadInstance();
	});
	let signupBusy = $state(false);
	let signupPinned = $state<string | null>(null);

	async function onToggleSignup(next: boolean): Promise<void> {
		if (!instance.info || signupBusy) return;
		signupBusy = true;
		try {
			const open = await setRegistrationOpen(next);
			instance.info = { ...instance.info, registrationOpen: open };
			toast.success(open ? 'Anyone can sign up now.' : 'Sign-up is closed.');
		} catch (err) {
			const message = err instanceof Error ? err.message : '';
			if (message.includes('KOMIKA_REGISTRATION_OPEN')) signupPinned = message;
			else toast.fromError(err, 'The setting could not be saved.');
		} finally {
			signupBusy = false;
		}
	}

	// ---- Sources: the MangaDex switch ------------------------------------------
	// Off on a new self-hosted install. The server refuses the change when its
	// environment pins it (`pinnedBy`); the switch is then read-only.
	let mangadex = $state<MangadexSetting | null>(null);
	let mangadexBusy = $state(false);

	async function refreshMangadex(): Promise<void> {
		try {
			mangadex = await loadMangadexSetting();
		} catch {
			mangadex = null; // An older server: the row is hidden.
		}
	}

	async function onToggleMangadex(next: boolean): Promise<void> {
		if (!mangadex || mangadexBusy) return;
		mangadexBusy = true;
		try {
			const enabled = await setMangadexEnabled(next);
			mangadex = { ...mangadex, enabled };
			if (instance.info) instance.info = { ...instance.info, mangadexEnabled: enabled };
			toast.success(enabled ? 'MangaDex is on.' : 'MangaDex is off.');
		} catch (err) {
			toast.fromError(err, 'The setting could not be saved.');
			await refreshMangadex();
		} finally {
			mangadexBusy = false;
		}
	}

	// ---- Sources: MangaDex ratings ---------------------------------------------
	// What readers are shown, not whether the server uses MangaDex: with this off no
	// response carries a MangaDex rating and a card shows our own readers' rating only.
	// Read from the public `instance` facts like the sign-up switch; a server that
	// predates the setting has no such fact and the row is hidden.
	let ratingsBusy = $state(false);
	let ratingsPinned = $state<string | null>(null);

	async function onToggleMangadexRatings(next: boolean): Promise<void> {
		if (!instance.info || ratingsBusy) return;
		ratingsBusy = true;
		try {
			const shown = await setShowMangadexRatings(next);
			instance.info = { ...instance.info, showMangadexRatings: shown };
			toast.success(shown ? 'MangaDex ratings are shown.' : 'MangaDex ratings are hidden.');
		} catch (err) {
			const message = err instanceof Error ? err.message : '';
			if (message.includes('KOMIKA_MANGADEX_RATINGS')) ratingsPinned = message;
			else toast.fromError(err, 'The setting could not be saved.');
		} finally {
			ratingsBusy = false;
		}
	}

	// ---- Sources: extension repos ---------------------------------------------
	// The engine keeps the repo list; extension counts are read off its extension list.
	let repoList = $state<ExtensionRepo[]>([]);
	let exts = $state<ExtensionInfo[]>([]);
	let extsLoading = $state(false);
	let extsError = $state<string | null>(null);
	/** The engine is down or still starting: a calm note, not an error wall. */
	let engineDown = $state(false);
	let repoUrl = $state('');
	let addingRepo = $state(false);
	let removingRepo = $state<string | null>(null);

	const repos = $derived(
		repoList.map((r) => ({ ...r, count: exts.filter((e) => e.repo === r.indexUrl).length })),
	);

	async function refreshRepos(): Promise<void> {
		extsLoading = true;
		extsError = null;
		try {
			repoList = await loadExtensionRepos();
			exts = repoList.length ? await loadExtensions(false) : [];
			engineDown = false;
		} catch (err) {
			if (isEngineUnreachable(err)) engineDown = true;
			else extsError = err instanceof Error ? err.message : 'Failed to load extension repos.';
		} finally {
			extsLoading = false;
		}
	}

	async function onAddRepo(ev: SubmitEvent): Promise<void> {
		ev.preventDefault();
		if (addingRepo) return;
		addingRepo = true;
		try {
			const count = await addExtensionRepo(repoUrl.trim());
			toast.success(`Repo added. ${count} extension${count === 1 ? '' : 's'} known.`);
			repoUrl = '';
			await refreshRepos();
		} catch (err) {
			if (isEngineUnreachable(err)) engineDown = true;
			else toast.fromError(err, 'Failed to add the repo.');
		} finally {
			addingRepo = false;
		}
	}

	async function onRemoveRepo(r: ExtensionRepo): Promise<void> {
		if (removingRepo) return;
		const ok = await confirm({
			title: `Remove ${r.name}?`,
			message:
				'Its extensions are no longer offered for install or updates. Series you already added stay in the catalog.',
			confirmLabel: 'Remove repo',
			danger: true,
		});
		if (!ok) return;
		removingRepo = r.indexUrl;
		try {
			await removeExtensionRepo(r.indexUrl);
			toast.success(`Removed ${r.name}.`);
			await refreshRepos();
		} catch (err) {
			if (isEngineUnreachable(err)) engineDown = true;
			else toast.fromError(err, 'Failed to remove the repo.');
		} finally {
			removingRepo = null;
		}
	}

	// ---- Sources: request limits -----------------------------------------------
	// Some sites block a server that asks too often. A limit makes the scanner check
	// that source one series at a time, evenly spaced. The server applies a change
	// at once. A limit from the server environment (`fromEnv`) is the server's default:
	// an admin can change it here, and removing that change restores the default.
	/** `null`: an older server without the feature, so the row is hidden. */
	let limits = $state<SourceRateLimit[] | null>(null);
	let limitSources = $state<SourceInfo[]>([]);
	let limitSourceId = $state('');
	let limitRequests = $state(10);
	let limitMinutes = $state(5);
	let limitRequestsInput = $state<HTMLInputElement | null>(null);
	let savingLimit = $state(false);
	let removingLimit = $state<string | null>(null);

	function limitSourceName(sourceId: string): string {
		const src = limitSources.find((x) => x.id === sourceId);
		return src ? `${src.name} (${src.lang})` : `Source ${sourceId}`;
	}

	/** The sources the form offers: every installed one, and any limited source the
	 * engine does not list (its extension was removed, or the engine is down). */
	const limitChoices = $derived(
		[...new Set([...limitSources.map((src) => src.id), ...(limits ?? []).map((l) => l.sourceId)])]
			.map((id) => ({ id, label: limitSourceName(id) }))
			.sort((a, b) => a.label.localeCompare(b.label)),
	);
	const limitExisting = $derived(limits?.find((l) => l.sourceId === limitSourceId) ?? null);

	function plural(n: number, unit: string): string {
		return `${n} ${unit}${n === 1 ? '' : 's'}`;
	}

	function span(secs: number): string {
		return secs % 60 === 0 ? plural(secs / 60, 'minute') : plural(secs, 'second');
	}

	function limitRule(l: SourceRateLimit): string {
		return `${plural(l.requests, 'request')} every ${span(l.windowSecs)}`;
	}

	/** What the limit means for the scanner: one series check costs two requests. */
	function limitPace(l: SourceRateLimit): string {
		const checks = Math.max(1, Math.floor(l.requests / 2));
		return `One series checked every ${span(Math.round(l.windowSecs / checks))}`;
	}

	async function refreshLimits(): Promise<void> {
		try {
			limits = await loadSourceRateLimits();
		} catch {
			limits = null;
			return;
		}
		try {
			limitSources = await loadSources();
		} catch {
			limitSources = []; // Names are a nicety: the list falls back to source ids.
		}
	}

	/** Put a limit's numbers in the form, so saving changes that limit. */
	function loadLimit(l: SourceRateLimit): void {
		limitSourceId = l.sourceId;
		limitRequests = l.requests;
		limitMinutes = Math.max(1, Math.round(l.windowSecs / 60));
	}

	function onPickLimitSource(): void {
		if (limitExisting) loadLimit(limitExisting);
	}

	function onEditLimit(l: SourceRateLimit): void {
		loadLimit(l);
		limitRequestsInput?.focus(); // Also scrolls the form into view.
		limitRequestsInput?.select();
	}

	async function onSaveLimit(ev: SubmitEvent): Promise<void> {
		ev.preventDefault();
		if (savingLimit || !limitSourceId) return;
		savingLimit = true;
		try {
			const saved = await setSourceRateLimit(limitSourceId, limitRequests, limitMinutes * 60);
			toast.success(`${limitSourceName(saved.sourceId)}: ${limitRule(saved)}.`);
			limitSourceId = '';
			await refreshLimits();
		} catch (err) {
			toast.fromError(err, 'The limit could not be saved.');
		} finally {
			savingLimit = false;
		}
	}

	/** Remove an admin's limit. A source with a server default goes back to it. */
	async function onRemoveLimit(l: SourceRateLimit): Promise<void> {
		if (removingLimit) return;
		const name = limitSourceName(l.sourceId);
		const ok = await confirm(
			l.overridesEnv
				? {
						title: `Go back to the server limit for ${name}?`,
						message: 'Your limit is removed. The limit set on the server applies again.',
						confirmLabel: 'Use server limit',
					}
				: {
						title: `Remove the limit for ${name}?`,
						message:
							'The server then checks this source at full speed again. A site that asked for fewer requests can block the server.',
						confirmLabel: 'Remove limit',
						danger: true,
					},
		);
		if (!ok) return;
		removingLimit = l.sourceId;
		try {
			await removeSourceRateLimit(l.sourceId);
			toast.success(
				l.overridesEnv ? `${name} uses the server limit again.` : `Removed the limit for ${name}.`,
			);
			if (limitSourceId === l.sourceId) limitSourceId = '';
			await refreshLimits();
		} catch (err) {
			toast.fromError(err, 'The limit could not be removed.');
		} finally {
			removingLimit = null;
		}
	}

	// ---- Extension updater ----------------------------------------------------
	// The server updates installed extensions on a timer; this is its status and
	// history, plus a way to run it now.
	let autoUpdate = $state<ExtensionAutoUpdate | null>(null);
	let autoUpdateError = $state<string | null>(null);
	let autoUpdateStarting = $state(false);
	/** Bumped by every poll loop so a newer one supersedes an older one. */
	let autoUpdatePoll = 0;
	/** Set when the page is left. Nothing may start, or carry on, after that: the
	 * controls that would stop it are gone. */
	let left = false;

	async function refreshAutoUpdate(): Promise<void> {
		try {
			autoUpdate = await loadExtensionAutoUpdate();
			autoUpdateError = null;
		} catch (err) {
			autoUpdateError = err instanceof Error ? err.message : 'Failed to load update status.';
		}
	}

	/** Start a pass, then follow it until it finishes (it runs server-side, and each
	 * extension is a download, so it outlasts the request that started it). */
	async function runUpdatesNow(): Promise<void> {
		autoUpdateStarting = true;
		autoUpdateError = null;
		try {
			autoUpdate = await runExtensionUpdates();
		} catch (err) {
			autoUpdateError = err instanceof Error ? err.message : 'Failed to start the update.';
			return;
		} finally {
			autoUpdateStarting = false;
		}
		await followUpdates();
	}

	/** Re-read the status until the running pass ends. Also started on load, because a
	 * pass the server began on its own timer would otherwise leave "Update all now"
	 * disabled until a reload. */
	async function followUpdates(): Promise<void> {
		if (left) return;
		const poll = ++autoUpdatePoll;
		// ~5 minutes at most; a pass that long is stuck, and a reload still works.
		for (let i = 0; i < 75 && autoUpdate?.running && poll === autoUpdatePoll; i++) {
			await new Promise((r) => setTimeout(r, 4000));
			if (poll !== autoUpdatePoll) return;
			await refreshAutoUpdate();
		}
	}

	function everyLabel(seconds: number): string {
		if (seconds % 3600 === 0) return `${seconds / 3600} h`;
		if (seconds % 60 === 0) return `${seconds / 60} min`;
		return `${seconds} s`;
	}

	$effect(() => {
		if (!auth.user) return;
		void refreshRepos();
		void refreshMangadex();
		void refreshLimits();
		void refreshBlockRules();
		void refreshAutoUpdate().then(followUpdates);
	});
	// Stop following a running update pass, and stop a merge run between batches, when
	// the page is left.
	$effect(() => () => {
		left = true;
		autoUpdatePoll++;
	});

	// ---- Maintenance: save the whole catalogue --------------------------------
	// Materializes the entire Suwayomi library into the DB read-cache the reader
	// reads from (metadata synchronously; chapter lists in a server background
	// task). Heavy on a large catalogue, so it asks first. Execution is gated
	// server-side by require_admin.
	let persisting = $state(false);
	let persistResult = $state<{ ok: boolean; text: string } | null>(null);

	async function runPersistCatalogue(): Promise<void> {
		if (persisting) return;
		const ok = await confirm({
			title: 'Save the whole catalogue now?',
			message:
				'Every series from the source engine is copied into the database. This is slow on a large catalogue. You can run it again safely.',
			confirmLabel: 'Save catalogue',
		});
		if (!ok) return;
		persisting = true;
		persistResult = null;
		try {
			const n = await persistCatalogue();
			persistResult = {
				ok: true,
				text: `Saved ${n} series. Chapter lists are filling in the background.`,
			};
		} catch (err) {
			persistResult = {
				ok: false,
				text: err instanceof Error ? err.message : 'Failed to save the catalogue.',
			};
		} finally {
			persisting = false;
		}
	}

	// ---- Maintenance: store covers in the DB ----------------------------------
	// Crawls the catalogue, fetches each still-uncached work's cover and stores a
	// bounded WebP in the DB so the reader serves covers from /covers/{id}.webp.
	// The returned number is how many works were still uncached at kick-off.
	let coversRunning = $state(false);
	let coversResult = $state<{ ok: boolean; text: string } | null>(null);

	async function runMaterializeCovers(): Promise<void> {
		if (coversRunning) return;
		const ok = await confirm({
			title: 'Download all missing covers now?',
			message:
				'The server fetches every cover it has not stored yet. It runs slowly in the background. You can run it again safely.',
			confirmLabel: 'Download covers',
		});
		if (!ok) return;
		coversRunning = true;
		coversResult = null;
		try {
			const n = await materializeCatalogueCovers();
			coversResult = {
				ok: true,
				text: `Started for ${n} work${n === 1 ? '' : 's'} with no stored cover. Progress shows in the server logs.`,
			};
		} catch (err) {
			coversResult = {
				ok: false,
				text: err instanceof Error ? err.message : 'Failed to start the cover download.',
			};
		} finally {
			coversRunning = false;
		}
	}

	// ---- Maintenance: bulk consolidation (destructive) ------------------------
	// One batch per server call: waiting reviews first, then groups of works that share
	// a title. The run is BOUNDED: a confirm gates the first batch, MAX_BATCHES caps a
	// single run, and Stop halts it between batches. The server merges a pair only on
	// evidence, closes pairs that are two different works, and leaves real questions in
	// Duplicates. A merge is irreversible, so the run stays attended by design.
	const BATCH_SIZE = 200;
	const MAX_BATCHES = 80;

	let consolidating = $state(false);
	let stopRequested = $state(false);
	let consolidateMsg = $state<string | null>(null);
	let batchesDone = $state(0);

	async function consolidate(): Promise<void> {
		if (consolidating) return;
		const ok = await confirm({
			title: 'Merge duplicate works?',
			message:
				`This sorts every pair of works that share a title or an alternative title. Articles (the, a, an) are ignored, so "The X" matches "X".\n\n` +
				`A pair is merged when the author, description, cover or original-language title also matches, or when the shared title is long enough to mean one series. Two MangaDex entries merge only on the same title and the same author. A novel, a sequel, a promo or a work in another language stays separate.\n\n` +
				`Pairs that are clearly two different works are closed. Unclear pairs stay in Duplicates, likeliest first.\n\n` +
				`Each merge permanently deletes the merged work. There is no undo. You can stop the run between batches.`,
			confirmLabel: 'Merge duplicates',
			danger: true,
		});
		if (!ok) return;
		consolidating = true;
		stopRequested = false;
		batchesDone = 0;
		consolidateMsg = 'Starting…';
		let merged = 0;
		let dismissed = 0;
		let pending: number | null = null;
		let stopped = false;
		let finished = false;
		const count = (n: number, noun: string) => `${n} ${noun}${n === 1 ? '' : 's'}`;
		const summary = () =>
			`Merged ${count(merged, 'work')}. Closed ${count(dismissed, 'pair')} as different works.` +
			(pending === null ? '' : ` ${count(pending, 'pair')} left to review.`);
		try {
			for (let i = 0; i < MAX_BATCHES; i++) {
				// `left`: each batch deletes works for good, so a run nobody can see or stop
				// any more ends with the batch in flight.
				if (stopRequested || left) {
					stopped = true;
					break;
				}
				const batch = await consolidateDuplicates(BATCH_SIZE);
				batchesDone += 1;
				merged += batch.merged;
				dismissed += batch.dismissed;
				pending = batch.pending;
				consolidateMsg = `Batch ${batchesDone} done. ${summary()}`;
				if (batch.done) {
					finished = true;
					break;
				}
			}
			consolidateMsg = stopped
				? `Stopped. ${summary()}`
				: finished
					? `Done. ${summary()}`
					: `Batch limit reached. ${summary()} Run again to continue.`;
		} catch (err) {
			const why = err instanceof Error ? err.message : 'Merging failed.';
			consolidateMsg = `${why} Before the failure: ${summary()}`;
		} finally {
			consolidating = false;
			stopRequested = false;
		}
	}
</script>

<div class="k-page">
	<PageHeader
		title="Settings"
		description="Your preferences, extension repos, and jobs that act on the whole instance."
	/>

	<div class="layout">
		<nav class="toc" aria-label="Settings sections">
			{#each sections as s (s.id)}
				<a href="#{s.id}">{s.label}</a>
			{/each}
		</nav>

		<div class="sections">
			<section id="appearance" class="group" aria-labelledby="appearance-h">
				<div class="group-head">
					<div>
						<h2 id="appearance-h" class="group-title">Appearance</h2>
						<p class="group-desc">Saved in this browser only.</p>
					</div>
				</div>
				<div class="group-body">
					<div class="setting">
						<div class="setting-text">
							<p class="setting-name">Theme</p>
							<p class="k-hint">System follows your device setting.</p>
						</div>
						<Segmented
							label="Theme"
							value={theme.pref}
							options={[
								{ value: 'dark', label: 'Dark' },
								{ value: 'light', label: 'Light' },
								{ value: 'system', label: 'System' },
							]}
							onchange={(v: ThemePref) => setTheme(v)}
						/>
					</div>
				</div>
			</section>

			<section id="content" class="group" aria-labelledby="content-h">
				<div class="group-head">
					<div>
						<h2 id="content-h" class="group-title">Content</h2>
						<p class="group-desc">Applies to your account, here and in the reader.</p>
					</div>
				</div>
				<div class="group-body">
					<div class="setting">
						<div class="setting-text">
							<p class="setting-name">Show NSFW content</p>
							<p class="k-hint">
								When this is off, the server blocks source browsing, extension install, and adding
								series from sources for your account. Catalog and Latest updates still list NSFW
								works, so you can fix wrong labels.
							</p>
						</div>
						<Switch
							label="Show NSFW content"
							checked={auth.user?.showNsfw ?? false}
							disabled={nsfwBusy}
							onchange={onToggleNsfw}
						/>
					</div>
				</div>
			</section>

			{#if rulesState !== 'off'}
				<section id="blocked" class="group" aria-labelledby="blocked-h">
					<div class="group-head">
						<div>
							<h2 id="blocked-h" class="group-title">Blocked content</h2>
							<p class="group-desc">
								Series that match a rule are hidden from every reader of this server, signed in or
								not, and cannot be opened. The catalogue pages of this console still show them.
							</p>
						</div>
					</div>
					<div class="group-body rules">
						{#if rulesState === 'loading'}
							<p class="k-hint">Loading rules…</p>
						{:else if rulesState === 'failed'}
							<p class="k-hint" role="alert">
								The rules could not be loaded.
								<button class="k-link retry" type="button" onclick={retryBlockRules}
									>Try again</button
								>
							</p>
						{:else if blockRules.length === 0}
							<p class="k-hint">No rules. Nothing is blocked.</p>
						{:else}
							<!-- Keyed by place: after a removal the next rule takes the row, and the
							     focus on its button with it. -->
							<ul class="repo-list" aria-label="Block rules" bind:this={ruleList}>
								{#each blockRules as rule, i (i)}
									{@const text = ruleText(rule, tagName)}
									<li>
										<span class="repo-text"><span class="repo-name">{text}</span></span>
										<Button
											variant="ghost"
											size="sm"
											icon="trash"
											iconOnly
											aria-label="Remove the rule: {text}"
											busy={savingRule === i}
											disabled={savingRule !== null && savingRule !== i}
											onclick={() => onRemoveRule(i)}
										/>
									</li>
								{/each}
							</ul>
						{/if}

						<!-- Not while the list could not be read: a save sends the whole list. -->
						{#if rulesState !== 'failed'}
							<form
								class="rule-form"
								aria-labelledby="rule-form-h"
								onsubmit={onAddRule}
								bind:this={ruleForm}
							>
								<p id="rule-form-h" class="setting-name">Add a rule</p>

								<div class="k-field">
									<label class="k-label" for="rule-tag">Tags</label>
									{#if draftTags.length > 0}
										<ul class="chips" aria-label="Tags of the new rule">
											{#each draftTags as id (id)}
												{@const name = tagName(id) ?? id}
												<li class="chip">
													<span>{name}</span>
													<button
														type="button"
														class="chip-x"
														aria-label="Remove tag {name}"
														onclick={() => removeDraftTag(id)}><Icon name="x" size={12} /></button
													>
												</li>
											{/each}
										</ul>
									{/if}
									<!-- A button adds the tag, not the choice itself: with a keyboard, moving
									     through a closed list already changes its value. -->
									<div class="tag-line">
										<select
											id="rule-tag"
											class="k-select"
											name="tag"
											autocomplete="off"
											bind:value={tagPick}
											disabled={shelves.length === 0 || tagsFull}
										>
											<option value="">
												{tagsState === 'loading'
													? 'Loading tags…'
													: shelves.length === 0
														? 'No tags to choose'
														: 'Choose a tag'}
											</option>
											{#each shelves as shelf (shelf.label)}
												<optgroup label={shelf.label}>
													{#each shelf.tags as t (t.id)}
														<option value={t.id}>{t.name}</option>
													{/each}
												</optgroup>
											{/each}
										</select>
										<Button icon="plus" disabled={!tagPick || tagsFull} onclick={addDraftTag}
											>Add tag</Button
										>
									</div>
									{#if tagsState === 'failed'}
										<p class="k-hint">
											The tags could not be loaded. A rule can still name ratings.
											<button class="k-link retry" type="button" onclick={refreshTags}
												>Try again</button
											>
										</p>
									{:else if tagsState === 'ready' && tagList.length === 0}
										<p class="k-hint">
											This server has no tags yet. A rule can still name ratings.
										</p>
									{:else}
										<p class="k-hint">
											A series must have all of these tags.
											{#if tagsFull}A rule takes at most {MAX_RULE_TAGS} tags.{/if}
										</p>
									{/if}
									{#if auth.user && !auth.user.showNsfw}
										<p class="k-hint">
											Tags that only adult series carry are not listed while Show NSFW content is
											off.
										</p>
									{/if}
								</div>

								<div class="k-field" role="group" aria-labelledby="rule-ratings-h">
									<span id="rule-ratings-h" class="k-label">Ratings</span>
									<div class="checks">
										{#each RATINGS as r (r)}
											<label class="check">
												<input type="checkbox" name="rating" value={r} bind:group={draftRatings} />
												<span>{RATING_LABEL[r]}</span>
											</label>
										{/each}
									</div>
									<p class="k-hint">Leave empty for any rating.</p>
								</div>

								<div class="rule-foot">
									<Button
										type="submit"
										variant="primary"
										icon="plus"
										aria-describedby="rule-why"
										busy={savingRule === 'add'}
										disabled={!draftCheck.ok || rulesState !== 'ready' || savingRule !== null}
										>Add rule</Button
									>
									<!-- Always here, so a reason that appears is announced. -->
									<p id="rule-why" class="k-hint" role="status">{draftCheck.reason ?? ''}</p>
								</div>
							</form>
						{/if}
					</div>
				</section>
			{/if}

			{#if instance.info}
				<section id="access" class="group" aria-labelledby="access-h">
					<div class="group-head">
						<div>
							<h2 id="access-h" class="group-title">Access</h2>
							<p class="group-desc">
								How readers reach this server, and the Cloudflare setup in front of it.
							</p>
						</div>
					</div>
					<div class="group-body">
						<AccessSettings />
					</div>
				</section>

				<section id="accounts" class="group" aria-labelledby="accounts-h">
					<div class="group-head">
						<div>
							<h2 id="accounts-h" class="group-title">Accounts</h2>
							<p class="group-desc">Who can get an account on this server.</p>
						</div>
					</div>
					<div class="group-body">
						<div class="setting">
							<div class="setting-text">
								<p class="setting-name">Anyone can sign up</p>
								{#if signupPinned}
									<p class="k-hint">{signupPinned}</p>
								{:else}
									<p class="k-hint">
										When this is off, the reader hides sign-up and only people you add on the <a
											class="k-link"
											href={href('/users')}>Users</a
										> page can sign in.
									</p>
								{/if}
							</div>
							<Switch
								label="Anyone can sign up"
								checked={instance.info.registrationOpen}
								disabled={signupBusy || !!signupPinned}
								onchange={onToggleSignup}
							/>
						</div>
					</div>
				</section>
			{/if}

			<section id="sources" class="group" aria-labelledby="sources-h">
				<div class="group-head">
					<div>
						<h2 id="sources-h" class="group-title">Sources</h2>
						<p class="group-desc">
							Where the library comes from. No source is built in: turn on MangaDex, add extension
							repos, or both.
						</p>
					</div>
				</div>
				<div class="group-body jobs">
					{#if mangadex}
						<div class="setting">
							<div class="setting-text">
								<p class="setting-name">MangaDex</p>
								<p class="k-hint">
									When on, the server fetches the MangaDex catalogue, covers and chapter pages. When
									off, it never contacts MangaDex.
								</p>
								{#if mangadex.pinned}
									<p class="k-hint">
										Set by <code>{mangadex.pinnedBy ?? 'the server configuration'}</code> in the server
										environment.
									</p>
								{/if}
							</div>
							<Switch
								label="MangaDex"
								checked={mangadex.enabled}
								disabled={mangadexBusy || mangadex.pinned}
								onchange={onToggleMangadex}
							/>
						</div>
					{/if}

					{#if instance.info?.showMangadexRatings !== undefined}
						<div class="setting">
							<div class="setting-text">
								<p class="setting-name">Show MangaDex ratings</p>
								{#if ratingsPinned}
									<p class="k-hint">{ratingsPinned}</p>
								{:else}
									<p class="k-hint">
										A series can show two ratings: the rating from your readers, and the rating from
										MangaDex. When this is off, every reader sees the rating from your readers only.
										When it is on, each reader can still hide MangaDex ratings in their own
										settings.
									</p>
								{/if}
							</div>
							<Switch
								label="Show MangaDex ratings"
								checked={instance.info.showMangadexRatings}
								disabled={ratingsBusy || !!ratingsPinned}
								onchange={onToggleMangadexRatings}
							/>
						</div>
					{/if}

					<div id="repos" class="setting repos">
						<div class="setting-text">
							<p class="setting-name">Extension repos</p>
							<p class="k-hint">
								A repo is a list of extensions the engine can install. Add the index address each
								repo publishes.
							</p>
						</div>
						{#if engineDown}
							<p class="k-hint calm" role="status">
								<Icon name="info" />
								<span
									>The source engine is not reachable yet. It may still be starting.
									<button class="k-link retry" type="button" onclick={refreshRepos}
										>Try again</button
									></span
								>
							</p>
						{:else}
							<form class="repo-form" onsubmit={onAddRepo}>
								<input
									class="k-input"
									type="url"
									name="repository"
									autocomplete="off"
									aria-label="Repo index address"
									bind:value={repoUrl}
									placeholder="https://example.org/extensions/index.min.json"
									required
								/>
								<Button type="submit" variant="primary" icon="plus" busy={addingRepo}
									>Add repo</Button
								>
							</form>

							{#if extsError}
								<p class="k-error" role="alert">
									{extsError}
									<button class="k-link retry" type="button" onclick={refreshRepos}
										>Try again</button
									>
								</p>
							{:else if repos.length > 0}
								<ul class="repo-list" aria-label="Extension repos">
									{#each repos as r (r.indexUrl)}
										<li>
											<span class="repo-text">
												<span class="repo-name">{r.name}</span>
												<span class="k-mono k-truncate k-muted" title={r.indexUrl}
													>{r.indexUrl}</span
												>
											</span>
											{#if r.count}
												<span class="k-hint k-num"
													>{r.count} extension{r.count === 1 ? '' : 's'}</span
												>
											{/if}
											<Button
												variant="ghost"
												size="sm"
												icon="trash"
												iconOnly
												aria-label="Remove {r.name}"
												busy={removingRepo === r.indexUrl}
												disabled={!!removingRepo && removingRepo !== r.indexUrl}
												onclick={() => onRemoveRepo(r)}
											/>
										</li>
									{/each}
								</ul>
							{:else if extsLoading}
								<p class="k-hint">Loading repos…</p>
							{:else}
								<p class="k-hint">No repo added yet. Extensions appear after you add one.</p>
							{/if}
						{/if}
					</div>

					{#if limits}
						<div id="limits" class="setting repos">
							<div class="setting-text">
								<p class="setting-name">Request limits</p>
								<p class="k-hint">
									Some sites block a server that asks too often. A limit makes the server check that
									source for new chapters slowly, one series at a time. One check uses 2 requests. A
									change applies at once.
								</p>
							</div>
							{#if limitChoices.length > 0}
								<form class="repo-form limit-form" onsubmit={onSaveLimit}>
									<select
										class="k-select"
										name="source"
										autocomplete="off"
										aria-label="Source to limit"
										bind:value={limitSourceId}
										onchange={onPickLimitSource}
										required
									>
										<option value="" disabled>Choose a source</option>
										{#each limitChoices as c (c.id)}
											<option value={c.id}>{c.label}</option>
										{/each}
									</select>
									<label class="limit-field">
										<input
											class="k-input"
											type="number"
											name="requests"
											autocomplete="off"
											min="1"
											max="10000"
											step="1"
											bind:value={limitRequests}
											bind:this={limitRequestsInput}
											required
										/>
										<span>requests every</span>
									</label>
									<label class="limit-field">
										<input
											class="k-input"
											type="number"
											name="minutes"
											autocomplete="off"
											min="1"
											max="1440"
											step="1"
											bind:value={limitMinutes}
											required
										/>
										<span>minutes</span>
									</label>
									<Button
										type="submit"
										variant="primary"
										icon={limitExisting ? undefined : 'plus'}
										busy={savingLimit}>{limitExisting ? 'Save limit' : 'Add limit'}</Button
									>
								</form>
							{:else}
								<p class="k-hint">
									Install an extension first. Then you can limit its source here.
								</p>
							{/if}

							{#if limits.length > 0}
								<ul class="repo-list" aria-label="Request limits">
									{#each limits as l (l.sourceId)}
										{@const name = limitSourceName(l.sourceId)}
										<li>
											<span class="repo-text">
												<span class="repo-name">{name}</span>
												<span class="k-muted">{limitRule(l)}. {limitPace(l)}.</span>
												{#if l.fromEnv}
													<span class="k-hint">Set on the server. You can change it here.</span>
												{:else if l.overridesEnv}
													<span class="k-hint"
														>Your limit. It replaces the one set on the server.</span
													>
												{/if}
											</span>
											<Button
												variant="ghost"
												size="sm"
												icon="edit"
												iconOnly
												aria-label="Change the limit for {name}"
												disabled={!!removingLimit}
												onclick={() => onEditLimit(l)}
											/>
											{#if !l.fromEnv}
												<Button
													variant="ghost"
													size="sm"
													icon="trash"
													iconOnly
													aria-label={l.overridesEnv
														? `Go back to the server limit for ${name}`
														: `Remove the limit for ${name}`}
													busy={removingLimit === l.sourceId}
													disabled={!!removingLimit && removingLimit !== l.sourceId}
													onclick={() => onRemoveLimit(l)}
												/>
											{/if}
										</li>
									{/each}
								</ul>
							{/if}
						</div>
					{/if}
				</div>
				<div class="group-foot">
					<a class="k-link" href={href('/sources')}>Manage installed extensions</a>
				</div>
			</section>

			<section id="updates" class="group" aria-labelledby="updates-h">
				<div class="group-head">
					<div>
						<h2 id="updates-h" class="group-title">Extension updates</h2>
						<p class="group-desc">
							A source that stops working is often one extension release behind. The server can
							update installed extensions on a timer.
						</p>
					</div>
					<Button
						icon="refresh"
						busy={autoUpdateStarting || !!autoUpdate?.running}
						onclick={runUpdatesNow}
					>
						{autoUpdate?.running ? 'Updating…' : 'Update all now'}
					</Button>
				</div>
				<div class="group-body k-stack">
					<div class="setting">
						<div class="setting-text">
							<p class="setting-name">Automatic updates</p>
							<p class="k-hint">
								{#if autoUpdate && !autoUpdate.enabled}
									The server does not update extensions on its own. Set
									<code>EXTENSION_AUTO_UPDATE_INTERVAL_SECONDS</code> on the server to turn this on.
								{:else}
									The interval is set on the server.
								{/if}
							</p>
						</div>
						{#if !autoUpdate}
							<span class="k-badge">{autoUpdateError ? 'Unknown' : 'Loading…'}</span>
						{:else if !autoUpdate.enabled}
							<span class="k-badge" data-tone="warning">Off</span>
						{:else}
							<span class="k-badge" data-tone="success"
								>On, every {everyLabel(autoUpdate.intervalSeconds)}</span
							>
						{/if}
					</div>

					{#if autoUpdateError}
						<div class="k-notice" data-tone="danger" role="alert">
							<Icon name="alert" />
							<div class="k-notice__body">{autoUpdateError}</div>
						</div>
					{/if}

					{#if autoUpdate}
						{#if autoUpdate.lastError}
							<div class="k-notice" data-tone="danger" role="alert">
								<Icon name="alert" />
								<div class="k-notice__body">
									Last run {shortDate(autoUpdate.lastRunAt)} failed: {autoUpdate.lastError}
								</div>
							</div>
						{:else if autoUpdate.lastRunAt}
							<p class="k-hint">
								Last run {shortDate(autoUpdate.lastRunAt)}.
								{#if autoUpdate.lastUpdated === 0 && autoUpdate.lastFailed === 0}
									Everything was up to date.
								{:else}
									{autoUpdate.lastUpdated} updated{#if autoUpdate.lastFailed > 0}, <span class="bad"
											>{autoUpdate.lastFailed} failed</span
										>{/if}.
								{/if}
							</p>
						{:else if !autoUpdate.running}
							<p class="k-hint">No run yet since the server started.</p>
						{/if}

						{#if autoUpdate.recent.length > 0}
							<details class="history">
								<summary>Update history ({autoUpdate.recent.length})</summary>
								<ul>
									{#each autoUpdate.recent as e (e.at + e.pkgName)}
										<li class:bad={!!e.error}>
											<span class="k-muted k-num">{shortDate(e.at)}</span>
											<span class="k-mono k-truncate">{e.pkgName}</span>
											{#if e.error}
												<span>failed: {e.error}</span>
											{:else}
												<span class="k-num"
													>{e.fromVersion ?? 'unknown'} to {e.toVersion ?? 'unknown'}</span
												>
											{/if}
											<span class="k-muted">{e.origin}</span>
										</li>
									{/each}
								</ul>
							</details>
						{/if}
					{/if}
				</div>
			</section>

			<section id="maintenance" class="group" aria-labelledby="maintenance-h">
				<div class="group-head">
					<div>
						<h2 id="maintenance-h" class="group-title">Maintenance</h2>
						<p class="group-desc">
							Jobs that act on the whole catalogue. Each one asks before it starts.
						</p>
					</div>
				</div>
				<div class="group-body jobs">
					<div class="setting">
						<div class="setting-text">
							<p class="setting-name">Save the whole catalogue</p>
							<p class="k-hint">
								Copy every series from the source engine into the database that readers load from.
								Series details are saved first. Chapter lists follow in the background.
							</p>
							{#if persistResult}
								<p class="result" class:bad={!persistResult.ok} role="status">
									{persistResult.text}
								</p>
							{/if}
						</div>
						<Button icon="database" busy={persisting} onclick={runPersistCatalogue}>
							{persisting ? 'Saving…' : 'Save catalogue'}
						</Button>
					</div>

					<div class="setting">
						<div class="setting-text">
							<p class="setting-name">Store covers on this server</p>
							<p class="k-hint">
								Download each cover once and keep it in the database, so covers load from this
								server. Only covers that are not stored yet are fetched.
							</p>
							{#if coversResult}
								<p class="result" class:bad={!coversResult.ok} role="status">{coversResult.text}</p>
							{/if}
						</div>
						<Button icon="image" busy={coversRunning} onclick={runMaterializeCovers}>
							{coversRunning ? 'Starting…' : 'Download covers'}
						</Button>
					</div>

					<div class="setting">
						<div class="setting-text">
							<p class="setting-name">Merge duplicate works</p>
							<p class="k-hint">
								Sort works that share a title or an alternative title. Articles (the, a, an) are
								ignored. A pair with matching evidence is merged. A pair that is two different works
								is closed. Each merge deletes the merged work and cannot be undone. Unclear pairs
								stay in
								<a class="k-link" href={href('/review')}>Duplicates</a>, likeliest first.
							</p>
							{#if consolidateMsg}
								<p class="result" role="status">{consolidateMsg}</p>
							{/if}
						</div>
						<div class="job-actions">
							{#if consolidating}
								<Button disabled={stopRequested} onclick={() => (stopRequested = true)}>
									{stopRequested ? 'Stopping after this batch…' : 'Stop'}
								</Button>
							{/if}
							<Button variant="danger" icon="merge" busy={consolidating} onclick={consolidate}>
								{consolidating ? `Merging, batch ${batchesDone + 1}` : 'Merge duplicates'}
							</Button>
						</div>
					</div>
				</div>
			</section>

			<section id="about" class="group" aria-labelledby="about-h">
				<div class="group-head">
					<h2 id="about-h" class="group-title">About</h2>
				</div>
				<div class="group-body">
					<dl class="facts">
						<dt>Server</dt>
						<!-- The embedded console talks to its own origin's /graphql: show that origin. -->
						<dd class="k-mono">{target === 'embedded' ? serverAddress() : config.apiEndpoint}</dd>
						{#if instance.info?.version}
							<dt>Version</dt>
							<dd class="k-mono">{instance.info.version}</dd>
						{/if}
						<dt>Signed in as</dt>
						<dd>{auth.user?.username ?? 'Unknown'}</dd>
					</dl>
				</div>
			</section>
		</div>
	</div>
</div>

<style>
	.layout {
		display: grid;
		grid-template-columns: 180px minmax(0, 720px);
		gap: var(--k-space-8);
		align-items: start;
	}
	.toc {
		position: sticky;
		top: var(--k-space-6);
		display: grid;
		gap: 2px;
	}
	.toc a {
		padding: 7px var(--k-space-3);
		border-radius: var(--k-radius);
		color: var(--k-text-dim);
		font-size: var(--k-text-sm);
		font-weight: 600;
	}
	.toc a:hover {
		background: var(--k-hover-fill);
		color: var(--k-text-bright);
	}
	/* Sections are separated by a rule and space, not boxed: nothing here needs
	   elevation, and the page reads as one form. */
	.sections {
		display: grid;
		min-width: 0;
	}
	.group {
		padding: var(--k-space-8) 0;
		border-top: 1px solid var(--k-border-1);
		scroll-margin-top: var(--k-space-4);
	}
	.group:first-child {
		padding-top: 0;
		border-top: 0;
	}
	.group-head {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		flex-wrap: wrap;
		gap: var(--k-space-3);
	}
	.group-title {
		font-size: var(--k-text-xl);
		font-weight: 700;
	}
	.group-desc {
		margin-top: 6px;
		max-width: 62ch;
		color: var(--k-text-dim);
		font-size: var(--k-text-sm);
		line-height: 1.6;
	}
	.group-body {
		margin-top: var(--k-space-5);
	}
	.group-foot {
		margin-top: var(--k-space-4);
		font-size: var(--k-text-sm);
	}

	.setting {
		display: flex;
		align-items: center;
		justify-content: space-between;
		flex-wrap: wrap;
		gap: var(--k-space-3) var(--k-space-6);
	}
	.setting-text {
		flex: 1 1 280px;
		min-width: 0;
		display: grid;
		gap: 4px;
	}
	.setting-name {
		color: var(--k-text-1);
		font-weight: 700;
	}
	.jobs {
		display: grid;
		gap: var(--k-space-5);
	}
	.jobs .setting + .setting {
		padding-top: var(--k-space-5);
		border-top: 1px solid var(--k-border);
	}
	.job-actions {
		display: flex;
		flex-wrap: wrap;
		gap: var(--k-space-2);
	}
	.result {
		margin-top: 4px;
		color: var(--k-success);
		font-size: var(--k-text-sm);
		line-height: 1.55;
	}
	.bad {
		color: var(--k-danger);
	}
	code {
		font-family: var(--k-font-mono);
		font-size: 0.92em;
		overflow-wrap: anywhere;
	}

	/* Blocked content: the rules, then the form that adds one. */
	.rules,
	.rule-form {
		display: grid;
		gap: var(--k-space-4);
	}
	.rule-form {
		margin-top: var(--k-space-2);
	}
	.tag-line,
	.rule-foot {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--k-space-2) var(--k-space-3);
	}
	.tag-line .k-select {
		flex: 1 1 220px;
		max-width: 340px;
		min-width: 0;
	}
	/* The same chips as on a series page, without the outline: a soft fill, like the rows. */
	.chips {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		padding: 0;
		list-style: none;
	}
	.chip {
		display: inline-flex;
		align-items: center;
		gap: 2px;
		max-width: 100%;
		min-height: 28px;
		padding: 2px 3px 2px 10px;
		border-radius: var(--k-radius);
		background: var(--k-surface-4);
		color: var(--k-text-1);
		font-size: var(--k-text-sm);
		overflow-wrap: anywhere;
	}
	.chip-x {
		flex: none;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 22px;
		height: 22px;
		border: 0;
		border-radius: var(--k-radius-sm);
		background: transparent;
		color: var(--k-text-dim);
		cursor: pointer;
		transition:
			background var(--k-dur) var(--k-ease),
			color var(--k-dur) var(--k-ease);
	}
	.chip-x:hover {
		background: var(--k-hover-fill);
		color: var(--k-text-bright);
	}
	.checks {
		display: flex;
		flex-wrap: wrap;
		gap: var(--k-space-1) var(--k-space-5);
	}
	.check {
		display: inline-flex;
		align-items: center;
		gap: var(--k-space-2);
		min-height: 28px;
		color: var(--k-text-1);
		font-size: var(--k-text-sm);
		cursor: pointer;
	}
	.check input {
		flex: none;
		width: 16px;
		height: 16px;
		cursor: pointer;
	}
	@media (pointer: coarse) {
		.chip {
			min-height: 40px;
		}
		.chip-x {
			width: 36px;
			height: 36px;
		}
		.check {
			min-height: 44px;
		}
	}

	.repos {
		display: grid;
		justify-content: stretch;
		gap: var(--k-space-4);
	}
	.repo-form {
		display: flex;
		flex-wrap: wrap;
		gap: var(--k-space-2);
	}
	.repo-form .k-input {
		flex: 1 1 280px;
		min-width: 0;
	}
	/* Flat rows, no rules: the list reads as one quiet block. */
	.repo-list {
		display: grid;
		gap: 2px;
		padding: 0;
		list-style: none;
	}
	.repo-list li {
		display: flex;
		align-items: center;
		gap: var(--k-space-3);
		min-height: 52px;
		padding: 6px 6px 6px 14px;
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
		color: var(--k-text-2);
	}
	.repo-text {
		display: grid;
		gap: 2px;
		flex: 1 1 auto;
		min-width: 0;
		font-size: var(--k-text-sm);
	}
	.repo-name {
		color: var(--k-text-1);
		font-weight: 600;
	}
	.repo-text .k-mono {
		font-size: var(--k-text-xs);
	}
	.limit-form .k-select {
		flex: 1 1 200px;
		min-width: 0;
	}
	.limit-field {
		display: flex;
		align-items: center;
		gap: var(--k-space-2);
		color: var(--k-text-2);
		font-size: var(--k-text-sm);
		white-space: nowrap;
	}
	.repo-form .limit-field .k-input {
		flex: none;
		width: 84px;
	}
	.calm {
		display: flex;
		align-items: flex-start;
		gap: 8px;
		line-height: 1.55;
	}
	.calm :global(svg) {
		flex: none;
		margin-top: 3px;
	}
	.retry {
		padding: 0;
		border: 0;
		background: none;
		font: inherit;
		cursor: pointer;
	}

	.history summary {
		cursor: pointer;
		color: var(--k-text-muted);
		font-size: var(--k-text-sm);
		font-weight: 600;
	}
	.history ul {
		display: grid;
		gap: 6px;
		margin-top: var(--k-space-3);
		padding: 0;
		list-style: none;
		font-size: var(--k-text-xs);
	}
	.history li {
		display: grid;
		grid-template-columns: 124px minmax(0, 1.4fr) minmax(0, 1fr) auto;
		gap: var(--k-space-3);
		align-items: baseline;
	}

	.facts {
		display: grid;
		grid-template-columns: 120px minmax(0, 1fr);
		gap: var(--k-space-2) var(--k-space-4);
		font-size: var(--k-text-sm);
	}
	.facts dt {
		color: var(--k-text-dim);
	}
	.facts dd {
		overflow-wrap: anywhere;
	}

	@media (max-width: 900px) {
		.layout {
			grid-template-columns: minmax(0, 1fr);
			gap: var(--k-space-4);
		}
		.toc {
			position: static;
			display: flex;
			gap: var(--k-space-1);
			overflow-x: auto;
			padding-bottom: 4px;
		}
		.toc a {
			white-space: nowrap;
		}
		.history li {
			grid-template-columns: minmax(0, 1fr);
			gap: 2px;
		}
	}
</style>
