<script lang="ts">
	import { page } from '$app/state';
	import { auth } from '$lib/auth.svelte';
	import {
		socialLive,
		loadChapterComments,
		submitChapterComment,
		loadSeriesComments,
		submitSeriesComment,
		uploadCommentMedia,
		canModerate,
		deleteComment,
		banCommenter,
		voteOnComment,
		type CommentView,
		type CommentOpts,
		type PendingMedia,
	} from '$lib/data/social-repo';
	import Icon from '$lib/components/Icon.svelte';
	import Avatar from '$lib/components/Avatar.svelte';

	interface Props {
		/** 'chapter' for a per-chapter thread, 'series' for series-level discussion. */
		targetType: 'chapter' | 'series';
		/** Backend id of the target (chapter id or series id). */
		targetId: string;
		/** Stable key for the offline/local fallback store. */
		storageKey: string;
		/** Composer placeholder + sign-in prompt context. */
		prompt?: string;
	}
	let { targetType, targetId, storageKey, prompt = 'Share your thoughts…' }: Props = $props();

	/** Levels of visual indent before replies stop nesting further right (keeps deep
	 *  threads readable on a phone; the tree structure is still exact via parentId). */
	const MAX_INDENT = 4;

	// The flat comment list (root threads + all descendants, chronological). The reply
	// tree is derived from it via `parentId`.
	let comments = $state<CommentView[]>([]);
	let revealed = $state<Record<string, boolean>>({});
	let modError = $state<string | null>(null);

	// Root composer state.
	let draft = $state('');
	let spoiler = $state(false);
	let posting = $state(false);
	let postError = $state<string | null>(null);
	let media = $state<PendingMedia | null>(null);
	let uploading = $state(false);
	let rootFileInput = $state<HTMLInputElement | null>(null);

	// Reply composer state (only one reply box is open at a time).
	let replyingTo = $state<string | null>(null);
	let replyDraft = $state('');
	let replySpoiler = $state(false);
	let replyPosting = $state(false);
	let replyError = $state<string | null>(null);
	let replyMedia = $state<PendingMedia | null>(null);
	let replyUploading = $state(false);
	let replyFileInput = $state<HTMLInputElement | null>(null);

	const needsAuth = $derived(socialLive() && !auth.user);
	// Voting needs the live backend AND a signed-in user; on a Suwayomi/mock deployment
	// (no social layer) the buttons show counts but stay disabled rather than erroring.
	const canVote = $derived(socialLive() && !!auth.user);
	const canMod = $derived(canModerate());

	// Reply tree, derived from the flat list. Roots (top-level, plus any orphan whose
	// parent isn't in the set) show newest-first; each comment's replies stay
	// chronological (oldest-first), the conventional reading order.
	const tree = $derived.by(() => {
		const ids = new Set(comments.map((c) => c.id));
		const childrenById = new Map<string, CommentView[]>();
		const roots: CommentView[] = [];
		for (const c of comments) {
			if (c.parentId && ids.has(c.parentId)) {
				const arr = childrenById.get(c.parentId);
				if (arr) arr.push(c);
				else childrenById.set(c.parentId, [c]);
			} else {
				roots.push(c);
			}
		}
		roots.reverse();
		return { roots, childrenOf: (id: string) => childrenById.get(id) ?? [] };
	});

	const load = (id: string, key: string) =>
		targetType === 'series' ? loadSeriesComments(id, key) : loadChapterComments(id, key);
	const submit = (
		id: string,
		key: string,
		body: string,
		sp: boolean,
		cur: CommentView[],
		opts: CommentOpts,
	) =>
		targetType === 'series'
			? submitSeriesComment(id, key, body, sp, cur, opts)
			: submitChapterComment(id, key, body, sp, cur, opts);

	// (Re)load when the target or sign-in state changes.
	$effect(() => {
		const id = targetId;
		const key = storageKey;
		void auth.user?.id; // reload after sign-in so "mine" resolves
		let cancelled = false;
		if (socialLive() && !id) {
			comments = [];
			return;
		}
		load(id ?? '', key)
			.then((list) => {
				if (!cancelled) {
					comments = list;
					revealed = {};
				}
			})
			.catch(() => {});
		return () => {
			cancelled = true;
		};
	});

	function reveal(id: string) {
		revealed = { ...revealed, [id]: true };
	}

	// ---- media attach ------------------------------------------------------
	async function onPickImage(e: Event, scope: 'root' | 'reply') {
		const input = e.currentTarget as HTMLInputElement;
		const file = input.files?.[0];
		input.value = ''; // allow re-picking the same file later
		if (!file) return;
		if (scope === 'root') {
			uploading = true;
			postError = null;
		} else {
			replyUploading = true;
			replyError = null;
		}
		try {
			const staged = await uploadCommentMedia(file);
			if (scope === 'root') media = staged;
			else replyMedia = staged;
		} catch (err) {
			const msg = err instanceof Error ? err.message : 'Could not upload the image.';
			if (scope === 'root') postError = msg;
			else replyError = msg;
		} finally {
			if (scope === 'root') uploading = false;
			else replyUploading = false;
		}
	}

	// ---- post --------------------------------------------------------------
	const canPostRoot = $derived((draft.trim().length > 0 || !!media) && !posting && !uploading);
	async function postRoot() {
		if (!canPostRoot) return;
		posting = true;
		postError = null;
		try {
			comments = await submit(targetId ?? '', storageKey, draft.trim(), spoiler, comments, {
				media,
			});
			draft = '';
			spoiler = false;
			media = null;
		} catch (err) {
			postError = err instanceof Error ? err.message : 'Could not post your comment.';
		} finally {
			posting = false;
		}
	}

	function openReply(id: string) {
		replyingTo = id;
		replyDraft = '';
		replySpoiler = false;
		replyMedia = null;
		replyError = null;
	}
	function cancelReply() {
		replyingTo = null;
		replyMedia = null;
	}
	const canPostReply = $derived(
		(replyDraft.trim().length > 0 || !!replyMedia) && !replyPosting && !replyUploading,
	);
	async function postReply(parentId: string) {
		if (!canPostReply) return;
		replyPosting = true;
		replyError = null;
		try {
			comments = await submit(
				targetId ?? '',
				storageKey,
				replyDraft.trim(),
				replySpoiler,
				comments,
				{
					parentId,
					media: replyMedia,
				},
			);
			replyingTo = null;
			replyMedia = null;
		} catch (err) {
			replyError = err instanceof Error ? err.message : 'Could not post your reply.';
		} finally {
			replyPosting = false;
		}
	}

	/**
	 * Deep link to the report form with this comment already attached.
	 *
	 * `comment` is the load-bearing one — it is what lets the server verify the author
	 * and snapshot the body. `by` is display-only (the form shows "a comment by X" so the
	 * reader can see they picked the right one); the server ignores it and re-derives the
	 * real author from the id. `series` gives an admin the page to look at, and is only
	 * meaningful when this thread hangs off a series rather than a chapter.
	 */
	function reportHref(c: CommentView): string {
		const p = new URLSearchParams({ kind: 'COMMENT_ABUSE', comment: c.id, by: c.name });
		if (targetType === 'series') p.set('series', targetId);
		return `/support/report?${p}`;
	}

	// ---- moderation --------------------------------------------------------
	async function removeComment(id: string) {
		modError = null;
		try {
			comments = await deleteComment(id, comments);
		} catch (err) {
			modError = err instanceof Error ? err.message : 'Could not delete the comment.';
		}
	}
	// Like (1) / dislike (-1) a comment; clicking your current vote clears it. Optimistic
	// via the returned list; a failure surfaces in `modError`.
	let voting = $state<string | null>(null);
	async function vote(c: CommentView, value: number) {
		if (!canVote || voting) return;
		const next = c.myVote === value ? 0 : value;
		voting = c.id;
		modError = null;
		try {
			comments = await voteOnComment(c.id, next, comments);
		} catch (err) {
			modError = err instanceof Error ? err.message : 'Could not register your vote.';
		} finally {
			voting = null;
		}
	}
	async function banAuthor(c: CommentView) {
		modError = null;
		if (!confirm(`Ban ${c.name}? They won't be able to sign in.`)) return;
		try {
			await banCommenter(c.authorId);
			comments = await load(targetId ?? '', storageKey);
			revealed = {};
		} catch (err) {
			modError = err instanceof Error ? err.message : 'Could not ban the user.';
		}
	}
</script>

<div class="c-head">
	<h3><Icon name="comment" size={20} stroke="#87857f" />{comments.length} comments</h3>
</div>

{#if needsAuth}
	<div class="signin-prompt">
		<span>Sign in to join the discussion.</span>
		<a
			class="signin-link"
			href={`/login?redirect=${encodeURIComponent(page.url.pathname + page.url.search)}`}
		>
			Sign in
		</a>
	</div>
{:else}
	<div class="composer">
		<div class="avatar me">
			<Avatar
				url={auth.user?.avatarUrl}
				name={auth.user?.username ?? 'You'}
				colorKey={auth.user?.id}
				size={40}
			/>
		</div>
		<div class="composer-body">
			<textarea bind:value={draft} placeholder={prompt} rows="3"></textarea>
			{#if media}
				<div class="media-preview">
					<img src={media.url} alt="Attachment preview" />
					<button class="media-remove" onclick={() => (media = null)} aria-label="Remove image">
						<Icon name="x" size={15} />
					</button>
				</div>
			{/if}
			{#if postError}<p class="post-error">{postError}</p>{/if}
			<div class="composer-foot">
				<label class="spoiler-toggle">
					<input type="checkbox" bind:checked={spoiler} />
					<span>Contains spoilers</span>
				</label>
				<div class="foot-actions">
					<button
						class="attach"
						onclick={() => rootFileInput?.click()}
						disabled={uploading || !!media}
						title="Attach an image"
						aria-label="Attach an image"
					>
						{#if uploading}<span class="spin"></span>{:else}<Icon name="image" size={17} />{/if}
					</button>
					<button
						class="post"
						class:enabled={canPostRoot}
						onclick={postRoot}
						disabled={!canPostRoot}
					>
						{posting ? 'Posting…' : 'Post comment'}
					</button>
				</div>
			</div>
			<input
				class="file-hidden"
				type="file"
				accept="image/*"
				bind:this={rootFileInput}
				onchange={(e) => onPickImage(e, 'root')}
			/>
		</div>
	</div>
{/if}

{#if modError}<p class="post-error">{modError}</p>{/if}

<div class="c-list">
	{#each tree.roots as c (c.id)}
		{@render node(c, 0)}
	{/each}
	{#if comments.length === 0}
		<p class="c-empty">No comments yet — be the first to start the discussion.</p>
	{/if}
</div>

<!-- Recursive comment node: renders one comment, its reply composer, then its
     children (which re-enter this snippet), so arbitrary-depth trees render. -->
{#snippet node(c: CommentView, depth: number)}
	<div class="comment" style="--indent:{Math.min(depth, MAX_INDENT)}" class:reply={depth > 0}>
		<div class="avatar">
			<Avatar url={c.avatarUrl} name={c.name} colorKey={c.authorId} size={depth > 0 ? 32 : 40} />
		</div>
		<div class="c-body">
			<div class="c-meta">
				<span class="c-name">{c.name}</span>
				{#if c.isOp}<span class="op">TRANSLATOR</span>{/if}
				{#if c.mine}<span class="mine">You</span>{/if}
				<span class="c-time">{c.time}</span>
			</div>
			{#if c.hasSpoiler && !revealed[c.id]}
				<button class="spoiler-veil" onclick={() => reveal(c.id)}>
					<Icon name="alert" size={14} />Spoiler — tap to reveal
				</button>
			{:else}
				{#if c.body}<p class="c-text">{c.body}</p>{/if}
				{#if c.mediaUrl}
					<a class="c-media" href={c.mediaUrl} target="_blank" rel="noopener noreferrer">
						<img
							src={c.mediaUrl}
							alt="Comment attachment"
							loading="lazy"
							width={c.mediaWidth ?? undefined}
							height={c.mediaHeight ?? undefined}
							style={c.mediaWidth && c.mediaHeight
								? `aspect-ratio:${c.mediaWidth}/${c.mediaHeight}`
								: undefined}
						/>
					</a>
				{/if}
			{/if}
			<div class="c-actions">
				<button
					class="act vote"
					class:on={c.myVote === 1}
					disabled={!canVote || voting === c.id}
					aria-pressed={c.myVote === 1}
					title="Like"
					onclick={() => vote(c, 1)}
				>
					<Icon name="thumbs-up" size={14} />{c.likes || ''}
				</button>
				<button
					class="act vote"
					class:on={c.myVote === -1}
					disabled={!canVote || voting === c.id}
					aria-pressed={c.myVote === -1}
					title="Dislike"
					onclick={() => vote(c, -1)}
				>
					<Icon name="thumbs-down" size={14} />{c.dislikes || ''}
				</button>
				{#if !needsAuth}
					<button
						class="act"
						onclick={() => (replyingTo === c.id ? cancelReply() : openReply(c.id))}
					>
						<Icon name="reply" size={14} />{replyingTo === c.id ? 'Cancel' : 'Reply'}
					</button>
				{/if}
				{#if !c.mine}
					<!-- The ONLY way a reader can hand us a comment id: they can't see one, so
					     without this link an abuse report is just a username they typed, which
					     an admin cannot verify and which vanishes if the comment is deleted.
					     With the id the server snapshots the body and takes the author from the
					     DB. Not shown on your own comments — deleting is what you want there. -->
					<a class="act" href={reportHref(c)} title="Report this comment to a moderator">
						<Icon name="flag" size={14} />Report
					</a>
				{/if}
				{#if canMod}
					<button class="act mod" onclick={() => removeComment(c.id)}>
						<Icon name="x" size={14} />Delete
					</button>
					{#if !c.mine && c.authorId}
						<button class="act mod danger" onclick={() => banAuthor(c)}>
							<Icon name="alert" size={14} />Ban
						</button>
					{/if}
				{/if}
			</div>

			{#if replyingTo === c.id}
				<div class="reply-composer">
					<textarea bind:value={replyDraft} placeholder={`Reply to ${c.name}…`} rows="2"></textarea>
					{#if replyMedia}
						<div class="media-preview">
							<img src={replyMedia.url} alt="Attachment preview" />
							<button
								class="media-remove"
								onclick={() => (replyMedia = null)}
								aria-label="Remove image"
							>
								<Icon name="x" size={15} />
							</button>
						</div>
					{/if}
					{#if replyError}<p class="post-error">{replyError}</p>{/if}
					<div class="composer-foot">
						<label class="spoiler-toggle">
							<input type="checkbox" bind:checked={replySpoiler} />
							<span>Spoilers</span>
						</label>
						<div class="foot-actions">
							<button
								class="attach"
								onclick={() => replyFileInput?.click()}
								disabled={replyUploading || !!replyMedia}
								aria-label="Attach an image"
							>
								{#if replyUploading}<span class="spin"></span>{:else}<Icon
										name="image"
										size={16}
									/>{/if}
							</button>
							<button
								class="post sm"
								class:enabled={canPostReply}
								onclick={() => postReply(c.id)}
								disabled={!canPostReply}
							>
								{replyPosting ? 'Posting…' : 'Reply'}
							</button>
						</div>
					</div>
					<input
						class="file-hidden"
						type="file"
						accept="image/*"
						bind:this={replyFileInput}
						onchange={(e) => onPickImage(e, 'reply')}
					/>
				</div>
			{/if}

			{#each tree.childrenOf(c.id) as child (child.id)}
				{@render node(child, depth + 1)}
			{/each}
		</div>
	</div>
{/snippet}

<style>
	.c-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		margin-bottom: 18px;
	}
	.c-head h3 {
		display: flex;
		align-items: center;
		gap: 9px;
		margin: 0;
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 18px;
		color: var(--k-text-bright);
	}
	.signin-prompt {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		flex-wrap: wrap;
		padding: 16px 18px;
		border: 1px solid var(--k-border-1);
		border-radius: 12px;
		background: var(--k-surface);
		color: var(--k-text-dimmer);
		font-size: 14px;
		margin-bottom: 20px;
	}
	.signin-link {
		font-weight: 700;
		color: var(--k-primary);
		text-decoration: none;
	}
	.composer {
		display: flex;
		gap: 14px;
		margin-bottom: 26px;
	}
	.avatar {
		flex: 0 0 auto;
		width: 40px;
		height: 40px;
		border-radius: 50%;
		display: flex;
		align-items: center;
		justify-content: center;
		font-weight: 700;
		font-size: 15px;
	}
	.avatar.me {
		background: var(--k-primary);
		color: var(--k-on-primary);
	}
	.composer-body {
		flex: 1;
		min-width: 0;
	}
	textarea {
		width: 100%;
		box-sizing: border-box;
		resize: vertical;
		background: var(--k-surface);
		border: 1px solid var(--k-border-4);
		border-radius: 10px;
		padding: 12px 14px;
		color: var(--k-text-1);
		font: inherit;
		font-size: 14px;
		line-height: 1.5;
	}
	textarea:focus {
		outline: none;
		border-color: var(--k-primary);
	}
	.post-error {
		color: var(--k-danger, #e08a8a);
		font-size: 12.5px;
		margin: 8px 0 0;
	}
	.composer-foot {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		margin-top: 10px;
	}
	.spoiler-toggle {
		display: flex;
		align-items: center;
		gap: 7px;
		font-size: 13px;
		color: var(--k-text-dimmer);
		cursor: pointer;
	}
	.foot-actions {
		display: flex;
		align-items: center;
		gap: 10px;
	}
	.attach {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 34px;
		height: 34px;
		border-radius: 8px;
		border: 1px solid var(--k-border-4);
		background: var(--k-surface);
		color: var(--k-text-dimmer);
		cursor: pointer;
	}
	.attach:hover:not(:disabled) {
		color: var(--k-primary);
		border-color: var(--k-primary);
	}
	.attach:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	.file-hidden {
		display: none;
	}
	.spin {
		width: 15px;
		height: 15px;
		border-radius: 50%;
		border: 2px solid var(--k-border-1);
		border-top-color: var(--k-primary);
		animation: spin 0.7s linear infinite;
	}
	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}
	.media-preview {
		position: relative;
		display: inline-block;
		margin-top: 10px;
		max-width: 220px;
	}
	.media-preview img {
		display: block;
		max-width: 100%;
		max-height: 200px;
		border-radius: 10px;
		border: 1px solid var(--k-border-1);
	}
	.media-remove {
		position: absolute;
		top: 6px;
		right: 6px;
		width: 26px;
		height: 26px;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		border: none;
		border-radius: 50%;
		background: rgba(0, 0, 0, 0.6);
		color: #fff;
		cursor: pointer;
	}
	.post {
		padding: 9px 18px;
		border-radius: 8px;
		border: none;
		background: var(--k-border-1);
		color: var(--k-text-faint);
		font-weight: 700;
		font-size: 13.5px;
		cursor: not-allowed;
	}
	.post.sm {
		padding: 7px 14px;
		font-size: 13px;
	}
	.post.enabled {
		background: var(--k-primary);
		color: var(--k-on-primary);
		cursor: pointer;
	}
	.c-list {
		display: flex;
		flex-direction: column;
		gap: 22px;
	}
	.c-empty {
		color: var(--k-text-faint);
		font-size: 14px;
		margin: 0;
	}
	.comment {
		display: flex;
		gap: 14px;
	}
	/* Nested replies indent proportionally to depth (capped), with a connector rail. */
	.comment.reply {
		margin-top: 18px;
		margin-left: calc(var(--indent, 0) * 14px);
		padding-left: 14px;
		border-left: 2px solid var(--k-border-1);
	}
	.c-body {
		flex: 1;
		min-width: 0;
	}
	.c-meta {
		display: flex;
		align-items: center;
		gap: 9px;
		flex-wrap: wrap;
	}
	.c-name {
		font-weight: 700;
		font-size: 14px;
		color: var(--k-text-1);
	}
	.op,
	.mine {
		font-size: 10px;
		font-weight: 700;
		letter-spacing: 0.03em;
		padding: 2px 6px;
		border-radius: 5px;
	}
	.op {
		color: var(--k-ongoing, #5fbf7e);
		background: rgba(95, 191, 126, 0.14);
	}
	.mine {
		color: var(--k-primary);
		background: rgba(255, 255, 255, 0.08);
	}
	.c-time {
		font-size: 12px;
		color: var(--k-text-fainter);
	}
	.c-text {
		margin: 6px 0 0;
		font-size: 14.5px;
		line-height: 1.55;
		color: var(--k-text-2);
	}
	.c-media {
		display: inline-block;
		margin-top: 10px;
	}
	.c-media img {
		display: block;
		max-width: min(360px, 100%);
		max-height: 320px;
		width: auto;
		height: auto;
		border-radius: 12px;
		border: 1px solid var(--k-border-1);
		background: var(--k-surface);
	}
	.spoiler-veil {
		display: inline-flex;
		align-items: center;
		gap: 7px;
		margin-top: 8px;
		padding: 8px 12px;
		border-radius: 8px;
		border: 1px dashed var(--k-border-4);
		background: transparent;
		color: var(--k-text-dimmer);
		font-size: 13px;
		cursor: pointer;
	}
	.c-actions {
		display: flex;
		align-items: center;
		gap: 16px;
		margin-top: 10px;
	}
	/* Also matches the Report link, which is an <a> (it navigates) among buttons — hence
	   the font/decoration resets a button gets for free. */
	.c-actions .act {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		background: transparent;
		border: none;
		color: var(--k-text-faint);
		font-family: inherit;
		font-size: 13px;
		font-weight: 600;
		text-decoration: none;
		cursor: pointer;
		padding: 0;
	}
	.c-actions .act:hover {
		color: var(--k-primary);
	}
	.c-actions .act:disabled {
		cursor: default;
	}
	.c-actions .act:disabled:hover {
		color: var(--k-text-faint);
	}
	/* The viewer's active vote: like tints primary, dislike tints danger. */
	.c-actions .act.vote.on {
		color: var(--k-primary);
	}
	.c-actions .act.vote:nth-of-type(2).on {
		color: var(--k-danger, #e08a8a);
	}
	.mod {
		color: var(--k-text-dimmer);
	}
	.mod.danger,
	.mod.danger:hover {
		color: var(--k-danger, #e08a8a);
	}
	.reply-composer {
		margin-top: 12px;
		padding: 12px;
		border: 1px solid var(--k-border-1);
		border-radius: 12px;
		background: var(--k-surface);
	}
</style>
