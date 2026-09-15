<script lang="ts">
	import { untrack } from 'svelte';
	import { protectUnsaved } from '$lib/unsaved.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import Avatar from '$lib/components/Avatar.svelte';
	import {
		getMyComments,
		getProfile,
		updateProfile,
		uploadAvatar,
		type MyCommentView,
		type ProfileView,
	} from '$lib/data/source';
	import { auth, revalidateSession } from '$lib/auth.svelte';
	import { backend } from '$lib/context';

	// The profile is fetched HERE, once, and only once auth is ready.
	//
	// There used to be a `+page.ts` that awaited `getProfile()` as well, so every
	// visit pulled the entire payload (session + library + progress) twice: the `load` copy raced ahead of `initAuth`, went out with no token and
	// resolved to null for a signed-in user, and the effect below immediately
	// refetched the real one. Deleting that load removes a full duplicate round trip
	// per visit and costs nothing — its result was always discarded.
	let liveProfile = $state<ProfileView | null>(null);
	const profile = $derived(liveProfile);
	// Signed-out is decided by AUTH — the source of truth — not by `profile` being
	// null: a signed-in viewer has no profile yet while the fetch is in flight, and
	// keying the CTA off `profile` would flash (or, on a hiccup, stick on) "Sign in"
	// for someone who is actually signed in. A signed-in user only ever sees the
	// profile or the loading skeleton.
	const signedOut = $derived(auth.ready && !auth.user);
	// `profileLoaded` distinguishes "fetch still in flight" from "fetch settled on
	// null" — without it, a signed-in user whose `getProfile()` resolves null (an
	// expired/desynced session or a transient backend error) would be trapped on
	// the loading skeleton forever, since `!profile` alone can't tell the two apart.
	let profileLoaded = $state(false);
	let loadFailed = $state(false);
	const loadingProfile = $derived(!signedOut && !profile && !profileLoaded);
	async function refreshProfile(): Promise<void> {
		loadFailed = false;
		const p = await getProfile();
		liveProfile = p;
		profileLoaded = true;
		// Signed-in per auth but nothing came back: re-validate the session so a
		// genuinely expired/revoked login clears `auth.user` and falls through to the
		// sign-in CTA. A transient backend error keeps the user signed in and leaves
		// `loadFailed` set, so the page shows a retry state instead of hanging.
		if (!p && auth.user) {
			loadFailed = true;
			await revalidateSession();
		}
	}
	$effect(() => {
		void auth.ready;
		void auth.user?.id;
		if (!auth.ready) return;
		// Signed out: the sign-in CTA renders; there's nothing to fetch.
		if (!auth.user) return;
		void refreshProfile();
	});

	// --- Edit profile (display name + bio) + avatar upload ------------------
	let editing = $state(false);
	let editName = $state('');
	let editBio = $state('');
	protectUnsaved(
		() =>
			editing &&
			(editName !== (auth.user?.displayName ?? '') || editBio !== (auth.user?.bio ?? '')),
		'Leave this page and discard your profile changes?',
	);
	let savingProfile = $state(false);
	let profileError = $state('');
	let uploadingAvatar = $state(false);
	let avatarError = $state('');
	let fileInput = $state<HTMLInputElement | null>(null);

	const canEdit = $derived(!!auth.user && !!backend.updateProfile);

	function openEditor(): void {
		if (!auth.user) return;
		editName = auth.user.displayName ?? '';
		editBio = auth.user.bio ?? '';
		profileError = '';
		editing = true;
	}

	async function saveProfile(): Promise<void> {
		if (savingProfile) return;
		savingProfile = true;
		profileError = '';
		try {
			const updated = await updateProfile({ displayName: editName, bio: editBio });
			if (updated && auth.user) {
				auth.user.displayName = updated.displayName;
				auth.user.bio = updated.bio;
			}
			editing = false;
			refreshProfile();
		} catch (err) {
			profileError = err instanceof Error ? err.message : 'Could not save your profile.';
		} finally {
			savingProfile = false;
		}
	}

	async function onAvatarPicked(e: Event): Promise<void> {
		const input = e.currentTarget as HTMLInputElement;
		const file = input.files?.[0];
		if (!file) return;
		uploadingAvatar = true;
		avatarError = '';
		try {
			const url = await uploadAvatar(file);
			if (auth.user) auth.user.avatarUrl = url;
			refreshProfile();
		} catch (err) {
			avatarError = err instanceof Error ? err.message : 'Could not upload the image.';
		} finally {
			uploadingAvatar = false;
			input.value = ''; // allow re-picking the same file
		}
	}

	// --- My comments ---------------------------------------------------------------
	// Paged separately from the profile so a long comment history never holds up the
	// header, and "Load more" appends rather than refetching.
	let comments = $state<MyCommentView[]>([]);
	let commentsTotal = $state(0);
	let commentsPage = $state(0);
	let commentsHasNext = $state(false);
	let commentsLoading = $state(false);
	let commentsError = $state(false);
	let revealed = $state<Record<string, boolean>>({});
	async function loadComments(next: number): Promise<void> {
		if (commentsLoading) return;
		commentsLoading = true;
		commentsError = false;
		try {
			const res = await getMyComments(next);
			// Page 1 replaces (first load / retry); later pages append, skipping any row a
			// new comment pushed across the page boundary since the last fetch.
			if (next === 1) comments = res.items;
			else {
				const seen = new Set(comments.map((c) => c.id));
				comments = [...comments, ...res.items.filter((c) => !seen.has(c.id))];
			}
			commentsTotal = res.total;
			commentsPage = next;
			commentsHasNext = res.hasNextPage;
		} catch (err) {
			console.warn('[komika] my comments failed:', err);
			commentsError = true;
		} finally {
			commentsLoading = false;
		}
	}
	// Keyed on the id STRING, not the profile object: saving the bio or a new avatar
	// refetches the profile, and that must not throw away the loaded comment pages.
	const profileId = $derived(profile?.id ?? '');
	$effect(() => {
		if (!profileId) return;
		// `loadComments` reads its own state before its first await; untracked so a
		// load finishing can't re-run this effect and loop back to page 1.
		untrack(() => {
			comments = [];
			commentsPage = 0;
			revealed = {};
			void loadComments(1);
		});
	});
	const countLabel = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;
</script>

<svelte:head>
	<title>{profile?.name ? `${profile.name} · komiq` : 'Profile · komiq'}</title>
</svelte:head>

{#if loadingProfile}
	<!-- Auth restoring or the profile fetch is in flight — show a skeleton, never
	     the sign-in CTA (which would misrepresent a signed-in user as signed out). -->
	<div class="profile k-gutter" aria-busy="true">
		<div class="head">
			<div class="k-skeleton sk-avatar"></div>
			<div class="who">
				<div class="k-skeleton sk-line sk-name"></div>
				<div class="k-skeleton sk-line sk-handle"></div>
			</div>
			<div class="bio-wrap">
				<div class="k-skeleton sk-line sk-bio"></div>
			</div>
		</div>
		<div class="stats">
			{#each Array(4) as _, i (i)}
				<div class="stat">
					<div class="k-skeleton sk-line sk-stat-value"></div>
					<div class="k-skeleton sk-line sk-stat-label"></div>
				</div>
			{/each}
		</div>
		<div class="body">
			<div class="main">
				<div class="k-skeleton sk-line sk-section"></div>
				{#each Array(3) as _, i (i)}<div class="k-skeleton sk-comment"></div>{/each}
			</div>
		</div>
	</div>
{:else if signedOut}
	<!-- Genuinely signed out (auth resolved, no user) — no sample profile is shown. -->
	<div class="signed-out k-gutter">
		<div class="so-icon"><Icon name="bookmark" size={26} /></div>
		<h1>Your profile</h1>
		<p>Sign in to see your reading stats and your comments.</p>
		<a class="so-btn k-hit" href="/login?redirect=%2Fprofile">Sign in</a>
	</div>
{:else if profile}
	<div class="profile k-gutter">
		<div class="head">
			<div class="avatar-wrap">
				<!-- Sized by CSS (smaller on a phone); `size` only seeds the initial's font. -->
				<Avatar
					url={profile.avatarUrl}
					name={profile.name}
					colorKey={profile.id || profile.name}
					size={96}
				/>
				{#if canEdit}
					<button
						class="avatar-edit k-hit"
						aria-label="Change profile photo"
						title="Change profile photo"
						disabled={uploadingAvatar}
						onclick={() => fileInput?.click()}
					>
						{#if uploadingAvatar}<span class="spin"></span>{:else}<span
								class="cam"
								aria-hidden="true">📷</span
							>{/if}
					</button>
					<input
						bind:this={fileInput}
						type="file"
						accept="image/png,image/jpeg,image/webp"
						class="visually-hidden"
						aria-label="Upload profile photo"
						onchange={onAvatarPicked}
					/>
				{/if}
			</div>
			<div class="who">
				<div class="name-row">
					<h1>{profile.name}</h1>
					<span class="pro">{profile.badge}</span>
				</div>
				<div class="handle">{profile.handle} · {profile.since}</div>
			</div>
			<div class="bio-wrap">
				<p class="bio">{profile.bio}</p>
				{#if avatarError}<p class="setting-error" role="alert">{avatarError}</p>{/if}
			</div>
			{#if auth.user}
				<div class="actions">
					{#if canEdit}
						<button class="edit k-hit" onclick={openEditor} aria-expanded={editing}
							>Edit profile</button
						>
					{/if}
					<a class="gear k-hit" href="/settings">
						<Icon name="gear" size={17} /><span>Settings</span>
					</a>
				</div>
			{/if}
		</div>

		{#if editing}
			<div class="editor">
				<label class="field">
					<span>Display name</span>
					<input
						type="text"
						name="displayName"
						autocomplete="nickname"
						bind:value={editName}
						maxlength="50"
						placeholder={auth.user?.username}
					/>
				</label>
				<label class="field">
					<span>Bio</span>
					<textarea
						name="bio"
						autocomplete="off"
						bind:value={editBio}
						maxlength="500"
						rows="3"
						placeholder="Tell readers about yourself…"
					></textarea>
				</label>
				{#if profileError}<p class="setting-error" role="alert">{profileError}</p>{/if}
				<div class="editor-actions">
					<button class="cancel k-hit" onclick={() => (editing = false)} disabled={savingProfile}
						>Cancel</button
					>
					<button class="save k-hit" onclick={saveProfile} disabled={savingProfile}>
						{savingProfile ? 'Saving…' : 'Save changes'}
					</button>
				</div>
			</div>
		{/if}

		<div class="stats">
			{#each profile.stats as s (s.label)}
				<div class="stat">
					<span class="stat-value">{s.value}</span>
					<span class="stat-label">{s.label}</span>
				</div>
			{/each}
		</div>

		<div class="body">
			<section class="main" aria-labelledby="comments-title">
				<div class="section-head">
					<h2 class="section-label" id="comments-title">Your comments</h2>
					{#if commentsPage > 0 && commentsTotal > 0}
						<span class="section-count">{commentsTotal}</span>
					{/if}
				</div>

				{#if commentsPage === 0 && commentsLoading}
					{#each Array(3) as _, i (i)}<div class="k-skeleton sk-comment"></div>{/each}
				{:else if commentsError && comments.length === 0}
					<div class="empty">
						<p>Couldn't load your comments.</p>
						<button class="ghost-btn k-hit" onclick={() => loadComments(1)}>Retry</button>
					</div>
				{:else if comments.length === 0}
					<div class="empty">
						<div class="empty-icon"><Icon name="comment" size={20} /></div>
						<p>You haven't commented yet.</p>
						<span>Join the discussion on a series or a chapter — it'll show up here.</span>
					</div>
				{:else}
					<ul class="comment-list">
						{#each comments as c (c.id)}
							{@const hidden = c.hasSpoiler && !revealed[c.id]}
							<li class="comment" class:linked={!!c.href}>
								<div class="c-top">
									{#if c.href}
										<!-- Stretched over the whole row, so the row is the tap target. -->
										<a class="c-where" href={c.href}>{c.where}</a>
									{:else}
										<span class="c-where">{c.where}</span>
									{/if}
									{#if c.time}<time class="c-time" datetime={c.createdAt} title={c.createdAt}
											>{c.time}</time
										>{/if}
								</div>
								{#if c.isReply}<span class="c-tag">Reply</span>{/if}
								<p class="c-body" class:blurred={hidden}>{c.body}</p>
								{#if hidden}
									<button class="reveal" onclick={() => (revealed = { ...revealed, [c.id]: true })}
										>Spoiler — tap to show</button
									>
								{/if}
								<div class="c-meta">
									<span
										class="c-stat"
										title={countLabel(c.likes, 'like', 'likes')}
										aria-label={countLabel(c.likes, 'like', 'likes')}
									>
										<Icon name="thumbs-up" size={14} /><span aria-hidden="true">{c.likes}</span>
									</span>
									<span
										class="c-stat"
										title={countLabel(c.replies, 'reply', 'replies')}
										aria-label={countLabel(c.replies, 'reply', 'replies')}
									>
										<Icon name="comment" size={14} /><span aria-hidden="true">{c.replies}</span>
									</span>
									{#if c.href}<span class="c-go" aria-hidden="true"
											>View thread <Icon name="chevron-right" size={14} /></span
										>{/if}
								</div>
							</li>
						{/each}
					</ul>
					{#if commentsError}
						<p class="setting-error" role="alert">Couldn't load more comments.</p>
					{/if}
					{#if commentsHasNext}
						<button
							class="ghost-btn k-hit more"
							disabled={commentsLoading}
							onclick={() => loadComments(commentsPage + 1)}
						>
							{commentsLoading ? 'Loading…' : 'Load more'}
						</button>
					{/if}
				{/if}
			</section>
		</div>
	</div>
{:else if loadFailed}
	<!-- Signed-in per auth, but the profile fetch settled empty and the session is
	     still valid (transient backend error) — offer a retry rather than hang. -->
	<div class="signed-out k-gutter">
		<div class="so-icon"><Icon name="bookmark" size={26} /></div>
		<h1>Couldn't load your profile</h1>
		<p>Something went wrong reaching the server. Check your connection and try again.</p>
		<button class="so-btn k-hit" onclick={() => refreshProfile()}>Retry</button>
	</div>
{/if}

<style>
	/* ---- Layout ------------------------------------------------------------
	   Header row, stats, then the comments in one readable column. Settings have
	   their own screen (/settings); the gear in the header row links there. */
	.profile {
		padding-top: 44px;
		padding-bottom: 32px;
	}
	.head {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr) auto;
		grid-template-areas:
			'avatar who actions'
			'avatar bio bio';
		column-gap: 24px;
		row-gap: 10px;
		align-items: center;
	}
	.avatar-wrap,
	.sk-avatar {
		grid-area: avatar;
		align-self: start;
	}
	.who {
		grid-area: who;
		min-width: 0;
		align-self: end;
	}
	.bio-wrap {
		grid-area: bio;
		min-width: 0;
		align-self: start;
	}
	.actions {
		grid-area: actions;
		display: flex;
		gap: 10px;
		align-self: end;
	}
	.body {
		margin-top: 40px;
	}
	.main {
		max-width: 860px;
		display: flex;
		flex-direction: column;
		gap: 14px;
	}

	/* ---- Loading skeleton ---------------------------------------------------- */
	.sk-avatar {
		width: 96px;
		height: 96px;
		border-radius: 50%;
	}
	.sk-line {
		height: 12px;
		border-radius: 5px;
	}
	.sk-name {
		width: min(220px, 100%);
		height: 28px;
		margin-bottom: 10px;
	}
	.sk-handle {
		width: min(200px, 80%);
		height: 13px;
	}
	.sk-bio {
		width: min(460px, 90%);
		height: 14px;
	}
	.sk-stat-value {
		width: 52px;
		height: 24px;
		margin-bottom: 8px;
	}
	.sk-stat-label {
		width: 76px;
		height: 11px;
	}
	.sk-section {
		width: 160px;
		height: 14px;
	}
	.sk-comment {
		height: 112px;
		border-radius: 12px;
	}

	/* ---- Signed out / failed -------------------------------------------------- */
	.signed-out {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 14px;
		padding: 120px 20px;
		text-align: center;
	}
	.so-icon {
		width: 56px;
		height: 56px;
		border-radius: 50%;
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-1);
		display: flex;
		align-items: center;
		justify-content: center;
		color: var(--k-text-disabled);
	}
	.signed-out h1 {
		font-size: 28px;
		margin: 0;
		color: var(--k-text-bright);
	}
	.signed-out p {
		margin: 0;
		max-width: 340px;
		font-size: 14px;
		line-height: 1.5;
		color: var(--k-text-dimmer);
	}
	.so-btn {
		margin-top: 6px;
		height: 42px;
		padding: 0 22px;
		display: inline-flex;
		align-items: center;
		border-radius: 8px;
		background: var(--k-primary);
		color: var(--k-on-primary);
		font-weight: 700;
		font-size: 13.5px;
		text-decoration: none;
		border: none;
		cursor: pointer;
		font-family: inherit;
	}

	/* ---- Header --------------------------------------------------------------- */
	.avatar-wrap {
		position: relative;
		width: 96px;
		height: 96px;
	}
	/* The Avatar sizes itself inline; let the wrapper drive it so the phone layout
	   can shrink it. */
	.avatar-wrap :global(.avatar-img),
	.avatar-wrap :global(.avatar-fallback) {
		width: 100% !important;
		height: 100% !important;
	}
	.avatar-edit {
		position: absolute;
		right: -2px;
		bottom: -2px;
		width: 34px;
		height: 34px;
		border-radius: 50%;
		background: var(--k-primary);
		color: var(--k-on-primary);
		border: 2px solid var(--k-bg, #0c0c0d);
		display: flex;
		align-items: center;
		justify-content: center;
		cursor: pointer;
		font-size: 15px;
		line-height: 1;
		transition: filter 0.15s;
	}
	.avatar-edit:hover:not(:disabled) {
		filter: brightness(1.1);
	}
	.avatar-edit:disabled {
		opacity: 0.7;
		cursor: default;
	}
	.cam {
		font-size: 15px;
	}
	.spin {
		width: 15px;
		height: 15px;
		border-radius: 50%;
		border: 2px solid rgba(255, 255, 255, 0.4);
		border-top-color: #fff;
		animation: spin 0.7s linear infinite;
	}
	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}
	.visually-hidden {
		position: absolute;
		width: 1px;
		height: 1px;
		padding: 0;
		margin: -1px;
		overflow: hidden;
		clip: rect(0, 0, 0, 0);
		white-space: nowrap;
		border: 0;
	}
	.name-row {
		display: flex;
		align-items: center;
		gap: 10px;
		flex-wrap: wrap;
	}
	h1 {
		font-size: 32px;
		line-height: 1.15;
		margin: 0;
		color: var(--k-text-bright);
		overflow-wrap: anywhere;
	}
	.pro {
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.04em;
		color: var(--k-ongoing);
		background: rgba(95, 191, 126, 0.14);
		border: 1px solid rgba(95, 191, 126, 0.4);
		border-radius: 6px;
		padding: 3px 9px;
	}
	.handle {
		font-size: 14px;
		color: var(--k-text-dimmer);
		margin-top: 4px;
		overflow-wrap: anywhere;
	}
	.bio {
		max-width: 620px;
		font-size: 14.5px;
		line-height: 1.6;
		color: var(--k-text-muted);
		margin: 0;
		overflow-wrap: anywhere;
		white-space: pre-line;
	}
	.edit,
	.gear {
		height: 42px;
		padding: 0 18px;
		border-radius: 8px;
		font-weight: 700;
		font-size: 13.5px;
		font-family: inherit;
		cursor: pointer;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		gap: 8px;
		white-space: nowrap;
	}
	.edit {
		background: var(--k-primary);
		border: none;
		color: var(--k-on-primary);
	}
	.gear {
		text-decoration: none;
		background: transparent;
		border: 1px solid var(--k-border-4);
		color: var(--k-text-3);
	}
	.gear:hover {
		border-color: rgba(255, 255, 255, 0.34);
		color: var(--k-text);
	}

	/* ---- Editor --------------------------------------------------------------- */
	.editor {
		margin-top: 24px;
		max-width: 560px;
		display: flex;
		flex-direction: column;
		gap: 16px;
		padding: 22px 24px;
		border: 1px solid var(--k-border-1);
		border-radius: 12px;
		background: var(--k-surface);
	}
	.field {
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.field span {
		font-size: 12.5px;
		font-weight: 600;
		color: var(--k-text-dim);
	}
	.field input,
	.field textarea {
		width: 100%;
		padding: 10px 12px;
		background: var(--k-surface-2, var(--k-surface));
		border: 1px solid var(--k-border-2);
		border-radius: 8px;
		color: var(--k-text);
		font-family: var(--k-font-sans);
		/* 16px keeps iOS Safari from zooming the page on focus. */
		font-size: 16px;
		resize: vertical;
	}
	.field input:focus,
	.field textarea:focus {
		outline: none;
		border-color: var(--k-border-strong);
	}
	.editor-actions {
		display: flex;
		justify-content: flex-end;
		gap: 10px;
	}
	.cancel,
	.save {
		height: 40px;
		padding: 0 18px;
		border-radius: 8px;
		font-weight: 700;
		font-size: 13.5px;
		cursor: pointer;
	}
	.cancel {
		background: transparent;
		border: 1px solid var(--k-border-4);
		color: var(--k-text-3);
	}
	.save {
		background: var(--k-primary);
		border: none;
		color: var(--k-on-primary);
	}
	.save:disabled,
	.cancel:disabled {
		opacity: 0.6;
		cursor: default;
	}

	/* ---- Stats ---------------------------------------------------------------- */
	.stats {
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 1px;
		margin-top: 32px;
		background: var(--k-border-1);
		border: 1px solid var(--k-border-1);
		border-radius: 12px;
		overflow: hidden;
	}
	.stat {
		background: var(--k-surface);
		padding: 18px 20px;
		display: flex;
		flex-direction: column;
		gap: 4px;
		min-width: 0;
	}
	.stat-value {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 26px;
		color: var(--k-text-bright);
		line-height: 1;
	}
	.stat-label {
		font-size: 12.5px;
		color: var(--k-text-dimmer);
	}

	.setting-error {
		margin: 4px 0 0;
		font-size: 12.5px;
		color: var(--k-danger, #e08a8a);
	}

	/* ---- Comments ------------------------------------------------------------- */
	.section-head {
		display: flex;
		align-items: baseline;
		gap: 10px;
	}
	.section-label {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 16px;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		margin: 0;
		color: var(--k-text-dimmer);
	}
	.section-count {
		font-size: 13px;
		font-weight: 700;
		color: var(--k-text-faint);
	}
	.comment-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 10px;
	}
	.comment {
		position: relative;
		display: flex;
		flex-direction: column;
		gap: 8px;
		padding: 14px 16px;
		border: 1px solid var(--k-border-1);
		border-radius: 12px;
		background: var(--k-surface);
		transition:
			border-color 0.15s,
			background-color 0.15s;
	}
	.comment.linked:hover {
		border-color: var(--k-border-4);
	}
	.comment:focus-within {
		border-color: var(--k-border-strong);
	}
	.c-top {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		gap: 12px;
	}
	.c-where {
		min-width: 0;
		font-weight: 700;
		font-size: 14px;
		color: var(--k-text-1);
		text-decoration: none;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	a.c-where {
		outline: none;
	}
	/* Stretched link: the whole card opens the thread. */
	a.c-where::after {
		content: '';
		position: absolute;
		inset: 0;
		border-radius: inherit;
	}
	.c-time {
		flex: 0 0 auto;
		font-size: 12px;
		color: var(--k-text-fainter);
	}
	.c-tag {
		align-self: flex-start;
		font-size: 10.5px;
		font-weight: 700;
		letter-spacing: 0.05em;
		text-transform: uppercase;
		color: var(--k-text-faint);
		border: 1px solid var(--k-border-4);
		border-radius: 5px;
		padding: 1px 6px;
	}
	.c-body {
		margin: 0;
		font-size: 14px;
		line-height: 1.5;
		color: var(--k-text-2);
		white-space: pre-line;
		overflow-wrap: anywhere;
		display: -webkit-box;
		-webkit-line-clamp: 4;
		line-clamp: 4;
		-webkit-box-orient: vertical;
		overflow: hidden;
	}
	.c-body.blurred {
		filter: blur(6px);
		user-select: none;
	}
	/* Above the stretched link so it reveals instead of navigating. */
	.reveal {
		position: relative;
		z-index: 1;
		align-self: flex-start;
		margin-top: -4px;
		min-height: 32px;
		padding: 0 12px;
		border-radius: 8px;
		border: 1px solid var(--k-border-4);
		background: var(--k-surface-2, var(--k-surface));
		color: var(--k-text-2);
		font-family: inherit;
		font-size: 12.5px;
		font-weight: 700;
		cursor: pointer;
	}
	.c-meta {
		display: flex;
		align-items: center;
		gap: 16px;
		font-size: 12.5px;
		color: var(--k-text-faint);
	}
	.c-stat {
		display: inline-flex;
		align-items: center;
		gap: 5px;
		font-variant-numeric: tabular-nums;
	}
	.c-go {
		margin-left: auto;
		display: inline-flex;
		align-items: center;
		gap: 2px;
		font-weight: 600;
		color: var(--k-text-dimmer);
	}
	.comment.linked:hover .c-go {
		color: var(--k-text);
	}
	.empty {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 8px;
		padding: 36px 20px;
		border: 1px dashed var(--k-border-4);
		border-radius: 12px;
		text-align: center;
	}
	.empty p {
		margin: 0;
		font-weight: 600;
		font-size: 14.5px;
		color: var(--k-text-2);
	}
	.empty span {
		max-width: 320px;
		font-size: 13px;
		line-height: 1.45;
		color: var(--k-text-faint);
	}
	.empty-icon {
		width: 40px;
		height: 40px;
		border-radius: 50%;
		display: flex;
		align-items: center;
		justify-content: center;
		background: var(--k-surface-2);
		color: var(--k-text-disabled);
	}
	.ghost-btn {
		height: 40px;
		padding: 0 18px;
		border-radius: 8px;
		border: 1px solid var(--k-border-4);
		background: transparent;
		color: var(--k-text-2);
		font-family: inherit;
		font-weight: 700;
		font-size: 13.5px;
		cursor: pointer;
	}
	.ghost-btn:disabled {
		opacity: 0.6;
		cursor: default;
	}
	.ghost-btn.more {
		align-self: center;
		margin-top: 4px;
	}

	/* ---- Tablet / phone --------------------------------------------------------- */
	@media (max-width: 820px) {
		.body {
			margin-top: 28px;
		}
	}
	@media (max-width: 640px) {
		.profile {
			padding-top: 24px;
		}
		.head {
			grid-template-columns: auto minmax(0, 1fr);
			grid-template-areas:
				'avatar who'
				'bio bio'
				'actions actions';
			column-gap: 16px;
			row-gap: 14px;
		}
		.who {
			align-self: center;
		}
		.avatar-wrap,
		.sk-avatar {
			width: 72px;
			height: 72px;
		}
		.avatar-wrap :global(.avatar-fallback) {
			font-size: 32px !important;
		}
		.avatar-edit {
			width: 30px;
			height: 30px;
			font-size: 13px;
		}
		h1 {
			font-size: 24px;
		}
		.handle {
			font-size: 13px;
		}
		.bio {
			font-size: 14px;
		}
		.actions {
			display: grid;
			grid-auto-flow: column;
			grid-auto-columns: minmax(0, 1fr);
		}
		.stats {
			grid-template-columns: repeat(2, minmax(0, 1fr));
			margin-top: 22px;
		}
		.stat {
			padding: 14px 16px;
		}
		.stat-value {
			font-size: 22px;
		}
		.editor {
			padding: 18px;
			max-width: none;
		}
		.editor-actions > button {
			flex: 1;
		}
		.comment {
			padding: 14px;
		}
	}
</style>
