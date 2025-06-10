/**
 * GraphQL operation documents for Komika's unified backend.
 *
 * These are domain-shaped (they mirror `@komika/types`), so the client needs no
 * mapping layer — the server is responsible for federating Suwayomi (catalog,
 * chapters, pages, library, progress) and adding the social layer. The contract
 * these operations expect is documented in `schema/komika.graphql`.
 */

/**
 * Fields the client selects OPPORTUNISTICALLY.
 *
 * They are newer than some deployed servers, and GraphQL rejects the whole
 * document on an unknown field — so a reader shipped ahead of the API would fail
 * every query, not just degrade one label. {@link GraphQLBackend} watches for
 * `Unknown field "…"`, strips the offender from the document and retries once,
 * then omits it for the rest of the session. Every field listed here MUST
 * therefore be optional in `@komika/types` and have a caller-side fallback.
 *
 * The stripper is LINE-BASED, so each name here must additionally be selected:
 *   - on its OWN LINE (`id detectedAt` on one line is not removed), and
 *   - with NO alias, arguments or sub-selection (`when: detectedAt`,
 *     `detectedAt(tz:…)`, `detectedAt { … }` are not removed), and
 *   - never as the ONLY selection in its set — removing it would leave `{ }`.
 * Violating any of these is not corrupting (`stripUnsupported` returns the document
 * unchanged rather than emit invalid GraphQL) but it silently disables the fallback,
 * so the query just fails against an older server.
 */
export const OPTIONAL_SERIES_FIELDS = ['detectedAt', 'workId'] as const;

/**
 * Query ARGUMENTS the client passes OPPORTUNISTICALLY — same deploy-ordering hazard
 * as {@link OPTIONAL_SERIES_FIELDS}, different error: an argument the server doesn't
 * know fails validation with `Unknown argument "…"`, which likewise rejects the whole
 * document. The reader never sets `includeNsfw`, but it ships the same documents, so
 * a reader deployed ahead of the API would break every browse/search screen on an
 * argument it doesn't even use. {@link GraphQLBackend} strips it and retries once.
 *
 * Unlike the field stripper this one is TOKEN-based, not line-based: it removes the
 * `$name: Type` declaration and every `name: $name` use wherever they sit, so
 * `canonicalUpdates(page: $page, includeNsfw: $includeNsfw)` on one line is fine (the
 * leftover comma is an ignored token in GraphQL, and an argument list left completely
 * empty is collapsed). The constraints that DO apply: each name here must be passed
 * as a variable of the SAME name (a literal `includeNsfw: true`, or a renamed
 * `includeNsfw: $wantNsfw`, is not removed), and must be safe to omit — omitting it
 * just falls back to the server's default behaviour.
 */
export const OPTIONAL_ARGUMENTS = ['includeNsfw'] as const;

const SERIES_FIELDS = /* GraphQL */ `
	fragment SeriesFields on Series {
		id
		title
		altTitles
		author
		artist
		description
		genres
		type
		status
		coverUrl
		sourceId
		chapterCount
		isMarked
		isNsfw
		rating {
			average
			count
			distribution
		}
		scan {
			avgIntervalHours
			overrideIntervalHours
			pollEveryMinutes
			paused
			statusOverride
			pausedOverride
			pollEveryMinutesOverride
			lastScannedAt
			nextScanAt
		}
		createdAt
		updatedAt
		# Chapter recency. \`updatedAt\` is a metadata touch that moves on every poll,
		# so it can never be the "· 4h" label; \`latestChapterAt\` is the real publish
		# time and \`detectedAt\` is when we found it. Keep \`detectedAt\` on its own
		# line — the unknown-field fallback strips it by line (OPTIONAL_SERIES_FIELDS).
		latestChapterAt
		detectedAt
	}
`;

const CHAPTER_FIELDS = /* GraphQL */ `
	fragment ChapterFields on Chapter {
		id
		seriesId
		number
		title
		pageCount
		uploadedAt
		scanlator
		read
		lastPageRead
		bookmarked
		isDownloaded
	}
`;

const USER_REF = /* GraphQL */ `
	fragment UserRefFields on UserRef {
		id
		username
		avatarUrl
	}
`;

// ---- catalog ---------------------------------------------------------------

export const DISCOVERY = /* GraphQL */ `
	${SERIES_FIELDS}
	query Discovery {
		discovery {
			kind
			title
			genre
			items {
				...SeriesFields
			}
		}
	}
`;

export const UPDATES = /* GraphQL */ `
	${SERIES_FIELDS}
	query Updates($page: Int) {
		updates(page: $page) {
			items {
				...SeriesFields
			}
			page
			hasNextPage
			total
		}
	}
`;

// Catalogue search AND Browse. `types`/`status`/`sort`/`contentRating` are BROWSE
// arguments (the empty-query path); the server ignores them for a text query, which
// ranks by FTS relevance.
//
// These four are HARD requirements of the deployed API, deliberately NOT in
// OPTIONAL_ARGUMENTS: an unknown argument fails GraphQL validation and takes the whole
// document with it, so a reader shipped ahead of the server would break Browse for
// everyone rather than degrade it. Unlike `updatesFeed` — which is a whole optional
// method behind `backend.updatesFeed?.()` with a two-feed fallback — Browse has NO
// fallback path: there is exactly one `search` and every screen that browses goes
// through it. This couples the reader to a server that knows these arguments. Deploy
// the server first. (Listing them in OPTIONAL_ARGUMENTS would be worse than useless:
// stripping `sort` silently returns TRENDING under a "Top rated" chip, i.e. a lie
// instead of an error.)
export const SEARCH = /* GraphQL */ `
	${SERIES_FIELDS}
	query Search(
		$query: String!
		$page: Int
		$genres: [String!]
		$minRating: Float
		$maxRating: Float
		$types: [ComicType!]
		$status: SeriesStatus
		$sort: BrowseSort
		$contentRating: ContentRatingFilter
		$includeNsfw: Boolean
		$hasChapters: Boolean
	) {
		search(
			query: $query
			page: $page
			genres: $genres
			minRating: $minRating
			maxRating: $maxRating
			types: $types
			status: $status
			sort: $sort
			contentRating: $contentRating
			includeNsfw: $includeNsfw
			hasChapters: $hasChapters
		) {
			items {
				...SeriesFields
			}
			page
			hasNextPage
			total
		}
	}
`;

// The full genre/tag facet set across the persisted catalogue (S4), most-common
// first with counts — drives the search genre multi-select.
export const GENRE_FACETS = /* GraphQL */ `
	query GenreFacets {
		genreFacets {
			genre
			count
		}
	}
`;

// Multi-source aggregated chapters for a work (S2): one row per chapter number
// across ALL a work's sources, with per-source availability for reading.
export const AGGREGATED_CHAPTERS = /* GraphQL */ `
	query AggregatedChapters($workId: ID!) {
		aggregatedChapters(workId: $workId) {
			number
			title
			sources {
				sourceType
				sourceId
				suwayomiMangaId
				chapterId
				scanlator
			}
		}
	}
`;

export const SERIES = /* GraphQL */ `
	${SERIES_FIELDS}
	query Series($id: ID!) {
		series(id: $id) {
			...SeriesFields
			libraryStatus
			isFavorite
			# The canonical work behind a numeric Suwayomi id — how the reader finds the
			# work whose SIBLING sources belong in the source picker. Selected here and
			# nowhere else: it is a resolver field (one query per series), so putting it
			# on SeriesFields would make every browse card pay for it. Keep it on its own
			# line — the unknown-field fallback strips it by line (OPTIONAL_SERIES_FIELDS).
			workId
			views {
				total
				last7d
				last24h
			}
		}
	}
`;

// Federated multi-extension search (S3): fans out to every installed source,
// dedupes to one canonical work per series, and returns each work with its
// per-source translator list. User-facing; NSFW-gated by the viewer's posture.
export const SEARCH_ALL_SOURCES = /* GraphQL */ `
	${SERIES_FIELDS}
	query SearchAllSources($query: String!, $page: Int) {
		searchAllSources(query: $query, page: $page) {
			items {
				series {
					...SeriesFields
				}
				translators {
					sourceType
					sourceId
					sourceName
					lang
					suwayomiMangaId
					extensionPkgName
					extensionIconUrl
				}
			}
			page
			hasNextPage
			sourcesQueried
		}
	}
`;

export const CHAPTERS = /* GraphQL */ `
	${CHAPTER_FIELDS}
	query Chapters($seriesId: ID!) {
		chapters(seriesId: $seriesId) {
			...ChapterFields
		}
	}
`;

export const PAGES = /* GraphQL */ `
	query Pages($chapterId: ID!) {
		pages(chapterId: $chapterId) {
			index
			sourceUrl
			width
			height
		}
	}
`;

// ---- library & progress ----------------------------------------------------

export const LIBRARY = /* GraphQL */ `
	${SERIES_FIELDS}
	query Library {
		library {
			...SeriesFields
			libraryStatus
			isFavorite
		}
	}
`;

// Batched per-series read progress for the whole library (one query instead of a
// `chapters()` fan-out per series) — shelves the Library/Profile screens fast.
export const LIBRARY_PROGRESS = /* GraphQL */ `
	query LibraryProgress {
		libraryProgress {
			id
			read
			total
		}
	}
`;

export const MARK = /* GraphQL */ `
	${SERIES_FIELDS}
	mutation Mark($seriesId: ID!, $marked: Boolean!) {
		mark(seriesId: $seriesId, marked: $marked) {
			...SeriesFields
			libraryStatus
			isFavorite
		}
	}
`;

export const SET_LIBRARY_STATUS = /* GraphQL */ `
	${SERIES_FIELDS}
	mutation SetLibraryStatus($seriesId: ID!, $status: String) {
		setLibraryStatus(seriesId: $seriesId, status: $status) {
			...SeriesFields
			libraryStatus
			isFavorite
		}
	}
`;

export const SET_FAVORITE = /* GraphQL */ `
	${SERIES_FIELDS}
	mutation SetFavorite($seriesId: ID!, $favorite: Boolean!) {
		setFavorite(seriesId: $seriesId, favorite: $favorite) {
			...SeriesFields
			libraryStatus
			isFavorite
		}
	}
`;

export const SET_PROGRESS = /* GraphQL */ `
	mutation SetProgress($chapterId: ID!, $lastPageRead: Int!, $read: Boolean!) {
		setProgress(chapterId: $chapterId, lastPageRead: $lastPageRead, read: $read)
	}
`;

// Popularity signal: fire once per chapter open (see `views` module server-side). No
// auth — anonymous reads count too — so it never carries user identity.
export const RECORD_VIEW = /* GraphQL */ `
	mutation RecordView($seriesId: ID!) {
		recordView(seriesId: $seriesId)
	}
`;

// ---- social ----------------------------------------------------------------

export const REVIEWS = /* GraphQL */ `
	${USER_REF}
	query Reviews($seriesId: ID!, $page: Int) {
		reviews(seriesId: $seriesId, page: $page) {
			items {
				id
				seriesId
				author {
					...UserRefFields
				}
				score
				body
				hasSpoiler
				createdAt
				updatedAt
			}
			page
			hasNextPage
			total
		}
	}
`;

export const MY_REVIEW = /* GraphQL */ `
	${USER_REF}
	query MyReview($seriesId: ID!) {
		myReview(seriesId: $seriesId) {
			id
			seriesId
			author {
				...UserRefFields
			}
			score
			body
			hasSpoiler
			createdAt
			updatedAt
		}
	}
`;

export const POST_REVIEW = /* GraphQL */ `
	${USER_REF}
	mutation PostReview($input: PostReviewInput!) {
		postReview(input: $input) {
			id
			seriesId
			author {
				...UserRefFields
			}
			score
			body
			hasSpoiler
			createdAt
			updatedAt
		}
	}
`;

export const COMMENTS = /* GraphQL */ `
	${USER_REF}
	query Comments($targetType: String!, $targetId: ID!, $page: Int) {
		comments(targetType: $targetType, targetId: $targetId, page: $page) {
			items {
				id
				targetType
				targetId
				parentId
				author {
					...UserRefFields
				}
				body
				hasSpoiler
				mediaUrl
				mediaWidth
				mediaHeight
				createdAt
				likes
				dislikes
				myVote
			}
			page
			hasNextPage
			total
		}
	}
`;

export const POST_COMMENT = /* GraphQL */ `
	${USER_REF}
	mutation PostComment($input: PostCommentInput!) {
		postComment(input: $input) {
			id
			targetType
			targetId
			parentId
			author {
				...UserRefFields
			}
			body
			hasSpoiler
			mediaUrl
			mediaWidth
			mediaHeight
			createdAt
			likes
			dislikes
			myVote
		}
	}
`;

// Like (1), dislike (-1), or clear (0) a comment; returns the fresh tallies.
export const VOTE_COMMENT = /* GraphQL */ `
	mutation VoteComment($commentId: ID!, $value: Int!) {
		voteComment(commentId: $commentId, value: $value) {
			likes
			dislikes
			myVote
		}
	}
`;

// ---- notifications ---------------------------------------------------------

export const NOTIFICATIONS = /* GraphQL */ `
	${USER_REF}
	query Notifications($page: Int) {
		notifications(page: $page) {
			id
			kind
			actor {
				...UserRefFields
			}
			commentId
			commentExcerpt
			targetType
			targetId
			seriesId
			count
			createdAt
			read
		}
	}
`;

export const UNREAD_NOTIFICATION_COUNT = /* GraphQL */ `
	query UnreadNotificationCount {
		unreadNotificationCount
	}
`;

export const MARK_NOTIFICATIONS_READ = /* GraphQL */ `
	mutation MarkNotificationsRead($ids: [ID!]) {
		markNotificationsRead(ids: $ids)
	}
`;

// ---- auth ------------------------------------------------------------------

const SESSION_USER_FIELDS = /* GraphQL */ `
	fragment SessionUserFields on SessionUser {
		id
		username
		displayName
		bio
		avatarUrl
		isAdmin
		showNsfw
		joinedAt
	}
`;

const SESSION_FIELDS = /* GraphQL */ `
	${SESSION_USER_FIELDS}
	fragment SessionFields on Session {
		token
		user {
			...SessionUserFields
		}
	}
`;

export const SESSION = /* GraphQL */ `
	${SESSION_FIELDS}
	query Session {
		session {
			...SessionFields
		}
	}
`;

export const LOGIN = /* GraphQL */ `
	${SESSION_FIELDS}
	mutation Login($username: String!, $password: String!) {
		login(username: $username, password: $password) {
			...SessionFields
		}
	}
`;

export const REGISTER = /* GraphQL */ `
	${SESSION_FIELDS}
	mutation Register($input: RegisterInput!) {
		register(input: $input) {
			...SessionFields
		}
	}
`;

export const LOGOUT = /* GraphQL */ `
	mutation Logout {
		logout
	}
`;

// ---- admin -----------------------------------------------------------------

export const UPDATE_SERIES_ADMIN = /* GraphQL */ `
	${SERIES_FIELDS}
	mutation UpdateSeriesAdmin($input: SeriesAdminInput!) {
		updateSeriesAdmin(input: $input) {
			...SeriesFields
		}
	}
`;

export const SCAN_STATUS = /* GraphQL */ `
	query ScanStatus {
		scanStatus {
			librarySize
			overdueCount
			lastTickAt
			nextDueAt
		}
	}
`;

export const TRIGGER_SCAN = /* GraphQL */ `
	${SERIES_FIELDS}
	mutation TriggerScan($seriesId: ID!) {
		triggerScan(seriesId: $seriesId) {
			...SeriesFields
		}
	}
`;

const ADMIN_USER_FIELDS = /* GraphQL */ `
	fragment AdminUserFields on AdminUser {
		id
		username
		email
		avatarUrl
		isAdmin
		isBanned
		createdAt
	}
`;

export const BAN_USER = /* GraphQL */ `
	${ADMIN_USER_FIELDS}
	mutation BanUser($userId: ID!, $banned: Boolean!) {
		banUser(userId: $userId, banned: $banned) {
			...AdminUserFields
		}
	}
`;

export const DELETE_COMMENT = /* GraphQL */ `
	mutation DeleteComment($commentId: ID!) {
		deleteComment(commentId: $commentId)
	}
`;

export const USERS = /* GraphQL */ `
	${ADMIN_USER_FIELDS}
	query Users($page: Int) {
		users(page: $page) {
			items {
				...AdminUserFields
			}
			page
			hasNextPage
			total
		}
	}
`;

export const SET_USER_ADMIN = /* GraphQL */ `
	${ADMIN_USER_FIELDS}
	mutation SetUserAdmin($userId: ID!, $isAdmin: Boolean!) {
		setUserAdmin(userId: $userId, isAdmin: $isAdmin) {
			...AdminUserFields
		}
	}
`;

export const MERGE_QUEUE = /* GraphQL */ `
	query MergeQueue($page: Int, $limit: Int) {
		mergeQueue(page: $page, limit: $limit) {
			items {
				id
				sourceSeriesId
				sourceWorkId
				candidateWorkId
				candidateTitle
				sourceTitle
				sourceCoverUrl
				candidateCoverUrl
				score
				method
				status
				createdAt
			}
			page
			hasNextPage
			total
		}
	}
`;

export const COVER_ISSUES = /* GraphQL */ `
	query CoverIssues($page: Int) {
		coverIssues(page: $page) {
			items {
				workId
				title
				coverUrl
				reason
				detail
				attempts
				firstSeen
				lastSeen
			}
			page
			hasNextPage
			total
		}
	}
`;

export const RETRY_COVER = /* GraphQL */ `
	mutation RetryCover($workId: ID!) {
		retryCover(workId: $workId)
	}
`;

export const RESOLVE_MERGE_CANDIDATE = /* GraphQL */ `
	mutation ResolveMergeCandidate($id: ID!, $accept: Boolean!) {
		resolveMergeCandidate(id: $id, accept: $accept)
	}
`;

export const MERGE_WORKS = /* GraphQL */ `
	mutation MergeWorks($sourceWorkId: ID!, $targetWorkId: ID!) {
		mergeWorks(sourceWorkId: $sourceWorkId, targetWorkId: $targetWorkId) {
			targetWorkId
			movedSourceSeries
		}
	}
`;

export const ADD_SOURCE_SERIES = /* GraphQL */ `
	mutation AddSourceSeries($suwayomiMangaId: ID!) {
		addSourceSeries(suwayomiMangaId: $suwayomiMangaId) {
			decision
			workId
			matchedWorkId
			score
			method
			sourceSeriesId
		}
	}
`;

// ---- admin sources & extensions (EXT-1/EXT-2) --------------------------------

const EXTENSION_FIELDS = /* GraphQL */ `
	fragment ExtensionFields on ExtensionInfo {
		pkgName
		name
		lang
		versionName
		isInstalled
		hasUpdate
		isNsfw
		iconUrl
		repo
		subscribed
	}
`;

export const EXTENSIONS = /* GraphQL */ `
	${EXTENSION_FIELDS}
	query Extensions($refresh: Boolean!) {
		extensions(refresh: $refresh) {
			...ExtensionFields
		}
	}
`;

export const SOURCES = /* GraphQL */ `
	query Sources {
		sources {
			id
			name
			lang
			isNsfw
			iconUrl
			pkgName
		}
	}
`;

export const SOURCE_BROWSE = /* GraphQL */ `
	query SourceBrowse($sourceId: ID!, $type: SourceBrowseType!, $page: Int!, $query: String) {
		sourceBrowse(sourceId: $sourceId, type: $type, page: $page, query: $query) {
			page
			hasNextPage
			items {
				suwayomiMangaId
				title
				thumbnailUrl
				inLibrary
			}
		}
	}
`;

export const ADD_EXTENSION_REPO = /* GraphQL */ `
	mutation AddExtensionRepo($indexUrl: String!) {
		addExtensionRepo(indexUrl: $indexUrl)
	}
`;

export const INSTALL_EXTENSION = /* GraphQL */ `
	${EXTENSION_FIELDS}
	mutation InstallExtension($pkgName: String!) {
		installExtension(pkgName: $pkgName) {
			...ExtensionFields
		}
	}
`;

export const UNINSTALL_EXTENSION = /* GraphQL */ `
	${EXTENSION_FIELDS}
	mutation UninstallExtension($pkgName: String!) {
		uninstallExtension(pkgName: $pkgName) {
			...ExtensionFields
		}
	}
`;

export const UPDATE_EXTENSION = /* GraphQL */ `
	${EXTENSION_FIELDS}
	mutation UpdateExtension($pkgName: String!) {
		updateExtension(pkgName: $pkgName) {
			...ExtensionFields
		}
	}
`;

export const BULK_ADD_SOURCE_SERIES = /* GraphQL */ `
	mutation BulkAddSourceSeries($suwayomiMangaIds: [ID!]!) {
		bulkAddSourceSeries(suwayomiMangaIds: $suwayomiMangaIds) {
			total
			succeeded
			failed
			newWorks
			autoMerged
			queuedForReview
			alreadyExisting
			entries {
				suwayomiMangaId
				error
				result {
					decision
					workId
					matchedWorkId
					score
					method
					sourceSeriesId
				}
			}
		}
	}
`;

export const SET_SHOW_NSFW = /* GraphQL */ `
	mutation SetShowNsfw($value: Boolean!) {
		setShowNsfw(value: $value)
	}
`;

// Admin maintenance: materialize the whole Suwayomi library into the DB cache
// (metadata synchronously; chapter lists in a background task). Returns the
// number of series persisted. Admin-gated server-side (require_admin).
export const PERSIST_CATALOGUE = /* GraphQL */ `
	mutation PersistCatalogue {
		persistCatalogue
	}
`;

// Admin maintenance: materialize every canonical work's cover into the DB
// (work_cover_blob) so the web reader serves covers from /covers/{id}.webp instead
// of the Cloudflare image Worker. Kicks off a polite background crawl and returns
// how many works are still uncached (queued) at start. Admin-gated (require_admin).
export const MATERIALIZE_CATALOGUE_COVERS = /* GraphQL */ `
	mutation MaterializeCatalogueCovers {
		materializeCatalogueCovers
	}
`;

// ---- profile ---------------------------------------------------------------

export const UPDATE_PROFILE = /* GraphQL */ `
	${SESSION_USER_FIELDS}
	mutation UpdateProfile($input: UpdateProfileInput!) {
		updateProfile(input: $input) {
			...SessionUserFields
		}
	}
`;

export const MY_ACTIVITY = /* GraphQL */ `
	query MyActivity($limit: Int) {
		myActivity(limit: $limit) {
			id
			kind
			targetType
			targetId
			createdAt
		}
	}
`;

export const CANONICAL_UPDATES = /* GraphQL */ `
	query CanonicalUpdates($page: Int, $includeNsfw: Boolean) {
		canonicalUpdates(page: $page, includeNsfw: $includeNsfw) {
			workId
			mangadexId
			title
			isNsfw
			coverUrl
			latestChapter
			latestChapterTitle
			latestAt
		}
	}
`;

// The reader's merged Updates feed, paginated server-side. `page` and `type` are CORE
// arguments, not opportunistic ones, so neither belongs in OPTIONAL_ARGUMENTS — and note
// that `stripUnsupported` cannot rescue this document at all: it strips unknown
// ARGUMENTS, and an unknown FIELD (`updatesFeed` on a server that predates it) fails
// validation outright. Deploy the server before the reader; the reader guards with
// `backend.updatesFeed?.()` and falls back to the two-feed merge when it is absent.
export const UPDATES_FEED = /* GraphQL */ `
	query UpdatesFeed($page: Int, $type: ComicType) {
		updatesFeed(page: $page, type: $type) {
			items {
				id
				workId
				title
				coverUrl
				type
				latestChapter
				latestChapterTitle
				chapterCount
				releasedAt
				detectedAt
				rating
				isNsfw
			}
			page
			hasNextPage
			total
		}
	}
`;

// ---- canonical reader path -------------------------------------------------

// The two enrichment fields (S2) are opt-in resolver fields keyed on the canonical
// `w_` work id, so they're selected ONLY here (canonicalSeries), never on the shared
// SeriesFields fragment used by native/numeric-id queries.
export const CANONICAL_SERIES = /* GraphQL */ `
	${SERIES_FIELDS}
	query CanonicalSeries($workId: ID!) {
		canonicalSeries(workId: $workId) {
			...SeriesFields
			libraryStatus
			isFavorite
			views {
				total
				last7d
				last24h
			}
			localizedDescriptions {
				lang
				description
			}
			credits {
				role
				name
			}
			covers {
				fileName
				url
				thumbnailUrl
				lang
				volume
				isPrimary
			}
		}
	}
`;

export const CANONICAL_CHAPTERS = /* GraphQL */ `
	${CHAPTER_FIELDS}
	query CanonicalChapters($workId: ID!) {
		canonicalChapters(workId: $workId) {
			...ChapterFields
		}
	}
`;

export const CANONICAL_PAGES = /* GraphQL */ `
	query CanonicalPages($chapterId: ID!) {
		canonicalPages(chapterId: $chapterId) {
			index
			sourceUrl
			width
			height
		}
	}
`;

// ---- native-embedded-Suwayomi source routing -------------------------------

const WORK_SOURCE_FIELDS = /* GraphQL */ `
	fragment WorkSourceFields on WorkSource {
		sourceType
		sourceId
		sourceKey
		sourceUrl
		isNsfw
		lang
		extension {
			pkgName
			repoUrl
			apkName
			versionCode
			lang
		}
	}
`;

export const WORK_SOURCES = /* GraphQL */ `
	${WORK_SOURCE_FIELDS}
	query WorkSources($workId: ID!) {
		workSources(workId: $workId) {
			...WorkSourceFields
		}
	}
`;

export const WORK_SOURCES_BATCH = /* GraphQL */ `
	${WORK_SOURCE_FIELDS}
	query WorkSourcesBatch($workIds: [ID!]!) {
		workSourcesBatch(workIds: $workIds) {
			workId
			sources {
				...WorkSourceFields
			}
		}
	}
`;

/**
 * P2.1: the whole series/reader page in ONE round trip. Replaces the
 * canonicalSeries + canonicalChapters + workSources parallel fan-out AND the
 * per-translator `chapters` N+1 (plus the selected translator's `series`
 * follow-up) with a single `seriesPage(workId)` request. The `series` selection
 * mirrors CANONICAL_SERIES (S2 enrichment included); each source's `series`
 * mirrors the SERIES query so the picker's metadata is byte-compatible with what
 * the old per-source round trips returned. `aggregated` is opt-in via
 * `$withAggregated` — the series page wants it, the reader route does not.
 */
export const SERIES_PAGE = /* GraphQL */ `
	${SERIES_FIELDS}
	${CHAPTER_FIELDS}
	${WORK_SOURCE_FIELDS}
	query SeriesPage($workId: ID!, $withAggregated: Boolean! = false) {
		seriesPage(workId: $workId) {
			workId
			series {
				...SeriesFields
				libraryStatus
				isFavorite
				views {
					total
					last7d
					last24h
				}
				localizedDescriptions {
					lang
					description
				}
				credits {
					role
					name
				}
				covers {
					fileName
					url
					thumbnailUrl
					lang
					volume
					isPrimary
				}
			}
			spineChapters {
				...ChapterFields
			}
			sources {
				source {
					...WorkSourceFields
				}
				chapters {
					...ChapterFields
				}
				series {
					...SeriesFields
					libraryStatus
					isFavorite
					views {
						total
						last7d
						last24h
					}
				}
			}
			aggregated @include(if: $withAggregated) {
				number
				title
				sources {
					sourceType
					sourceId
					suwayomiMangaId
					chapterId
					scanlator
				}
			}
		}
	}
`;

/**
 * Admin dedup-review expansion for one side of a candidate row. Lives here, below
 * {@link WORK_SOURCE_FIELDS}, because it reuses that fragment — these consts are
 * interpolated at module-eval time, so it cannot be declared beside MERGE_QUEUE.
 */
export const WORK_REVIEW_DETAIL = /* GraphQL */ `
	${WORK_SOURCE_FIELDS}
	query WorkReviewDetail($workId: ID!) {
		workReviewDetail(workId: $workId) {
			workId
			title
			coverUrl
			author
			artist
			year
			status
			contentRating
			originalLanguage
			isNsfw
			description
			chapterCount
			altTitles
			sources {
				...WorkSourceFields
			}
		}
	}
`;

// ---- admin catalogue provenance + scan pause (EXT-2) -------------------------

export const SERIES_SOURCES_BATCH = /* GraphQL */ `
	${WORK_SOURCE_FIELDS}
	query SeriesSourcesBatch($seriesIds: [ID!]!) {
		seriesSourcesBatch(seriesIds: $seriesIds) {
			seriesId
			workId
			sources {
				...WorkSourceFields
			}
		}
	}
`;

// ---- admin background source-ingest jobs (S1) --------------------------------

const SOURCE_INGEST_JOB_FIELDS = /* GraphQL */ `
	fragment SourceIngestJobFields on SourceIngestJob {
		id
		sourceId
		state
		pagesDone
		itemsSeen
		succeeded
		failed
		newWorks
		autoMerged
		queuedForReview
		alreadyExisting
		error
		startedAt
		finishedAt
	}
`;

export const SOURCE_INGEST_JOBS = /* GraphQL */ `
	${SOURCE_INGEST_JOB_FIELDS}
	query SourceIngestJobs($active: Boolean!) {
		sourceIngestJobs(active: $active) {
			...SourceIngestJobFields
		}
	}
`;

export const START_SOURCE_INGEST = /* GraphQL */ `
	${SOURCE_INGEST_JOB_FIELDS}
	mutation StartSourceIngest($sourceId: ID!) {
		startSourceIngest(sourceId: $sourceId) {
			...SourceIngestJobFields
		}
	}
`;

export const CANCEL_SOURCE_INGEST = /* GraphQL */ `
	${SOURCE_INGEST_JOB_FIELDS}
	mutation CancelSourceIngest($jobId: ID!) {
		cancelSourceIngest(jobId: $jobId) {
			...SourceIngestJobFields
		}
	}
`;

export const START_EXTENSION_INGEST = /* GraphQL */ `
	${SOURCE_INGEST_JOB_FIELDS}
	mutation StartExtensionIngest($pkgName: ID!) {
		startExtensionIngest(pkgName: $pkgName) {
			...SourceIngestJobFields
		}
	}
`;

export const CANCEL_EXTENSION_INGEST = /* GraphQL */ `
	${SOURCE_INGEST_JOB_FIELDS}
	mutation CancelExtensionIngest($pkgName: ID!) {
		cancelExtensionIngest(pkgName: $pkgName) {
			...SourceIngestJobFields
		}
	}
`;

export const SET_EXTENSION_SUBSCRIPTION = /* GraphQL */ `
	mutation SetExtensionSubscription($pkgName: ID!, $subscribed: Boolean!) {
		setExtensionSubscription(pkgName: $pkgName, subscribed: $subscribed)
	}
`;

export const SET_SERIES_PAUSED = /* GraphQL */ `
	${SERIES_FIELDS}
	mutation SetSeriesPaused($seriesId: ID!, $paused: Boolean!) {
		setSeriesPaused(seriesId: $seriesId, paused: $paused) {
			...SeriesFields
		}
	}
`;

// ---- admin series-detail editor (metadata + chapter overrides + rescan) ------

export const SERIES_ADMIN_META = /* GraphQL */ `
	query SeriesAdminMeta($seriesId: ID!) {
		seriesAdminMeta(seriesId: $seriesId) {
			workId
			titleOverride
			descriptionOverride
			contentTypeOverride
			isNsfwOverride
			tags
			hasCuratedTags
		}
	}
`;

export const UPDATE_SERIES_METADATA = /* GraphQL */ `
	${SERIES_FIELDS}
	mutation UpdateSeriesMetadata($input: SeriesMetadataInput!) {
		updateSeriesMetadata(input: $input) {
			...SeriesFields
		}
	}
`;

export const ADD_SERIES_ALT_TITLE = /* GraphQL */ `
	${SERIES_FIELDS}
	mutation AddSeriesAltTitle($id: ID!, $title: String!) {
		addSeriesAltTitle(id: $id, title: $title) {
			...SeriesFields
		}
	}
`;

export const REMOVE_SERIES_ALT_TITLE = /* GraphQL */ `
	${SERIES_FIELDS}
	mutation RemoveSeriesAltTitle($id: ID!, $title: String!) {
		removeSeriesAltTitle(id: $id, title: $title) {
			...SeriesFields
		}
	}
`;

export const CONSOLIDATE_EXACT_DUPLICATES = /* GraphQL */ `
	mutation ConsolidateExactDuplicates($limit: Int) {
		consolidateExactDuplicates(limit: $limit)
	}
`;

export const WORK_CHAPTERS_ADMIN = /* GraphQL */ `
	query WorkChaptersAdmin($workId: ID!) {
		workChaptersAdmin(workId: $workId) {
			number
			key
			sourceTitle
			titleOverride
			effectiveTitle
			hidden
			sourceCount
		}
	}
`;

export const SET_CHAPTER_OVERRIDE = /* GraphQL */ `
	mutation SetChapterOverride($input: ChapterOverrideInput!) {
		setChapterOverride(input: $input)
	}
`;

export const RESCAN_WORK = /* GraphQL */ `
	mutation RescanWork($workId: ID!) {
		rescanWork(workId: $workId)
	}
`;
