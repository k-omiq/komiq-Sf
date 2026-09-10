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
export const OPTIONAL_SERIES_FIELDS = [
	// Metadata is the sole nested optional selection (balanced child stripping).
	'metadata',
	'averageRating',
	'ratingCount',
	'detectedAt',
	'workId',
	'latestChapter',
	// F12's per-chapter date on `AggregatedChapter`. Optional for the usual reason: the
	// reader and the API deploy separately, and a reader that shipped first would
	// otherwise fail the WHOLE series page rather than lose one date line. Its caller-side
	// fallback is the pre-F12 behaviour — a row with no date renders without one.
	//
	// NOT named `releasedAt`, deliberately: this stripper matches by NAME across every
	// document, and `UpdateFeedRow.releasedAt` (which every deployed server already has)
	// is selected in the updates query. Sharing the name would mean one older server
	// rejecting the aggregated field also silently strips the feed's date for the rest of
	// the session. The name is also the more honest one — it is the FIRST release across
	// the chapter's sources, not the selected source's.
	'firstReleasedAt',
	// Cross-source read state on `AggregatedChapter`. Optional for the same deploy-ordering
	// reason, and named `readAnywhere` rather than `read` DELIBERATELY: this stripper
	// matches by NAME across every document, and `Chapter.read` is selected in the chapter
	// lists and the reader itself — sharing the name would mean one older server rejecting
	// this field also silently blanked every real read marker for the rest of the session.
	'readAnywhere',
	// `AggregatedChapter.chapterVolume` — the series page's volume headers. Optional for the
	// deploy-ordering reason; stripped, the list simply renders without headers. Not named
	// `volume`: this stripper matches by NAME across every document and `Cover.volume` is
	// selected on the same page.
	'chapterVolume',
	// `UpdateFeedRow.announcedAt` (migration 0099) — when we could first SHOW a chapter, as
	// opposed to when its source says it came out. Optional for the usual deploy-ordering
	// reason, and the caller-side fallback is exact rather than approximate: every render
	// site spells `announcedAt ?? releasedAt`, and the two are equal on every row that
	// predates the column, so a stripped document degrades to the old behaviour precisely.
	//
	// The name is unique across every document for the reason `firstReleasedAt` and
	// `readAnywhere` document above: this stripper matches by NAME globally, so reusing
	// `releasedAt`/`detectedAt` here would let one old server blank a field the feed depends
	// on for the rest of the session.
	'announcedAt',
	// `Chapter.foundAt` (migration 0099) — the per-chapter half of the same clock, on the
	// series-detail list. Optional so a reader ahead of the server loses one column instead
	// of the whole chapter list. Its fallback is NOT a substitute value: the UI omits the
	// Found line, just as it does for null, so stripping it is indistinguishable from the
	// ordinary no-record case.
	'foundAt',
	// Per-COPY fields on `ChapterSource` (one entry per (source, scanlation group) upload of
	// a chapter number). Optional for the usual deploy-ordering reason: without them a reader
	// that shipped first would fail the whole series page and reader. Fallbacks: a copy with
	// no `copyReleasedAt` renders without a date; a copy with no `copyExternalUrl` is opened
	// as ordinary pages (exactly the pre-field behaviour). Prefixed `copy…` because this
	// stripper matches by NAME across every document, and `releasedAt` / `externalUrl` are
	// load-bearing elsewhere (`UpdateFeedRow.releasedAt`, `Chapter.externalUrl`).
	'copyReleasedAt',
	'copyExternalUrl',
	// `SessionUser.mustChangePassword` (self-host accounts): set on an account an admin
	// created or reset, so the apps can ask for a new password at first sign-in. Optional
	// so the shared session fragment keeps working against a server that predates it;
	// stripped, the flag reads as absent and nobody is asked, which is the old behaviour.
	'mustChangePassword',
] as const;

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
export const OPTIONAL_ARGUMENTS = [
	'includeNsfw',
	// Browse v2's `search(filter: BrowseFilter)`. Optional by the definition above — it is
	// safe to omit, and the caller can tell that it was (`Backend.browseFilterSupported`).
	// Against a server that predates it the filter is stripped and the SAME request
	// carries the legacy arguments the reader derives from it (`toLegacyFilters` in the
	// reader's `$lib/data/browse-filters`), so an old server degrades to the subset it
	// understands — genres/format/single status/hasChapters — instead of failing Browse.
	//
	// Unlike `sort` (see the note above SEARCH) stripping this does not silently lie: the
	// reader checks `browseFilterSupported()` after the call and says which filters did
	// not apply. The variable's TYPE (`BrowseFilter`) is unknown to such a server as
	// well, which async-graphql reports as a separate `Unknown type` error in the SAME
	// validation pass, so the `Unknown argument "filter"` match below still fires.
	//
	// The name is now matched globally (token-based, see above): no other document may
	// pass an argument called `filter` as `$filter`, or one old server would strip it there
	// too.
	'filter',
] as const;

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
		# The newest chapter's NUMBER ("151"), which is NOT \`chapterCount\` ("12 ch") —
		# printing the count under a "Ch." label is the F4 bug the chapter-number contract
		# exists to prevent, so a null here means print the count ALONE. Own line, no alias
		# and no arguments, because the unknown-field fallback strips it by line
		# (OPTIONAL_SERIES_FIELDS) — this field is newer than the deployed API, and without
		# that entry a reader shipped first would fail EVERY series query rather than lose
		# one label.
		latestChapter
	}
`;

const CHAPTER_FIELDS = /* GraphQL */ `
	fragment ChapterFields on Chapter {
		id
		seriesId
		number
		label
		externalUrl
		title
		pageCount
		uploadedAt
		# When WE first had it, as against uploadedAt's "when the source published it". On its
		# own line, no alias/args/sub-selection, and not the only selection here — the three
		# conditions the line-based unknown-field fallback needs (OPTIONAL_SERIES_FIELDS).
		# (No backticks in this comment: it is inside a JS template literal.)
		foundAt
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
//
// `filter` (Browse v2) is the exception, and it IS in OPTIONAL_ARGUMENTS: it supersedes
// every argument above (filters apply WITH a text query too), and its absence is
// detectable by the caller — see the note on its OPTIONAL_ARGUMENTS entry.
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
		$filter: BrowseFilter
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
			filter: $filter
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

// Browse v2's tag vocabulary. NOT strippable by the unknown-field fallback — `tags` is
// the document's only root selection, so removing it would leave `{ }` and
// `stripUnsupported` bails (by design) — so a server that predates it is handled by
// the caller instead: `GraphQLBackend.tags()` maps its `Unknown field "tags"` to `[]`,
// and the reader then shows the legacy genre facets.
export const TAGS = /* GraphQL */ `
	query Tags {
		tags {
			id
			name
			group
			count
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
			label
			title
			firstReleasedAt
			# Cross-source read state — true when the viewer read this chapter under ANY
			# source, not just the selected translator. Own line, no alias: the
			# unknown-field fallback strips it by line (OPTIONAL_SERIES_FIELDS).
			readAnywhere
			# The volume label (MangaDex's), for the series page's volume headers. Own line,
			# no alias — the unknown-field fallback strips it by line.
			chapterVolume
			# One entry per COPY: every (source, scanlation group) upload of the number.
			sources {
				sourceType
				sourceId
				suwayomiMangaId
				chapterId
				scanlator
				copyReleasedAt
				copyExternalUrl
			}
		}
	}
`;

export const SERIES = /* GraphQL */ `
	${SERIES_FIELDS}
	query Series($id: ID!) {
		series(id: $id) {
			...SeriesFields
			metadata {
				year
				originalLanguage
				demographic
				contentRating
				tags {
					id
					name
					group
				}
				trackers {
					provider
					externalId
					url
				}
			}
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
			lastReadAt
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

// The series list's per-chapter toggle: sets exactly this chapter (no catch-up cascade),
// and unread clears every copy of it — which `setProgress` cannot express.
export const SET_CHAPTER_READ = /* GraphQL */ `
	mutation SetChapterRead($chapterId: ID!, $read: Boolean!) {
		setChapterRead(chapterId: $chapterId, read: $read)
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

// Withdrawing a rating is a DELETE, not a `postReview` with score 0: the server rejects
// anything outside 1..10, and `reviews.score` is NOT NULL, so there is no "scoreless
// review" state for a 0 to mean. Returns whether a row was actually removed.
export const DELETE_REVIEW = /* GraphQL */ `
	mutation DeleteReview($seriesId: ID!) {
		deleteReview(seriesId: $seriesId)
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
		mustChangePassword
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

// ---- self-host setup + passwords ---------------------------------------------

export const INSTANCE = /* GraphQL */ `
	query Instance {
		instance {
			setupRequired
			name
			version
			registrationOpen
			mangadexEnabled
		}
	}
`;

export const CREATE_OWNER = /* GraphQL */ `
	${SESSION_FIELDS}
	mutation CreateOwner(
		$setupCode: String!
		$username: String!
		$email: String!
		$password: String!
	) {
		createOwner(setupCode: $setupCode, username: $username, email: $email, password: $password) {
			...SessionFields
		}
	}
`;

export const CHANGE_PASSWORD = /* GraphQL */ `
	${SESSION_USER_FIELDS}
	mutation ChangePassword($currentPassword: String!, $newPassword: String!) {
		changePassword(currentPassword: $currentPassword, newPassword: $newPassword) {
			...SessionUserFields
		}
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

export const CREATE_USER = /* GraphQL */ `
	${ADMIN_USER_FIELDS}
	mutation CreateUser($input: CreateUserInput!) {
		createUser(input: $input) {
			...AdminUserFields
		}
	}
`;

export const RESET_USER_PASSWORD = /* GraphQL */ `
	mutation ResetUserPassword($userId: ID!, $password: String!) {
		resetUserPassword(userId: $userId, password: $password)
	}
`;

export const SET_REGISTRATION_OPEN = /* GraphQL */ `
	mutation SetRegistrationOpen($open: Boolean!) {
		setRegistrationOpen(open: $open)
	}
`;

// ---- admin: access + Cloudflare setup -----------------------------------------

const ACCESS_STATUS_FIELDS = /* GraphQL */ `
	fragment AccessStatusFields on AccessStatus {
		mode
		domain
		address
		cloudflare {
			domain
			connection
			zoneName
			complete
			removable
		}
		connector
		connectorMessage
		https
		trustedProxies
		trustedProxiesPinned
		job {
			kind
			running
			failed
			steps {
				id
				label
				state
				message
			}
		}
	}
`;

export const ACCESS_STATUS = /* GraphQL */ `
	${ACCESS_STATUS_FIELDS}
	query AccessStatus {
		accessStatus {
			...AccessStatusFields
		}
	}
`;

export const SET_ACCESS_MODE = /* GraphQL */ `
	${ACCESS_STATUS_FIELDS}
	mutation SetAccessMode($mode: String!, $domain: String) {
		setAccessMode(mode: $mode, domain: $domain) {
			...AccessStatusFields
		}
	}
`;

export const CHECK_CLOUDFLARE_TOKEN = /* GraphQL */ `
	mutation CheckCloudflareToken($token: String!, $domain: String!) {
		checkCloudflareToken(token: $token, domain: $domain) {
			zoneName
			zoneStatus
			accountName
		}
	}
`;

export const START_CLOUDFLARE_SETUP = /* GraphQL */ `
	${ACCESS_STATUS_FIELDS}
	mutation StartCloudflareSetup($input: CloudflareSetupInput!) {
		startCloudflareSetup(input: $input) {
			...AccessStatusFields
		}
	}
`;

export const RETRY_CLOUDFLARE_SETUP = /* GraphQL */ `
	${ACCESS_STATUS_FIELDS}
	mutation RetryCloudflareSetup($token: String!) {
		retryCloudflareSetup(token: $token) {
			...AccessStatusFields
		}
	}
`;

export const REMOVE_CLOUDFLARE_SETUP = /* GraphQL */ `
	${ACCESS_STATUS_FIELDS}
	mutation RemoveCloudflareSetup($token: String!) {
		removeCloudflareSetup(token: $token) {
			...AccessStatusFields
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

/** Every field of a report, shared by the submit mutation and the admin queue so the
 *  two can never drift apart. */
const REPORT_FIELDS = /* GraphQL */ `
	fragment ReportFields on Report {
		id
		kind
		status
		reporter
		subjectId
		subjectIdSecondary
		subjectTitle
		commentId
		reportedUsername
		commentExcerpt
		sourceUrl
		detail
		adminNote
		createdAt
		resolvedAt
	}
`;

/** Reader-facing: file a report. Works signed out (see `submitReport` on the server). */
export const SUBMIT_REPORT = /* GraphQL */ `
	${REPORT_FIELDS}
	mutation SubmitReport($input: ReportInput!) {
		submitReport(input: $input) {
			...ReportFields
		}
	}
`;

/** Admin queue. Null `status`/`kind` mean "every value". */
export const REPORTS = /* GraphQL */ `
	${REPORT_FIELDS}
	query Reports($status: ReportStatus, $kind: ReportKind, $page: Int) {
		reports(status: $status, kind: $kind, page: $page) {
			items {
				...ReportFields
			}
			page
			hasNextPage
			total
			openCount
		}
	}
`;

export const RESOLVE_REPORT = /* GraphQL */ `
	${REPORT_FIELDS}
	mutation ResolveReport($reportId: ID!, $status: ReportStatus!, $note: String) {
		resolveReport(reportId: $reportId, status: $status, note: $note) {
			...ReportFields
		}
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

// The admin SPLIT picker's list. A separate document from `WORK_SOURCES` rather than extra
// fields on it, because that one is fired by every reader on every series page and this one
// carries `source_series.id` — a row identity no public surface should be handing out.
export const WORK_SOURCE_ROWS = /* GraphQL */ `
	query WorkSourceRows($workId: ID!) {
		workSourceRows(workId: $workId) {
			id
			sourceType
			sourceName
			lang
			iconUrl
			title
			chapterCount
			sourceUrl
		}
	}
`;

// Detach sources onto a new work. NOT an "unmerge" — see `Backend.splitSourceSeries`.
//
// `newReaderId` is the field the caller navigates with, and it is NOT `newWorkId`: a
// Suwayomi-only split produces a work with no MangaDex anchor, which `canonicalSeries`
// refuses to serve, so linking it as `/series/w_…` would 404 on the page we just told the
// admin we created.
export const SPLIT_SOURCE_SERIES = /* GraphQL */ `
	mutation SplitSourceSeries($workId: ID!, $sourceSeriesIds: [ID!]!) {
		splitSourceSeries(workId: $workId, sourceSeriesIds: $sourceSeriesIds) {
			newWorkId
			newReaderId
			title
			movedSources
		}
	}
`;

// Catalogue search for the admin MERGE PICKER, which needs a different shape from the
// reader's `SEARCH` and so gets its own lean document rather than a flag on that one.
//
// `workId` IS THE POINT. `mergeWorks` addresses works, but `Series.id` is the reader id —
// `w_…` only when the work is MangaDex-anchored, and the numeric Suwayomi id otherwise —
// so passing `id` as `targetWorkId` would send a numeric id to a mutation that only
// understands `w_…`. It stays off `SeriesFields` because it is a resolver field (one query
// per row, an N+1 on any grid); paying it on ≤30 admin-only rows is the trade this
// document exists to make. Kept on its OWN LINE for the unknown-field stripper
// (OPTIONAL_SERIES_FIELDS).
//
// The rest is deliberately minimal — enough to RECOGNISE a duplicate (cover, author,
// chapter count) and no more; every extra field here is another per-row cost.
//
// `includeNsfw: true` because a duplicate is just as likely to be NSFW-flagged, and ~2,500
// mainstream works are wrongly flagged today — an admin merging duplicates must be able to
// see them. The server honours the argument for admins only, so this grants nothing to
// anyone else, and the stripper handles a server that predates it (OPTIONAL_ARGUMENTS).
export const SEARCH_FOR_MERGE = /* GraphQL */ `
	query SearchForMerge($query: String!, $page: Int, $includeNsfw: Boolean) {
		search(query: $query, page: $page, includeNsfw: $includeNsfw) {
			items {
				id
				title
				author
				coverUrl
				chapterCount
				isNsfw
				workId
			}
			page
			hasNextPage
			total
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

export const EXTENSION_REPOS = /* GraphQL */ `
	query ExtensionRepos {
		extensionRepos {
			indexUrl
			name
			badgeLabel
		}
	}
`;

export const REMOVE_EXTENSION_REPO = /* GraphQL */ `
	mutation RemoveExtensionRepo($indexUrl: String!) {
		removeExtensionRepo(indexUrl: $indexUrl)
	}
`;

export const MANGADEX_SETTING = /* GraphQL */ `
	query MangadexSetting {
		mangadexSetting {
			enabled
			pinned
			pinnedBy
		}
	}
`;

export const SET_MANGADEX_ENABLED = /* GraphQL */ `
	mutation SetMangadexEnabled($enabled: Boolean!) {
		setMangadexEnabled(enabled: $enabled)
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

export const MY_COMMENTS = /* GraphQL */ `
	query MyComments($page: Int) {
		myComments(page: $page) {
			items {
				id
				targetType
				targetId
				parentId
				body
				hasSpoiler
				createdAt
				likes
				dislikes
				replyCount
				seriesId
				seriesTitle
				chapterNumber
			}
			page
			hasNextPage
			total
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
			averageRating
			ratingCount
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
				status
				latestChapter
				latestChapterTitle
				chapterCount
				releasedAt
				# The feed's sort key, so also the clock the card labels with. Own line for
				# the unknown-field fallback (OPTIONAL_SERIES_FIELDS).
				announcedAt
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
			metadata {
				year
				originalLanguage
				demographic
				contentRating
				tags {
					id
					name
					group
				}
				trackers {
					provider
					externalId
					url
				}
			}
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
				metadata {
					year
					originalLanguage
					demographic
					contentRating
					tags {
						id
						name
						group
					}
					trackers {
						provider
						externalId
						url
					}
				}
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
				label
				title
				# Own line, no alias — the unknown-field fallback strips it by line.
				firstReleasedAt
				chapterVolume
				# One entry per COPY: every (source, scanlation group) upload of the number.
				# The two copy… fields are opportunistic (OPTIONAL_SERIES_FIELDS).
				sources {
					sourceType
					sourceId
					suwayomiMangaId
					chapterId
					scanlator
					copyReleasedAt
					copyExternalUrl
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

/**
 * Per-source scan health (Phase E4.3). Every field is a count the console renders, so there
 * is no fragment to share — this query has one caller.
 */
export const SOURCE_SCAN_HEALTH = /* GraphQL */ `
	query SourceScanHealth {
		sourceScanHealth {
			id
			name
			pkgName
			installed
			subscribed
			subscriptionDisabledAt
			series
			failing
			confirmedFailing
			cachedFallback
			fetchError
			zeroChapterSeries
			exclusiveWorks
			worstStreak
			lastFailureAt
			lastScannedAt
			outageDetectedAt
			outageLastAlertAt
			outageKind
			outageParkedUntil
			activity {
				surface
				reason
				count
				lastMessage
				lastAt
			}
		}
	}
`;

const EXTENSION_AUTO_UPDATE_FIELDS = /* GraphQL */ `
	fragment ExtensionAutoUpdateFields on ExtensionAutoUpdate {
		enabled
		intervalSeconds
		running
		lastRunAt
		lastError
		lastUpdated
		lastFailed
		recent {
			at
			pkgName
			fromVersion
			toVersion
			origin
			error
		}
	}
`;

export const EXTENSION_AUTO_UPDATE = /* GraphQL */ `
	${EXTENSION_AUTO_UPDATE_FIELDS}
	query ExtensionAutoUpdate {
		extensionAutoUpdate {
			...ExtensionAutoUpdateFields
		}
	}
`;

export const RUN_EXTENSION_UPDATES = /* GraphQL */ `
	${EXTENSION_AUTO_UPDATE_FIELDS}
	mutation RunExtensionUpdates {
		runExtensionUpdates {
			...ExtensionAutoUpdateFields
		}
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
			author
			artist
			year
			originalLanguage
			status
			demographic
			contentRating
			pinnedFields
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
		consolidateExactDuplicates(limit: $limit) {
			merged
			dismissed
			queued
			pending
			done
		}
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
