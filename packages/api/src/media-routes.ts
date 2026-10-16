import type { MediaRoute, Page } from '@komika/types';

/**
 * Typed media routes (M5): where a hosted web reader loads exact media objects
 * (`/media/v1/o/{copy}/{sha256}`) from.
 *
 * The server lists its routes (`mediaRoutes`); the reader trusts only the ones on its own
 * build-time list (`PUBLIC_KOMIKA_MEDIA_HOSTS`), plus the API origin as the owner. A
 * server cannot add a host to a hosted reader. Every media request goes out with no
 * credentials and no `Authorization`: an `<img>` sends no token, and probes use
 * `credentials: 'omit'` with no headers.
 */

/** A route the reader trusts: an absolute origin, never a path. */
export interface TrustedMediaRoute {
	id: string;
	origin: string;
	kind: 'OWNER' | 'REGIONAL';
	region: string | null;
}

/**
 * An exact media object on one route. A typed value, not a string, so it cannot be
 * handed to an image provider's `resolve()` and Worker-wrapped by mistake: the only way
 * to a loadable URL is {@link MediaRouter.href}.
 */
export interface MediaUrl {
	readonly kind: 'media';
	readonly routeId: string;
	readonly path: string;
}

/** A page as the reader shows it: the URL to load and, when it is an exact object, where
 *  that URL came from (for the owner fallback). */
export interface RoutedPage {
	url: string;
	media: MediaUrl | null;
}

/** Exact object paths only. Anything else in `mediaPath` is ignored. */
const OBJECT_PATH = /^\/media\/v1\/o\/[A-Za-z0-9_-]+\/[0-9a-f]{64}$/;

/** The probe target, `no-store` and `ACAO: *` on every route. */
export const MEDIA_READY_PATH = '/media/v1/ready';
export const PROBE_TIMEOUT_MS = 1500;
/** At most one probe per route in this window. */
export const PROBE_INTERVAL_MS = 60_000;
/** At most this many probe rounds per browser session. */
export const PROBE_ROUNDS_PER_SESSION = 3;
/** A switch needs a route faster by more than this share AND {@link SWITCH_MIN_MS}... */
export const SWITCH_MIN_SHARE = 0.3;
export const SWITCH_MIN_MS = 50;
/** ...in this many rounds in a row. */
export const SWITCH_ROUNDS = 2;

const STATE_KEY = 'komiq.mediaRoute';

/** `scheme://host[:port]` of an absolute http(s) URL with an exact host and no credentials, or null. */
export function mediaOrigin(value: string): string | null {
	let url: URL;
	try {
		url = new URL(value.trim());
	} catch {
		return null;
	}
	if (url.protocol !== 'https:' && url.protocol !== 'http:') return null;
	if (url.hostname.includes('*')) return null;
	if (url.username || url.password) return null;
	if (url.pathname !== '/' || url.search || url.hash) return null;
	return url.origin;
}

/**
 * The build-time allowlist: comma- or space-separated origins. An entry that is not a
 * bare http(s) origin is an error, so a typo fails the build instead of quietly
 * dropping a route.
 */
export function parseMediaHosts(raw: string | undefined): string[] {
	const out: string[] = [];
	for (const entry of (raw ?? '').split(/[\s,]+/).filter(Boolean)) {
		const origin = mediaOrigin(entry);
		if (!origin) {
			throw new Error(
				`PUBLIC_KOMIKA_MEDIA_HOSTS: "${entry}" is not an http(s) origin (scheme://host[:port])`,
			);
		}
		if (!out.includes(origin)) out.push(origin);
	}
	return out;
}

/**
 * The routes this reader may use: the server's list intersected with the build-time
 * allowlist, plus the API origin as the owner. The owner always exists and comes first.
 * Its origin is the server's owner base when that is the API origin or on the allowlist,
 * else the API origin. A regional route is kept only when its origin is on the allowlist.
 */
export function trustedMediaRoutes(
	server: readonly MediaRoute[] | null | undefined,
	allowlist: readonly string[],
	apiOrigin: string,
): TrustedMediaRoute[] {
	const api = mediaOrigin(apiOrigin) ?? apiOrigin.replace(/\/$/, '');
	const allowed = (origin: string | null): origin is string =>
		!!origin && (origin === api || allowlist.includes(origin));
	const listed = server ?? [];
	const ownerEntry = listed.find((r) => r.kind === 'OWNER');
	const ownerBase = ownerEntry?.base ? mediaOrigin(ownerEntry.base) : null;
	const owner: TrustedMediaRoute = {
		id: ownerEntry?.id || 'owner',
		origin: allowed(ownerBase) ? ownerBase : api,
		kind: 'OWNER',
		region: ownerEntry?.region ?? null,
	};
	const out = [owner];
	for (const route of listed) {
		if (route.kind !== 'REGIONAL' || !route.id) continue;
		const origin = route.base ? mediaOrigin(route.base) : null;
		if (!origin || origin === owner.origin || !allowlist.includes(origin)) continue;
		if (out.some((r) => r.id === route.id || r.origin === origin)) continue;
		out.push({ id: route.id, origin, kind: 'REGIONAL', region: route.region ?? null });
	}
	return out;
}

/** What survives a reload in the same tab (`sessionStorage`). */
interface RouterState {
	/** The sticky choice; null = none yet (the owner serves until a round picks). */
	choice: string | null;
	rounds: number;
	/** The route that beat the choice in the last round, and how many rounds in a row. */
	streak: { id: string; count: number } | null;
	/** Routes whose object failed this session. Never the owner. */
	down: string[];
	/** Last probe time per route, `Date.now()` ms. */
	probedAt: Record<string, number>;
}

type StateStorage = Pick<Storage, 'getItem' | 'setItem'>;

export interface MediaRouterOptions {
	routes: readonly TrustedMediaRoute[];
	fetch?: typeof fetch;
	/** `sessionStorage` in the browser; null keeps state in memory only. */
	storage?: StateStorage | null;
	now?: () => number;
}

/**
 * Picks the route exact objects load from. The choice holds for the session; probes
 * (`GET {origin}/media/v1/ready`, no credentials, 1.5 s deadline) run only when there is
 * more than one route, at most once per route per 60 s and three rounds per session, and
 * never download an image. A switch away from the held choice needs another route faster
 * by more than 30 % and 50 ms in two rounds in a row. A regional object that fails is
 * retried once on the owner and its route is marked down for the session.
 */
export class MediaRouter {
	readonly routes: readonly TrustedMediaRoute[];
	private readonly doFetch: typeof fetch;
	private readonly storage: StateStorage | null;
	private readonly now: () => number;
	private state: RouterState;
	private round: Promise<void> | null = null;

	constructor(opts: MediaRouterOptions) {
		if (!opts.routes.length || opts.routes[0].kind !== 'OWNER') {
			throw new Error('MediaRouter needs the owner route first');
		}
		this.routes = opts.routes;
		this.doFetch = opts.fetch ?? ((input, init) => fetch(input, init));
		this.storage = opts.storage ?? null;
		this.now = opts.now ?? Date.now;
		this.state = this.load();
	}

	get owner(): TrustedMediaRoute {
		return this.routes[0];
	}

	/** The route objects load from now. */
	current(): TrustedMediaRoute {
		const { choice, down } = this.state;
		return (choice && !down.includes(choice) && this.route(choice)) || this.owner;
	}

	/** The page's exact object on the current route, or null (load `sourceUrl`). */
	pageMedia(page: Page): MediaUrl | null {
		const path = page.mediaPath;
		if (!path || !OBJECT_PATH.test(path)) return null;
		return { kind: 'media', routeId: this.current().id, path };
	}

	/** The loadable URL of an exact object. */
	href(media: MediaUrl): string {
		return (this.route(media.routeId) ?? this.owner).origin + media.path;
	}

	/**
	 * After `media` failed to load: the same path on the owner, marking its route down
	 * for the session, or null when it already was on the owner (the caller then refreshes
	 * the chapter's pages once).
	 */
	fallback(media: MediaUrl): MediaUrl | null {
		if (media.routeId === this.owner.id || !this.route(media.routeId)) return null;
		this.markDown(media.routeId);
		void this.probe();
		return { kind: 'media', routeId: this.owner.id, path: media.path };
	}

	/** One probe round, within the limits; resolves when it is done. */
	probe(): Promise<void> {
		if (this.round) return this.round;
		if (this.routes.length < 2 || this.state.rounds >= PROBE_ROUNDS_PER_SESSION) {
			return Promise.resolve();
		}
		const at = this.now();
		const due = this.routes.filter(
			(r) =>
				!this.state.down.includes(r.id) &&
				!(at - (this.state.probedAt[r.id] ?? -Infinity) < PROBE_INTERVAL_MS),
		);
		if (!due.length) return Promise.resolve();
		this.state.rounds++;
		for (const r of due) this.state.probedAt[r.id] = at;
		this.save();
		this.round = Promise.all(due.map((r) => this.time(r))).then((times) => {
			this.round = null;
			this.decide(new Map(due.map((r, i) => [r.id, times[i]])));
		});
		return this.round;
	}

	/** Round-trip ms to the route's ready path, or null on failure or after the deadline. */
	private async time(route: TrustedMediaRoute): Promise<number | null> {
		const controller = new AbortController();
		const timer = setTimeout(() => controller.abort(), PROBE_TIMEOUT_MS);
		const start = this.now();
		try {
			const res = await this.doFetch(route.origin + MEDIA_READY_PATH, {
				method: 'GET',
				credentials: 'omit',
				cache: 'no-store',
				referrerPolicy: 'no-referrer',
				signal: controller.signal,
			});
			// The body is empty; a body that stalls past the deadline fails the probe.
			await res.arrayBuffer();
			return res.ok && !controller.signal.aborted ? this.now() - start : null;
		} catch {
			return null;
		} finally {
			clearTimeout(timer);
		}
	}

	private decide(times: Map<string, number | null>): void {
		const answered = [...times]
			.filter((e): e is [string, number] => e[1] !== null && !this.state.down.includes(e[0]))
			.sort((a, b) => a[1] - b[1] || this.order(a[0]) - this.order(b[0]));
		const held = this.state.choice && !this.state.down.includes(this.state.choice);
		if (!held) {
			// Nothing held yet (first round, or the choice went down): take the fastest.
			this.state.choice = answered[0]?.[0] ?? null;
			this.state.streak = null;
		} else if (times.has(this.state.choice!)) {
			const cur = times.get(this.state.choice!) ?? Infinity;
			const best = answered.find(([id]) => id !== this.state.choice);
			const faster =
				!!best && cur - best[1] > SWITCH_MIN_MS && best[1] < cur * (1 - SWITCH_MIN_SHARE);
			if (!faster) {
				this.state.streak = null;
			} else {
				const count = this.state.streak?.id === best[0] ? this.state.streak.count + 1 : 1;
				if (count >= SWITCH_ROUNDS) {
					this.state.choice = best[0];
					this.state.streak = null;
				} else {
					this.state.streak = { id: best[0], count };
				}
			}
		}
		this.save();
	}

	private markDown(id: string): void {
		if (id === this.owner.id || this.state.down.includes(id)) return;
		this.state.down.push(id);
		if (this.state.choice === id) this.state.choice = null;
		this.state.streak = null;
		this.save();
	}

	private route(id: string): TrustedMediaRoute | undefined {
		return this.routes.find((r) => r.id === id);
	}

	private order(id: string): number {
		return this.routes.findIndex((r) => r.id === id);
	}

	private load(): RouterState {
		// No prototype: a route id such as `__proto__` must be an ordinary key.
		const fresh: RouterState = {
			choice: null,
			rounds: 0,
			streak: null,
			down: [],
			probedAt: Object.create(null),
		};
		try {
			const raw = this.storage?.getItem(STATE_KEY);
			if (!raw) return fresh;
			const s = JSON.parse(raw) as Partial<RouterState>;
			const known = (id: unknown): id is string => typeof id === 'string' && !!this.route(id);
			return {
				choice: known(s.choice) ? s.choice : null,
				rounds: Number.isInteger(s.rounds) ? Math.max(0, s.rounds as number) : 0,
				streak:
					s.streak && known(s.streak.id) && Number.isInteger(s.streak.count)
						? { id: s.streak.id, count: s.streak.count }
						: null,
				down: Array.isArray(s.down) ? s.down.filter((id) => known(id) && id !== this.owner.id) : [],
				probedAt: Object.assign(
					Object.create(null),
					Object.fromEntries(
						Object.entries(s.probedAt ?? {}).filter(
							([id, at]) => known(id) && typeof at === 'number',
						),
					),
				),
			};
		} catch {
			return fresh;
		}
	}

	private save(): void {
		try {
			this.storage?.setItem(STATE_KEY, JSON.stringify(this.state));
		} catch {
			/* private mode: the choice lasts for this page only */
		}
	}
}

/**
 * Recovery for one open chapter's exact-object pages: one owner fallback per object, then
 * ONE fresh-transport refresh of the chapter's pages (`refresh`, which must skip every
 * HTTP cache), then nothing (the reader's retry button). Each page is replaced by the
 * refresh at most once, so a failing page can never loop.
 */
export class ChapterMediaRecovery {
	private readonly pages: RoutedPage[];
	private refreshed: Promise<RoutedPage[] | null> | null = null;
	private readonly replaced = new Set<number>();

	constructor(
		private readonly router: MediaRouter,
		pages: readonly RoutedPage[],
		private readonly refresh: () => Promise<RoutedPage[]>,
	) {
		this.pages = pages.map((p) => ({ ...p }));
	}

	/**
	 * Page `index` failed to load `url`. Returns the pages to load again (index -> new
	 * page): the owner copy of a regional object, or, after the owner failed too, every
	 * page the one refresh changed. A failure of a URL the page has already moved past
	 * returns the page's current URL. Empty = show the retry button.
	 */
	async recover(index: number, url: string): Promise<Map<number, RoutedPage>> {
		const failed = this.pages[index];
		const out = new Map<number, RoutedPage>();
		if (!failed) return out;
		if (failed.url !== url) return out.set(index, failed);
		if (!failed.media) return out;
		const owner = this.router.fallback(failed.media);
		if (owner) {
			const next = { url: this.router.href(owner), media: owner };
			this.pages[index] = next;
			out.set(index, next);
			return out;
		}
		this.refreshed ??= this.refresh().catch(() => null);
		const fresh = await this.refreshed;
		// A different page count is a different chapter layout; remounting by index would
		// show the wrong pages.
		if (!fresh || fresh.length !== this.pages.length) return out;
		fresh.forEach((page, i) => {
			if (!page.url || this.replaced.has(i) || page.url === this.pages[i].url) return;
			this.replaced.add(i);
			this.pages[i] = page;
			out.set(i, page);
		});
		// Another failure applied the refresh first: this page has a new URL already.
		if (!out.has(index) && this.pages[index].url !== url) out.set(index, this.pages[index]);
		return out;
	}
}
