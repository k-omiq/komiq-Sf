<script lang="ts">
	import { protectUnsaved } from '$lib/unsaved.svelte';
	import { page } from '$app/state';
	import { auth } from '$lib/auth.svelte';
	import {
		socialLive,
		loadChapterComments,
		submitChapterComment,
		loadSeriesComments,
		submitSeriesComment,
		loadScanlatorComments,
		submitScanlatorComment,
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
	import TurnstileWidget from '$lib/components/TurnstileWidget.svelte';

	interface Props {
		/** 'chapter' for a per-chapter thread, 'series' for series-level discussion,
		 *  'scanlator' for the thread of one scanlator. */
		targetType: 'chapter' | 'series' | 'scanlator';
		/** Backend id of the target (chapter id, series id or scanlator id). */
		targetId: string;
		/** Stable key for the offline/local fallback store. */
		storageKey: string;
		/** Composer placeholder + sign-in prompt context. */
		prompt?: string;
	}
	let { targetType, targetId, storageKey, prompt = 'Share your thoughts…' }: Props = $props();

	/** Each level uses one avatar column + gap; replies below this cap share their
	 *  parent's indent while remaining inside it for folding. */
	const MAX_INDENT = 4;

	// The flat comment list (root threads + all descendants, chronological). The reply
	// tree is derived from it via `parentId`.
	let comments = $state<CommentView[]>([]);
	let revealed = $state<Record<string, boolean>>({});
	let collapsed = $state<Record<string, boolean>>({});
	let modError = $state<string | null>(null);

	// Root composer state.
	let draft = $state('');
	let spoiler = $state(false);
	let posting = $state(false);
	let postError = $state<string | null>(null);
	let media = $state<PendingMedia | null>(null);
	let uploading = $state(false);
	let rootFileInput = $state<HTMLInputElement | null>(null);
	// The security check for a server that asks for one; shown once there is something to post.
	let rootCheck = $state<TurnstileWidget>();

	// Reply composer state (only one reply box is open at a time).
	let replyingTo = $state<string | null>(null);
	let replyDraft = $state('');
	protectUnsaved(
		() => !!draft.trim() || !!media || !!(replyingTo && (replyDraft.trim() || replyMedia)),
		'Leave this page and discard your comment draft?',
	);
	let replySpoiler = $state(false);
	let replyPosting = $state(false);
	let replyError = $state<string | null>(null);
	let replyMedia = $state<PendingMedia | null>(null);
	let replyUploading = $state(false);
	let replyFileInput = $state<HTMLInputElement | null>(null);
	let replyCheck = $state<TurnstileWidget>();

	const needsAuth = $derived(socialLive() && !auth.user);
	// Voting needs the live backend AND a signed-in user; on a Suwayomi/mock deployment
	// (no social layer) the buttons show counts but stay disabled rather than erroring.
	const canVote = $derived(socialLive() && !!auth.user);
	const canMod = $derived(canModerate());
	const commentCount = $derived(comments.filter((c) => !c.deleted).length);

	// Reply tree, derived from the flat list. Roots (top-level, plus any orphan whose
	// parent isn't in the set) show newest-first; each comment's replies stay
	// chronological (oldest-first). Tombstones remain in the tree; visual nesting
	// stops at MAX_INDENT without changing the parent relationship.
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
		const descendants = new Map<string, number>();
		function countReplies(id: string): number {
			const count = (childrenById.get(id) ?? []).reduce(
				(total, child) => total + (child.deleted ? 0 : 1) + countReplies(child.id),
				0,
			);
			descendants.set(id, count);
			return count;
		}
		for (const root of roots) countReplies(root.id);
		return {
			roots,
			childrenOf: (id: string) => childrenById.get(id) ?? [],
			replyCount: (id: string) => descendants.get(id) ?? 0,
		};
	});

	const LOAD = {
		chapter: loadChapterComments,
		series: loadSeriesComments,
		scanlator: loadScanlatorComments,
	};
	const SUBMIT = {
		chapter: submitChapterComment,
		series: submitSeriesComment,
		scanlator: submitScanlatorComment,
	};
	const load = (id: string, key: string) => LOAD[targetType](id, key);
	const submit = (
		id: string,
		key: string,
		body: string,
		sp: boolean,
		cur: CommentView[],
		opts: CommentOpts,
	) => SUBMIT[targetType](id, key, body, sp, cur, opts);

	// (Re)load when the target or sign-in state changes.
	$effect(() => {
		const id = targetId;
		const key = storageKey;
		void auth.user?.id; // reload after sign-in so "mine" resolves
		let cancelled = false;
		if (socialLive() && !id) {
			comments = [];
			revealed = {};
			collapsed = {};
			return;
		}
		load(id ?? '', key)
			.then((list) => {
				if (!cancelled) {
					comments = list;
					revealed = {};
					collapsed = {};
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
	function toggleFold(id: string) {
		collapsed = { ...collapsed, [id]: !collapsed[id] };
	}
	function foldLabel(c: CommentView): string {
		const label = c.deleted ? 'deleted comment' : `comment by ${c.name}`;
		return `${collapsed[c.id] ? 'Expand' : 'Collapse'} ${label}`;
	}

	// Deletion or a reload may turn an open reply target into a tombstone.
	$effect(() => {
		if (replyingTo && comments.some((c) => c.id === replyingTo && c.deleted)) cancelReply();
	});

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
				turnstileToken: await rootCheck?.token(),
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
					turnstileToken: await replyCheck?.token(),
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
		if (!confirm('Delete this comment?')) return;
		modError = null;
		try {
			comments = await deleteComment(id, comments, { targetType, storageKey });
			if (replyingTo === id) cancelReply();
		} catch (err) {
			modError = err instanceof Error ? err.message : 'Could not delete the comment.';
		}
	}
	// Like (1) / dislike (-1) a comment; clicking your current vote clears it. Optimistic
	// via the returned list; a failure surfaces in `modError`.
	let voting = $state<string | null>(null);
	async function vote(c: CommentView, value: number) {
		if (!canVote || c.mine || c.deleted || voting) return;
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
			collapsed = {};
		} catch (err) {
			modError = err instanceof Error ? err.message : 'Could not ban the user.';
		}
	}
</script>

<div class="c-head">
	<h3>
		<Icon name="comment" size={20} />{commentCount}
		{commentCount === 1 ? 'comment' : 'comments'}
	</h3>
</div>

{#if needsAuth}
	<div class="signin-prompt">
		<span>Sign in to join the discussion.</span>
		<a
			class="signin-link k-hit"
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
			<textarea
				aria-label="Your comment"
				name="comment"
				bind:value={draft}
				placeholder={prompt}
				rows="3"
			></textarea>
			{#if media}
				<div class="media-preview">
					<img src={media.url} width={media.width} height={media.height} alt="Attachment preview" />
					<button class="media-remove" onclick={() => (media = null)} aria-label="Remove image">
						<Icon name="x" size={15} />
					</button>
				</div>
			{/if}
			{#if draft.trim() || media}<TurnstileWidget bind:this={rootCheck} />{/if}
			{#if postError}<p class="post-error" role="alert">{postError}</p>{/if}
			<div class="composer-foot">
				<label class="spoiler-toggle">
					<input type="checkbox" bind:checked={spoiler} />
					<span>Contains spoilers</span>
				</label>
				<div class="foot-actions">
					<button
						class="attach k-hit"
						onclick={() => rootFileInput?.click()}
						disabled={uploading || !!media}
						title="Attach an image"
						aria-label="Attach an image"
					>
						{#if uploading}<span class="spin"></span>{:else}<Icon name="image" size={17} />{/if}
					</button>
					<button
						class="post k-hit"
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

{#if modError}<p class="post-error" role="alert">{modError}</p>{/if}

<div class="c-list">
	{#each tree.roots as c (c.id)}
		{@render node(c, 0, null)}
	{/each}
	{#if comments.length === 0}
		<p class="c-empty">No comments yet — be the first to start the discussion.</p>
	{/if}
</div>

<!-- Replies nest in the body up to the cap, then follow it at the same indent.
     Both stay inside this comment so folding hides every descendant. -->
{#snippet node(c: CommentView, depth: number, parent: CommentView | null)}
	<div class="comment" class:reply={depth > 0}>
		<div class="c-meta" class:folded={!!collapsed[c.id]}>
			<div class="avatar comment-avatar" class:deleted={!!c.deleted}>
				{#if !c.deleted}
					<Avatar url={c.avatarUrl} name={c.name} colorKey={c.authorId} size={28} />
				{/if}
			</div>
			{#if !c.deleted}
				<span class="c-name" title={c.name}>{c.name}</span>
				{#if c.isOp}<span class="op">TRANSLATOR</span>{/if}
				{#if c.mine}<span class="mine">You</span>{/if}
			{/if}
			{#if depth > MAX_INDENT && parent}
				<span class="c-relation" title={`↪ replying to ${parent.name || 'deleted comment'}`}>
					↪ {parent.name || 'deleted comment'}
				</span>
			{/if}
			<span class="c-time">{c.time}</span>
			{#if collapsed[c.id] && tree.replyCount(c.id) > 0}
				<span class="c-hidden-count">
					{tree.replyCount(c.id)}
					{tree.replyCount(c.id) === 1 ? 'reply' : 'replies'}
				</span>
			{/if}
			<button
				class="c-fold k-hit"
				aria-expanded={!collapsed[c.id]}
				aria-label={foldLabel(c)}
				onclick={() => toggleFold(c.id)}
			>
				<Icon name={collapsed[c.id] ? 'chevron-right' : 'chevron-down'} size={15} />
			</button>
		</div>
		{#if !collapsed[c.id]}
			<div class="c-content">
				{#if depth < MAX_INDENT && tree.childrenOf(c.id).length > 0}
					<button
						class="c-rail k-hit"
						tabindex="-1"
						aria-label={foldLabel(c)}
						onclick={() => toggleFold(c.id)}
					></button>
				{:else}
					<div class="c-rail-spacer"></div>
				{/if}
				<div class="c-body">
					{#if c.deleted}
						<p class="c-text c-deleted">
							{c.deleted === 'author'
								? 'Comment deleted by user'
								: 'Comment removed by a moderator'}
						</p>
					{:else}
						{#if c.hasSpoiler && !revealed[c.id]}
							<button class="spoiler-veil" onclick={() => reveal(c.id)}>
								<Icon name="alert" size={14} />Spoiler — tap to reveal
							</button>
						{:else}
							{#if c.body}<p class="c-text">{c.body}</p>{/if}
							{#if c.mediaUrl}
								<a
									class="c-media"
									class:legacy={!c.mediaWidth || !c.mediaHeight}
									aria-label="Open comment attachment in a new tab"
									href={c.mediaUrl}
									target="_blank"
									rel="noopener noreferrer"
								>
									<img
										src={c.mediaUrl}
										alt="Comment attachment"
										loading="lazy"
										width={c.mediaWidth || 360}
										height={c.mediaHeight || 270}
										style={c.mediaWidth && c.mediaHeight
											? `aspect-ratio:${c.mediaWidth}/${c.mediaHeight}`
											: undefined}
									/>
								</a>
							{/if}
						{/if}
						<div class="c-actions">
							<button
								class="act k-hit vote"
								class:on={c.myVote === 1}
								disabled={!canVote || c.mine || voting === c.id}
								aria-pressed={c.myVote === 1}
								title={c.mine ? "You can't vote on your own comment" : 'Like'}
								onclick={() => vote(c, 1)}
							>
								<Icon name="thumbs-up" size={14} />{c.likes || ''}
							</button>
							<button
								class="act k-hit vote"
								class:on={c.myVote === -1}
								disabled={!canVote || c.mine || voting === c.id}
								aria-pressed={c.myVote === -1}
								title={c.mine ? "You can't vote on your own comment" : 'Dislike'}
								onclick={() => vote(c, -1)}
							>
								<Icon name="thumbs-down" size={14} />{c.dislikes || ''}
							</button>
							{#if !needsAuth}
								<button
									class="act k-hit"
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
								<a
									class="act k-hit"
									href={reportHref(c)}
									title="Report this comment to a moderator"
								>
									<Icon name="flag" size={14} />Report
								</a>
							{/if}
							{#if canMod || c.mine}
								<button class="act k-hit mod" onclick={() => removeComment(c.id)}>
									<Icon name="x" size={14} />Delete
								</button>
								{#if canMod && !c.mine && c.authorId}
									<button class="act k-hit mod danger" onclick={() => banAuthor(c)}>
										<Icon name="alert" size={14} />Ban
									</button>
								{/if}
							{/if}
						</div>

						{#if replyingTo === c.id}
							<div class="reply-composer">
								<textarea
									aria-label={`Reply to ${c.name}`}
									name="reply"
									bind:value={replyDraft}
									placeholder={`Reply to ${c.name}…`}
									rows="2"
								></textarea>
								{#if replyMedia}
									<div class="media-preview">
										<img
											src={replyMedia.url}
											width={replyMedia.width}
											height={replyMedia.height}
											alt="Attachment preview"
										/>
										<button
											class="media-remove"
											onclick={() => (replyMedia = null)}
											aria-label="Remove image"
										>
											<Icon name="x" size={15} />
										</button>
									</div>
								{/if}
								<TurnstileWidget bind:this={replyCheck} />
								{#if replyError}<p class="post-error" role="alert">{replyError}</p>{/if}
								<div class="composer-foot">
									<label class="spoiler-toggle">
										<input type="checkbox" bind:checked={replySpoiler} />
										<span>Spoilers</span>
									</label>
									<div class="foot-actions">
										<button
											class="attach k-hit"
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
											class="post k-hit sm"
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
					{/if}
					{#if depth < MAX_INDENT}
						{#each tree.childrenOf(c.id) as child (child.id)}
							{@render node(child, depth + 1, c)}
						{/each}
					{/if}
				</div>
			</div>
			{#if depth >= MAX_INDENT}
				{#each tree.childrenOf(c.id) as child (child.id)}
					{@render node(child, depth + 1, c)}
				{/each}
			{/if}
		{/if}
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
		border: none;
		border-radius: 10px;
		padding: 12px 14px;
		color: var(--k-text-1);
		font: inherit;
		font-size: 14px;
		line-height: 1.5;
	}
	textarea:focus {
		outline: 2px solid var(--k-primary);
		outline-offset: 2px;
	}
	.post-error {
		color: var(--k-danger);
		font-size: 12.5px;
		margin: 8px 0 0;
	}
	/* On a phone the toggle and the buttons do not fit on one line: the row wraps
	   (buttons stay right-aligned) instead of squeezing each label onto two lines. */
	.composer-foot {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		justify-content: space-between;
		gap: 4px 12px;
		margin-top: 10px;
	}
	.spoiler-toggle {
		display: flex;
		align-items: center;
		gap: 7px;
		font-size: 13px;
		color: var(--k-text-dimmer);
		cursor: pointer;
		white-space: nowrap;
	}
	.foot-actions {
		display: flex;
		align-items: center;
		gap: 10px;
		margin-left: auto;
	}
	.attach {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 34px;
		height: 34px;
		border-radius: 8px;
		border: none;
		background: var(--k-surface);
		color: var(--k-text-dimmer);
		cursor: pointer;
	}
	.attach:hover:not(:disabled) {
		color: var(--k-primary);
		background: var(--k-surface-2);
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
		background: var(--k-surface);
		color: var(--k-text-1);
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
		white-space: nowrap;
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
		gap: 14px;
	}
	.c-empty {
		color: var(--k-text-faint);
		font-size: 14px;
		margin: 0;
	}
	.comment {
		min-width: 0;
	}
	/* Only the body grid adds indent; capped replies follow it at the same level. */
	.comment.reply {
		margin-top: 10px;
	}
	.comment-avatar {
		width: 28px;
		height: 28px;
	}
	.comment-avatar.deleted {
		background: var(--k-border-4);
	}
	.c-content {
		display: grid;
		grid-template-columns: 28px minmax(0, 1fr);
		column-gap: 10px;
	}
	.c-rail {
		position: relative;
		width: 28px;
		min-height: 44px;
		padding: 0;
		border: none;
		background: transparent;
		cursor: pointer;
	}
	.c-rail::before {
		content: '';
		position: absolute;
		top: 0;
		bottom: 0;
		left: 50%;
		width: 1px;
		background: var(--k-border-1);
		transition: background 0.15s;
	}
	.comment:hover > .c-content > .c-rail::before {
		background: var(--k-border-4);
	}
	.c-body {
		min-width: 0;
	}
	.c-meta {
		display: flex;
		align-items: center;
		gap: 6px;
		white-space: nowrap;
	}
	.c-meta.folded {
		opacity: 0.65;
	}
	.c-fold {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		flex: 0 0 20px;
		height: 24px;
		margin-left: auto;
		padding: 0;
		border: none;
		background: transparent;
		color: var(--k-text-faint);
		cursor: pointer;
	}
	.c-fold:hover {
		color: var(--k-text-1);
	}
	.c-name,
	.c-time,
	.c-relation,
	.c-hidden-count {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.c-name {
		flex-shrink: 0;
		max-width: 50%;
		font-weight: 700;
		font-size: 14px;
		color: var(--k-text-1);
	}
	.op,
	.mine {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		font-size: 10px;
		font-weight: 700;
		letter-spacing: 0.03em;
		padding: 2px 6px;
		border-radius: 5px;
	}
	.op {
		color: var(--k-ongoing);
		background: color-mix(in srgb, var(--k-ongoing) 14%, transparent);
	}
	.mine {
		color: var(--k-primary);
		background: var(--k-border-1);
	}
	.c-time,
	.c-relation,
	.c-hidden-count {
		font-size: 11.5px;
		color: var(--k-text-fainter);
	}
	.c-time::before {
		content: '· ';
	}
	/* In a narrow header the "replying to" hint gives way first; the name stays whole
	   as long as it can, and the badges, time and count never shrink. */
	.c-relation {
		flex-shrink: 100;
	}
	.op,
	.mine,
	.c-time,
	.c-hidden-count {
		flex-shrink: 0;
	}
	.c-text {
		margin: 4px 0 0;
		font-size: 14px;
		line-height: 1.55;
		color: var(--k-text-2);
	}
	.c-deleted {
		color: var(--k-text-fainter);
		font-style: italic;
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
		background: var(--k-surface);
	}
	/* Old uploads have no stored dimensions. Reserve a stable preview frame and
	   contain the full image; opening the attachment preserves its native size. */
	.c-media.legacy {
		display: block;
		width: min(360px, 100%);
		aspect-ratio: 4 / 3;
		border-radius: 12px;
		background: var(--k-surface);
	}
	.c-media.legacy img {
		width: 100%;
		height: 100%;
		max-height: none;
		object-fit: contain;
	}
	.spoiler-veil {
		display: inline-flex;
		align-items: center;
		gap: 7px;
		margin-top: 8px;
		padding: 8px 12px;
		border-radius: 8px;
		border: none;
		background: var(--k-surface);
		color: var(--k-text-dimmer);
		font-size: 13px;
		cursor: pointer;
	}
	/* Wraps: with the moderator actions the row is wider than a 320px phone. */
	.c-actions {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 16px;
		margin-top: 6px;
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
		font-size: 12.5px;
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
		color: var(--k-danger);
	}
	.mod {
		color: var(--k-text-dimmer);
	}
	.mod.danger,
	.mod.danger:hover {
		color: var(--k-danger);
	}
	.reply-composer {
		margin-top: 12px;
		padding: 12px;
		border-radius: 12px;
		background: var(--k-surface);
	}
	@media (max-width: 600px) {
		.c-meta {
			gap: 4px;
		}
		.comment-avatar {
			width: 24px;
			height: 24px;
		}
		.comment-avatar :global(.avatar-img),
		.comment-avatar :global(.avatar-fallback) {
			width: 24px !important;
			height: 24px !important;
			font-size: 11px !important;
		}
		.c-content {
			grid-template-columns: 24px minmax(0, 1fr);
			column-gap: 6px;
		}
		.c-rail {
			width: 24px;
		}
	}
	/* Touch: the actions keep their size (each carries the shared 44px target); a
	   wrapped second row just needs room so the two rows' targets do not overlap. */
	@media (pointer: coarse) {
		.c-actions {
			row-gap: 20px;
		}
		.spoiler-toggle {
			min-height: 44px;
		}
	}
</style>
