/**
 * Notifications repository — the inbound bell feed (replies to your comments + like
 * milestones). Live only against the unified Komika backend when signed in; otherwise
 * everything is empty (the app stays renderable, same contract as `social-repo`).
 */
import type { Notification } from '@komika/types';
import { backend } from '$lib/context';
import { config } from '$lib/config';
import { auth } from '$lib/auth.svelte';

export function notificationsLive(): boolean {
	return (
		config.backendEnabled &&
		config.backendKind === 'komika' &&
		!!auth.user &&
		!!backend.notifications
	);
}

export interface NotificationView {
	id: string;
	/** Rendered one-line message. */
	text: string;
	avatarUrl?: string | null;
	/** Actor name for the avatar fallback initial (empty for aggregate milestones). */
	actorName: string;
	/** A snippet of the viewer's own comment the event is about, if any. */
	excerpt: string | null;
	/** Deep-link to the thread, or null when it can't be resolved (non-navigating). */
	href: string | null;
	time: string;
	read: boolean;
}

/** Coarse "N ago" for the feed (mirrors source.ts' relTimeAgo). */
function relTime(iso: string): string {
	const then = Date.parse(iso);
	if (Number.isNaN(then)) return '';
	const now = typeof Date !== 'undefined' && Date.now ? Date.now() : then;
	const mins = Math.max(0, Math.round((now - then) / 60000));
	if (mins < 1) return 'just now';
	if (mins < 60) return `${mins}m ago`;
	const hrs = Math.round(mins / 60);
	if (hrs < 24) return `${hrs}h ago`;
	return `${Math.round(hrs / 24)}d ago`;
}

function messageFor(n: Notification): string {
	const who = n.actor?.username ?? 'Someone';
	if (n.kind === 'reply') return `${who} replied to your comment`;
	if (n.kind === 'like_milestone') {
		return n.count && n.count > 1
			? `Your comment reached ${n.count} likes`
			: 'Someone liked your comment';
	}
	return 'New activity on your comment';
}

/** Deep-link to the thread: a series discussion opens the series page; a chapter thread
 *  opens the reader at that chapter (the server resolves the owning `seriesId`). Null
 *  when it can't be resolved (non-navigating).
 *
 *  Deliberately carries NO `src`: a notification is raised for whichever source the
 *  scanner saw the chapter on, which is often not the source the reader defaults to,
 *  and `Notification` has no field naming it. That mismatch used to render a blank
 *  reader; `getReaderChapter` now locates the chapter's owning source itself (see
 *  `findChapterOwner`), which also repairs every link already sent. Adding `src` here
 *  would need a server schema change and would buy nothing the reader can't derive. */
function hrefFor(n: Notification): string | null {
	if (n.targetType === 'series' && n.targetId) return `/series/${n.targetId}`;
	if (n.targetType === 'chapter' && n.seriesId && n.targetId) {
		return `/read/${n.seriesId}?ch=${n.targetId}`;
	}
	return null;
}

function toView(n: Notification): NotificationView {
	return {
		id: n.id,
		text: messageFor(n),
		// Raw url — the Avatar component resolves it against the API origin.
		avatarUrl: n.actor?.avatarUrl ?? null,
		actorName: n.actor?.username ?? '',
		excerpt: n.commentExcerpt,
		href: hrefFor(n),
		time: relTime(n.createdAt),
		read: n.read,
	};
}

export async function getNotifications(): Promise<NotificationView[]> {
	if (!notificationsLive() || !backend.notifications) return [];
	try {
		const list = await backend.notifications();
		return list.map(toView);
	} catch {
		return [];
	}
}

export async function getUnreadCount(): Promise<number> {
	if (!notificationsLive() || !backend.unreadNotificationCount) return 0;
	try {
		return await backend.unreadNotificationCount();
	} catch {
		return 0;
	}
}

/** Mark all notifications read; best-effort (returns whether it succeeded). */
export async function markAllRead(): Promise<boolean> {
	if (!notificationsLive() || !backend.markNotificationsRead) return false;
	try {
		await backend.markNotificationsRead();
		return true;
	} catch {
		return false;
	}
}
