/** Local-only visual fixtures. Run: node e2e/fixtures/visual-server.mjs
 * PUBLIC_KOMIKA_BACKEND=on PUBLIC_KOMIKA_API=http://127.0.0.1:4101/api/graphql pnpm --filter @komika/reader dev
 * This deliberately implements read-only fixture responses, never production writes.
 */
import { createServer } from 'node:http';
const port = Number(process.env.VISUAL_FIXTURE_PORT || 4101);
const base = `http://127.0.0.1:${port}`;
const at = '2026-10-03T00:00:00Z';
const scan = {
	avgIntervalHours: 12,
	overrideIntervalHours: null,
	pollEveryMinutes: 30,
	paused: false,
	statusOverride: null,
	pausedOverride: null,
	pollEveryMinutesOverride: null,
	lastScannedAt: at,
	nextScanAt: at,
};
const series = [
	'Aster Crossing',
	'The Last Observatory',
	'Rain Over Harbor',
	'Paper Lanterns',
	'Midnight Orchard',
	'Northbound',
].map((title, i) => ({
	id: `w_fixture_${i}`,
	workId: `w_fixture_${i}`,
	title,
	altTitles: [],
	author: 'Fixture Author',
	artist: 'Fixture Artist',
	description:
		'An illustrated story about finding your place in a changing world. Local visual fixture content.',
	genres: ['Adventure', 'Fantasy'],
	type: ['MANGA', 'MANHWA', 'MANHUA'][i % 3],
	status: i === 3 ? 'COMPLETED' : 'ONGOING',
	coverUrl: `${base}/cover.svg?i=${i}`,
	sourceId: 'fixture',
	chapterCount: 12 + i,
	inLibrary: true,
	libraryStatus: null,
	isFavorite: i === 0,
	isNsfw: false,
	rating: { average: i === 5 ? 0 : 8.2 - i / 10, count: i === 5 ? 0 : 28, distribution: [] },
	scan,
	createdAt: at,
	updatedAt: at,
	latestChapterAt: at,
	latestChapter: String(12 + i),
	detectedAt: at,
	views: { total: 1234, last7d: 321, last24h: 64 },
	localizedDescriptions: [],
	credits: [],
	covers: [],
}));
for (const item of series)
	item.metadata = {
		year: 2021,
		originalLanguage: 'ja',
		demographic: 'shounen',
		contentRating: 'safe',
		tags: [
			{ id: 'tag-genre', name: 'Adventure', group: 'genre' },
			{ id: 'tag-theme', name: 'Coming of Age', group: 'theme' },
			{ id: null, name: 'Long Strip', group: 'format' },
		],
		trackers: [
			{
				provider: 'mangadex',
				externalId: '436b9823-2d06-4477-af20-c526c1fd689b',
				url: 'https://mangadex.org/title/436b9823-2d06-4477-af20-c526c1fd689b',
			},
			{ provider: 'al', externalId: '123', url: 'https://anilist.co/manga/123' },
		],
	};
const chapters = Array.from({ length: 12 }, (_, i) => ({
	id: `fixture-chapter-${i}`,
	seriesId: series[0].id,
	number: i + 1,
	title: `Chapter ${i + 1}`,
	scanlator: 'Fixture Studio',
	uploadedAt: at,
	foundAt: at,
	read: i < 3,
	lastPageRead: 0,
	pageCount: 4,
	externalUrl: null,
	readableAt: at,
	language: 'en',
}));
const source = {
	id: 'fixture',
	sourceId: 'fixture',
	sourceType: 'mangadex',
	sourceKey: 'fixture',
	sourceSeriesId: 'fixture',
	workId: series[0].id,
	name: 'Fixture Studio',
	sourceName: 'Fixture Studio',
	lang: 'en',
	isNsfw: false,
	iconUrl: null,
	pkgName: 'fixture',
	chapterCount: 12,
	suwayomiMangaId: null,
	isPrimary: true,
};
const user = {
	id: 'fixture-user',
	username: 'reader',
	email: 'fixture@example.invalid',
	avatarUrl: null,
	isAdmin: true,
	isBanned: false,
	createdAt: at,
	showNsfw: false,
	readCount: 3,
	libraryCount: 6,
};
const page = (items) => ({ items, page: 1, hasNextPage: false, total: items.length });
export function fixtureData() {
	const feeds = ['TRENDING', 'POPULAR', 'RECENTLY_ADDED'].map((kind) => ({
		kind,
		title:
			kind === 'RECENTLY_ADDED'
				? 'Latest Added'
				: kind === 'TRENDING'
					? 'Trending Today'
					: 'Popular',
		genre: null,
		items: series,
	}));
	const updates = series.map((s) => ({
		id: s.id,
		workId: s.id,
		title: s.title,
		coverUrl: s.coverUrl,
		type: s.type,
		status: s.status,
		latestChapter: s.latestChapter,
		latestChapterTitle: null,
		chapterCount: s.chapterCount,
		releasedAt: at,
		announcedAt: at,
		detectedAt: at,
		rating: s.rating.count ? s.rating.average : null,
		isNsfw: false,
	}));
	return {
		discovery: feeds,
		search: page(series),
		searchForMerge: series,
		series: series[0],
		canonicalSeries: series[0],
		library: series,
		libraryProgress: series.map((s) => ({
			id: s.id,
			read: 3,
			total: s.chapterCount,
			lastReadAt: at,
		})),
		updates: page(series),
		updatesFeed: page(updates),
		canonicalUpdates: [],
		chapters,
		canonicalChapters: chapters,
		pages: Array.from({ length: 4 }, (_, index) => ({
			index,
			sourceUrl: `${base}/page.svg`,
			url: `${base}/page.svg`,
			width: 800,
			height: 1200,
		})),
		canonicalPages: Array.from({ length: 4 }, (_, index) => ({
			index,
			sourceUrl: `${base}/page.svg`,
			url: `${base}/page.svg`,
			width: 800,
			height: 1200,
		})),
		seriesPage: {
			workId: series[0].id,
			series: series[0],
			spineChapters: chapters,
			sources: [],
			aggregated: [],
		},
		workSources: [source],
		workSourcesWithChapters: [],
		genreFacets: [
			{ genre: 'Adventure', count: 6 },
			{ genre: 'Fantasy', count: 6 },
		],
		me: user,
		session: { token: 'visual-fixture', user, expiresAt: '2099-01-01T00:00:00Z' },
		users: page([user]),
		sources: [source],
		scanStatus: { librarySize: 6, overdueCount: 1, lastTickAt: at, nextDueAt: at },
		scanHealth: {
			lastProgressAt: at,
			lastTickAt: at,
			scanned: 6,
			succeeded: 6,
			failed: 0,
			stalled: false,
		},
		extensionRepos: [],
		extensions: [],
		mergeQueue: [],
		sourceIngestJobs: [],
		coverIssues: page([]),
		reports: page([]),
		bugReports: page([]),
		notifications: [],
		unreadNotificationCount: 0,
		myActivity: [],
		myReview: null,
		reviews: page([]),
		comments: page([]),
		myPreferredSources: [],
		sourceScanHealth: [],
		workReview: null,
	};
}
const server = createServer(async (req, res) => {
	res.setHeader('Access-Control-Allow-Origin', '*');
	res.setHeader('Access-Control-Allow-Headers', 'authorization, content-type');
	if (req.method === 'OPTIONS') {
		res.writeHead(204);
		res.end();
		return;
	}
	if (req.url?.startsWith('/cover.svg') || req.url?.startsWith('/page.svg')) {
		const i = Number(new URL(req.url, base).searchParams.get('i') || 0);
		const colors = ['#395953', '#745748', '#3c5068', '#7a643c', '#59466c', '#5a6351'];
		res.setHeader('Content-Type', 'image/svg+xml');
		res.end(
			`<svg xmlns="http://www.w3.org/2000/svg" width="800" height="1200"><rect width="800" height="1200" fill="${colors[i % 6]}"/><circle cx="400" cy="460" r="260" fill="#e8dcc5" opacity=".18"/><path d="M0 950L420 340L800 1100" fill="none" stroke="#e8dcc5" stroke-width="30" opacity=".25"/><text x="70" y="1040" fill="#f5eddf" font-family="serif" font-size="65">${series[i % 6].title}</text><text x="70" y="1120" fill="#f5eddf" font-family="sans-serif" font-size="22">LOCAL VISUAL FIXTURE</text></svg>`,
		);
		return;
	}
	let raw = '';
	for await (const chunk of req) raw += chunk;
	let query = '';
	try {
		query = JSON.parse(raw).query || '';
	} catch {}
	res.setHeader('Content-Type', 'application/json');
	if (/\bmutation\b/.test(query)) {
		res.end(JSON.stringify({ errors: [{ message: 'Visual fixture endpoint is read-only' }] }));
		return;
	}
	res.end(JSON.stringify({ data: fixtureData() }));
});
server.listen(port, '127.0.0.1', () =>
	console.log(`Read-only visual fixture GraphQL: ${base}/api/graphql`),
);
