#!/usr/bin/env node
// Capacity probe for komika-server: how many concurrent active users (reads, writes
// and images) can one server carry. Zero dependencies, Node 22.
//
// It keeps the measuring core of apps/server/bench/loadtest.mjs (one keep-alive
// Agent, closed-loop `--vus` ramp, open-loop `--rate`, discarded warm-up, p50/p95/p99,
// stop above a p95 limit) and adds what that tool lacks: the series page bundle, text
// search with real words, sign-in, writes, images, server metrics and a run manifest.
//
// SCENARIOS (one workload class each, so results never mix)
//   read          anonymous metadata reads, a weighted mix: home (discovery + updates
//                 feed + latest added), browse rows, the series page bundle, text search
//                 with words taken from real titles (`--typo-share` of them one letter
//                 off). Sent the way the reader sends anonymous reads: GET /graphql
//                 (`--anon-method post` for POST).
//   write         signed-in writes only: setProgress, mark / unmark, postComment then
//                 deleteComment. Each virtual user has its OWN account: the server takes a
//                 per-user lock for these writes, so shared accounts would queue behind
//                 each other and understate capacity. `--progress-pattern sequential`
//                 writes page n+1 of an established chapter; `random` (the default, and
//                 what earlier runs did) writes a random chapter at a random page.
//   session       one realistic signed-in reader per virtual user, driven by `--users`,
//                 not by a rate. No images. The model follows apps/reader (file:line at
//                 SESSION MODEL below).
//   image-warm    store covers (`/covers/…`) and fixture page stand-ins from a bounded
//                 harvested set, after one warm pass that fetched each once.
//   image-cold    unique, never-fetched fixture images, each fetched once. Refuses to
//                 start without `--fixture-images`.
//   mixed         `session` plus the page images the reader loads while reading
//                 (`--prefetch-pages` at chapter open, then one per page), as fixture
//                 stand-ins. Only chapters whose page list was harvested are read.
//   release-burst every virtual user opens the newest chapter of `--burst-works` works at
//                 the same instant (the start of the measured window), then reads it.
//   IMAGE POLICY: the server keeps no page images (the page route asks the engine and
//   `/img` fetches upstream on every request), so sustained image load only fetches
//   store covers and --fixture-images addresses. Engine pages, `/img?src=…` and any
//   address with a nested source are real-source: fetched only under
//   `--real-source-sample N` (at most 200 in the whole run, warm pass included).
//
// `--hot-works N` concentrates every virtual user on N works (the same N for the same
// seed, and the 5 hottest are inside the 20 hottest). `--seed` makes the pool choice and
// every virtual user's path reproducible; a run without it picks a seed and records it.
//
// SETUP, before any load
//   1. `--accounts N` test accounts: `register`, or `login` when the name exists. The
//      password is generated once and kept, with the session tokens, in `--state`
//      (mode 0600). It is never printed and never written to `--json`.
//   2. A pool of real ids: about `--pool` work ids from discovery / updatesFeed / browse,
//      then `seriesPage` for `--sample` of them. That call harvests chapter ids AND warms
//      the server's per-source caches, so the run does not measure cold upstream fetches.
//      Works whose series page errors or stays slow are left out, and listed with the
//      reason in `pool.excluded`.
//   3. Only when a step needs images: cover addresses from the pool and, when page
//      images can be fetched (fixtures or a real-source sample), page lists for at most
//      `--image-chapters` chapters; capped at `--image-urls`, then one warm pass over
//      them. Recorded in `images`; real-source use in `realSource`.
//
// EVERY request carries `X-Forwarded-For: 10.a.b.c`, derived from the virtual user's
// number. The server only believes it when the socket peer is in TRUSTED_PROXY_CIDRS;
// then each virtual user is its own client for the per-IP limiters.
//
// WHAT IS MEASURED
//   Latency is the time to the last response byte, over answered-OK requests plus
//   timeouts (a timeout counts at its full length). Fast failures (503, GraphQL
//   errors) are kept OUT of the percentiles, so a flood of quick refusals cannot make
//   the server look fast; they are counted as errors instead. A response with a GraphQL
//   `errors` array is an error. An image answer is an error when it is not 2xx, is empty
//   or is not `image/*`. Every image byte is read and counted. The ramp stops when p95
//   (reads or writes) passes `--max-p95`, image p95 passes `--max-image-p95`, or the
//   error share passes `--max-errors` (1%).
//   Requests ask for gzip like a browser does, so the server pays for compression.
//   `lag` is this generator's own event-loop delay (p99): when it is high, the
//   generator is the bottleneck and the step's numbers are not the server's.
//
// AROUND EACH LOAD STEP
//   `--metrics-url` (the server's loopback /metrics, e.g. through an ssh tunnel) is
//   scraped when the measured window opens, every `--metrics-interval` seconds, and when
//   it closes. The text is parsed generically (no metric name is assumed): counters get
//   a delta, gauges first/last/min/max. Hit ratios, byte totals and refusing lanes are
//   derived when matching counters exist. A failing scrape is recorded, never fatal.
//   `--host-cmd` runs once per load step when the window opens, with {label} and
//   {seconds} filled in (e.g. `ssh testhost ~/measure.sh --json {label} {seconds}`); its
//   JSON stdout is merged into the step. A failing command is recorded, never fatal.
//
// ONE COMMAND
//   `--suite <file.json>` runs a list of steps in order into ONE `--json` file with the
//   run manifest (git HEAD and dirty files, Node, generator host, every effective
//   option, `--image-digest`, `--settings-file`, seed, times), the pool report, and each
//   step's results, metrics deltas and host measurements. See bench/suites/.
//
// SAFETY
//   The production hosts (komiq.cc, *.komiq.cc and the configured production addresses
//   in any spelling, IPv4-in-IPv6 included) are refused as target, metrics, image proxy
//   or any fixture origin, by text and by DNS, and anywhere in an image address's query.
//   `--host-cmd` is refused when its text names production in any spelling; it is not
//   parsed, and the operator owns what it does. Its text is recorded only with secrets
//   replaced. There is no override.
//   The production addresses are not in the source. They come from KOMIKA_PRODUCTION_IPS
//   (IPv4 addresses separated by commas or spaces) and the git-ignored file
//   deploy/production-ips.local (one IPv4 address per line, `#` comments), both read and
//   joined. deploy/watch.sh reads the same two. An entry that is not an IPv4 address
//   stops the tool. With neither set, only the komiq.cc names are refused, and the tool
//   says so once on stderr.
//
// USAGE
//   node capacity.mjs --scenario read    --vus 1,2,4,8,16,32,64 --duration 20 --warmup 5
//   node capacity.mjs --scenario read    --rate 20,40,80,160 --duration 30
//   node capacity.mjs --scenario write   --vus 1,2,4,8,16,32 --state /s/state.json
//   node capacity.mjs --scenario session --users 100,250,500,1000 --duration 180 --warmup 60 --state /s/state.json
//   node capacity.mjs --suite suites/m2-baseline.json --metrics-url http://127.0.0.1:19091/metrics \
//        --host-cmd 'ssh testhost ~/measure.sh --json {label} {seconds}' --json m2.json
//   node capacity.mjs --help
//
// The operation text in Q below is copied byte for byte from
// packages/api/src/operations.ts (regenerate: see the note above Q). `--queries
// <file.json>` overrides single operations.

import http from 'node:http';
import https from 'node:https';
import zlib from 'node:zlib';
import os from 'node:os';
import dns from 'node:dns';
import { URL, fileURLToPath, pathToFileURL } from 'node:url';
import { dirname, resolve as resolvePath } from 'node:path';
import { existsSync, readFileSync, writeFileSync, renameSync, realpathSync } from 'node:fs';
import { randomBytes, createHash } from 'node:crypto';
import { execFileSync, spawn } from 'node:child_process';
import { monitorEventLoopDelay } from 'node:perf_hooks';

// ---------------------------------------------------------------------------
// GraphQL operations
// ---------------------------------------------------------------------------
// Generated, not typed by hand:
//   node --experimental-strip-types -e "import('<repo>/packages/api/src/operations.ts')
//     .then(o => console.log(JSON.stringify(Object.fromEntries(NAMES.map(n => [n, o[n]])))))"
// so the server validates and executes exactly what the reader ships (fragments,
// comments and all). Do not trim: SeriesFields is the expensive part of every feed.
let Q = {
	DISCOVERY:
		'\n\t\n\tfragment SeriesFields on Series {\n\t\tid\n\t\ttitle\n\t\taltTitles\n\t\tauthor\n\t\tartist\n\t\tdescription\n\t\tgenres\n\t\ttype\n\t\tstatus\n\t\tcoverUrl\n\t\tsourceId\n\t\tchapterCount\n\t\tisMarked\n\t\tisNsfw\n\t\trating {\n\t\t\taverage\n\t\t\tcount\n\t\t\tdistribution\n\t\t}\n\t\tmangadexRating\n\t\tscan {\n\t\t\tavgIntervalHours\n\t\t\toverrideIntervalHours\n\t\t\tpollEveryMinutes\n\t\t\tpaused\n\t\t\tstatusOverride\n\t\t\tpausedOverride\n\t\t\tpollEveryMinutesOverride\n\t\t\tlastScannedAt\n\t\t\tnextScanAt\n\t\t}\n\t\tcreatedAt\n\t\tupdatedAt\n\t\t# Chapter recency. `updatedAt` is a metadata touch that moves on every poll,\n\t\t# so it can never be the "· 4h" label; `latestChapterAt` is the real publish\n\t\t# time and `detectedAt` is when we found it. Keep `detectedAt` on its own\n\t\t# line — the unknown-field fallback strips it by line (OPTIONAL_SERIES_FIELDS).\n\t\tlatestChapterAt\n\t\tdetectedAt\n\t\t# The newest chapter\'s NUMBER ("151"), which is NOT `chapterCount` ("12 ch") —\n\t\t# printing the count under a "Ch." label is the F4 bug the chapter-number contract\n\t\t# exists to prevent, so a null here means print the count ALONE. Own line, no alias\n\t\t# and no arguments, because the unknown-field fallback strips it by line\n\t\t# (OPTIONAL_SERIES_FIELDS) — this field is newer than the deployed API, and without\n\t\t# that entry a reader shipped first would fail EVERY series query rather than lose\n\t\t# one label.\n\t\tlatestChapter\n\t}\n\n\tquery Discovery {\n\t\tdiscovery {\n\t\t\tkind\n\t\t\ttitle\n\t\t\tgenre\n\t\t\titems {\n\t\t\t\t...SeriesFields\n\t\t\t}\n\t\t}\n\t}\n',
	UPDATES_FEED:
		"\n\tquery UpdatesFeed($page: Int, $type: ComicType) {\n\t\tupdatesFeed(page: $page, type: $type) {\n\t\t\titems {\n\t\t\t\tid\n\t\t\t\tworkId\n\t\t\t\ttitle\n\t\t\t\tcoverUrl\n\t\t\t\ttype\n\t\t\t\tstatus\n\t\t\t\tlatestChapter\n\t\t\t\tlatestChapterTitle\n\t\t\t\tchapterCount\n\t\t\t\treleasedAt\n\t\t\t\t# The feed's sort key, so also the clock the card labels with. Own line for\n\t\t\t\t# the unknown-field fallback (OPTIONAL_SERIES_FIELDS).\n\t\t\t\tannouncedAt\n\t\t\t\tdetectedAt\n\t\t\t\trating\n\t\t\t\treaderRating\n\t\t\t\tmangadexRating\n\t\t\t\tisNsfw\n\t\t\t}\n\t\t\tpage\n\t\t\thasNextPage\n\t\t\ttotal\n\t\t}\n\t}\n",
	SEARCH:
		'\n\t\n\tfragment SeriesFields on Series {\n\t\tid\n\t\ttitle\n\t\taltTitles\n\t\tauthor\n\t\tartist\n\t\tdescription\n\t\tgenres\n\t\ttype\n\t\tstatus\n\t\tcoverUrl\n\t\tsourceId\n\t\tchapterCount\n\t\tisMarked\n\t\tisNsfw\n\t\trating {\n\t\t\taverage\n\t\t\tcount\n\t\t\tdistribution\n\t\t}\n\t\tmangadexRating\n\t\tscan {\n\t\t\tavgIntervalHours\n\t\t\toverrideIntervalHours\n\t\t\tpollEveryMinutes\n\t\t\tpaused\n\t\t\tstatusOverride\n\t\t\tpausedOverride\n\t\t\tpollEveryMinutesOverride\n\t\t\tlastScannedAt\n\t\t\tnextScanAt\n\t\t}\n\t\tcreatedAt\n\t\tupdatedAt\n\t\t# Chapter recency. `updatedAt` is a metadata touch that moves on every poll,\n\t\t# so it can never be the "· 4h" label; `latestChapterAt` is the real publish\n\t\t# time and `detectedAt` is when we found it. Keep `detectedAt` on its own\n\t\t# line — the unknown-field fallback strips it by line (OPTIONAL_SERIES_FIELDS).\n\t\tlatestChapterAt\n\t\tdetectedAt\n\t\t# The newest chapter\'s NUMBER ("151"), which is NOT `chapterCount` ("12 ch") —\n\t\t# printing the count under a "Ch." label is the F4 bug the chapter-number contract\n\t\t# exists to prevent, so a null here means print the count ALONE. Own line, no alias\n\t\t# and no arguments, because the unknown-field fallback strips it by line\n\t\t# (OPTIONAL_SERIES_FIELDS) — this field is newer than the deployed API, and without\n\t\t# that entry a reader shipped first would fail EVERY series query rather than lose\n\t\t# one label.\n\t\tlatestChapter\n\t}\n\n\tquery Search(\n\t\t$query: String!\n\t\t$page: Int\n\t\t$genres: [String!]\n\t\t$minRating: Float\n\t\t$maxRating: Float\n\t\t$types: [ComicType!]\n\t\t$status: SeriesStatus\n\t\t$sort: BrowseSort\n\t\t$contentRating: ContentRatingFilter\n\t\t$includeNsfw: Boolean\n\t\t$hasChapters: Boolean\n\t\t$filter: BrowseFilter\n\t) {\n\t\tsearch(\n\t\t\tquery: $query\n\t\t\tpage: $page\n\t\t\tgenres: $genres\n\t\t\tminRating: $minRating\n\t\t\tmaxRating: $maxRating\n\t\t\ttypes: $types\n\t\t\tstatus: $status\n\t\t\tsort: $sort\n\t\t\tcontentRating: $contentRating\n\t\t\tincludeNsfw: $includeNsfw\n\t\t\thasChapters: $hasChapters\n\t\t\tfilter: $filter\n\t\t) {\n\t\t\titems {\n\t\t\t\t...SeriesFields\n\t\t\t}\n\t\t\tpage\n\t\t\thasNextPage\n\t\t\ttotal\n\t\t}\n\t}\n',
	SERIES_PAGE:
		'\n\t\n\tfragment SeriesFields on Series {\n\t\tid\n\t\ttitle\n\t\taltTitles\n\t\tauthor\n\t\tartist\n\t\tdescription\n\t\tgenres\n\t\ttype\n\t\tstatus\n\t\tcoverUrl\n\t\tsourceId\n\t\tchapterCount\n\t\tisMarked\n\t\tisNsfw\n\t\trating {\n\t\t\taverage\n\t\t\tcount\n\t\t\tdistribution\n\t\t}\n\t\tmangadexRating\n\t\tscan {\n\t\t\tavgIntervalHours\n\t\t\toverrideIntervalHours\n\t\t\tpollEveryMinutes\n\t\t\tpaused\n\t\t\tstatusOverride\n\t\t\tpausedOverride\n\t\t\tpollEveryMinutesOverride\n\t\t\tlastScannedAt\n\t\t\tnextScanAt\n\t\t}\n\t\tcreatedAt\n\t\tupdatedAt\n\t\t# Chapter recency. `updatedAt` is a metadata touch that moves on every poll,\n\t\t# so it can never be the "· 4h" label; `latestChapterAt` is the real publish\n\t\t# time and `detectedAt` is when we found it. Keep `detectedAt` on its own\n\t\t# line — the unknown-field fallback strips it by line (OPTIONAL_SERIES_FIELDS).\n\t\tlatestChapterAt\n\t\tdetectedAt\n\t\t# The newest chapter\'s NUMBER ("151"), which is NOT `chapterCount` ("12 ch") —\n\t\t# printing the count under a "Ch." label is the F4 bug the chapter-number contract\n\t\t# exists to prevent, so a null here means print the count ALONE. Own line, no alias\n\t\t# and no arguments, because the unknown-field fallback strips it by line\n\t\t# (OPTIONAL_SERIES_FIELDS) — this field is newer than the deployed API, and without\n\t\t# that entry a reader shipped first would fail EVERY series query rather than lose\n\t\t# one label.\n\t\tlatestChapter\n\t}\n\n\t\n\tfragment ChapterFields on Chapter {\n\t\tid\n\t\tseriesId\n\t\tnumber\n\t\tlabel\n\t\texternalUrl\n\t\ttitle\n\t\tpageCount\n\t\tuploadedAt\n\t\t# When WE first had it, as against uploadedAt\'s "when the source published it". On its\n\t\t# own line, no alias/args/sub-selection, and not the only selection here — the three\n\t\t# conditions the line-based unknown-field fallback needs (OPTIONAL_SERIES_FIELDS).\n\t\t# (No backticks in this comment: it is inside a JS template literal.)\n\t\tfoundAt\n\t\tscanlator\n\t\tread\n\t\tlastPageRead\n\t\tbookmarked\n\t\tisDownloaded\n\t}\n\n\t\n\tfragment WorkSourceFields on WorkSource {\n\t\tsourceType\n\t\tsourceId\n\t\tsourceKey\n\t\tsourceUrl\n\t\tisNsfw\n\t\tlang\n\t\textension {\n\t\t\tpkgName\n\t\t\trepoUrl\n\t\t\tapkName\n\t\t\tversionCode\n\t\t\tlang\n\t\t}\n\t}\n\n\tquery SeriesPage($workId: ID!, $withAggregated: Boolean! = false) {\n\t\tseriesPage(workId: $workId) {\n\t\t\tworkId\n\t\t\tseries {\n\t\t\t\t...SeriesFields\n\t\t\t\tmetadata {\n\t\t\t\t\tyear\n\t\t\t\t\toriginalLanguage\n\t\t\t\t\tdemographic\n\t\t\t\t\tcontentRating\n\t\t\t\t\ttags {\n\t\t\t\t\t\tid\n\t\t\t\t\t\tname\n\t\t\t\t\t\tgroup\n\t\t\t\t\t}\n\t\t\t\t\ttrackers {\n\t\t\t\t\t\tprovider\n\t\t\t\t\t\texternalId\n\t\t\t\t\t\turl\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t\tlibraryStatus\n\t\t\t\tisFavorite\n\t\t\t\tviews {\n\t\t\t\t\ttotal\n\t\t\t\t\tlast7d\n\t\t\t\t\tlast24h\n\t\t\t\t}\n\t\t\t\tlocalizedDescriptions {\n\t\t\t\t\tlang\n\t\t\t\t\tdescription\n\t\t\t\t}\n\t\t\t\tcredits {\n\t\t\t\t\trole\n\t\t\t\t\tname\n\t\t\t\t}\n\t\t\t\tcovers {\n\t\t\t\t\tfileName\n\t\t\t\t\turl\n\t\t\t\t\tthumbnailUrl\n\t\t\t\t\tlang\n\t\t\t\t\tvolume\n\t\t\t\t\tisPrimary\n\t\t\t\t}\n\t\t\t}\n\t\t\tspineChapters {\n\t\t\t\t...ChapterFields\n\t\t\t}\n\t\t\tsources {\n\t\t\t\tsource {\n\t\t\t\t\t...WorkSourceFields\n\t\t\t\t}\n\t\t\t\tchapters {\n\t\t\t\t\t...ChapterFields\n\t\t\t\t}\n\t\t\t\tseries {\n\t\t\t\t\t...SeriesFields\n\t\t\t\t\tlibraryStatus\n\t\t\t\t\tisFavorite\n\t\t\t\t\tviews {\n\t\t\t\t\t\ttotal\n\t\t\t\t\t\tlast7d\n\t\t\t\t\t\tlast24h\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t\taggregated @include(if: $withAggregated) {\n\t\t\t\tnumber\n\t\t\t\tlabel\n\t\t\t\ttitle\n\t\t\t\t# Own line, no alias — the unknown-field fallback strips it by line.\n\t\t\t\tfirstReleasedAt\n\t\t\t\tchapterVolume\n\t\t\t\t# One entry per COPY: every (source, scanlation group) upload of the number.\n\t\t\t\t# The two copy… fields are opportunistic (OPTIONAL_SERIES_FIELDS).\n\t\t\t\tsources {\n\t\t\t\t\tsourceType\n\t\t\t\t\tsourceId\n\t\t\t\t\tsuwayomiMangaId\n\t\t\t\t\tchapterId\n\t\t\t\t\tscanlator\n\t\t\t\t\tcopyReleasedAt\n\t\t\t\t\tcopyExternalUrl\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n',
	SET_PROGRESS:
		'\n\tmutation SetProgress($chapterId: ID!, $lastPageRead: Int!, $read: Boolean!) {\n\t\tsetProgress(chapterId: $chapterId, lastPageRead: $lastPageRead, read: $read)\n\t}\n',
	MARK: '\n\t\n\tfragment SeriesFields on Series {\n\t\tid\n\t\ttitle\n\t\taltTitles\n\t\tauthor\n\t\tartist\n\t\tdescription\n\t\tgenres\n\t\ttype\n\t\tstatus\n\t\tcoverUrl\n\t\tsourceId\n\t\tchapterCount\n\t\tisMarked\n\t\tisNsfw\n\t\trating {\n\t\t\taverage\n\t\t\tcount\n\t\t\tdistribution\n\t\t}\n\t\tmangadexRating\n\t\tscan {\n\t\t\tavgIntervalHours\n\t\t\toverrideIntervalHours\n\t\t\tpollEveryMinutes\n\t\t\tpaused\n\t\t\tstatusOverride\n\t\t\tpausedOverride\n\t\t\tpollEveryMinutesOverride\n\t\t\tlastScannedAt\n\t\t\tnextScanAt\n\t\t}\n\t\tcreatedAt\n\t\tupdatedAt\n\t\t# Chapter recency. `updatedAt` is a metadata touch that moves on every poll,\n\t\t# so it can never be the "· 4h" label; `latestChapterAt` is the real publish\n\t\t# time and `detectedAt` is when we found it. Keep `detectedAt` on its own\n\t\t# line — the unknown-field fallback strips it by line (OPTIONAL_SERIES_FIELDS).\n\t\tlatestChapterAt\n\t\tdetectedAt\n\t\t# The newest chapter\'s NUMBER ("151"), which is NOT `chapterCount` ("12 ch") —\n\t\t# printing the count under a "Ch." label is the F4 bug the chapter-number contract\n\t\t# exists to prevent, so a null here means print the count ALONE. Own line, no alias\n\t\t# and no arguments, because the unknown-field fallback strips it by line\n\t\t# (OPTIONAL_SERIES_FIELDS) — this field is newer than the deployed API, and without\n\t\t# that entry a reader shipped first would fail EVERY series query rather than lose\n\t\t# one label.\n\t\tlatestChapter\n\t}\n\n\tmutation Mark($seriesId: ID!, $marked: Boolean!) {\n\t\tmark(seriesId: $seriesId, marked: $marked) {\n\t\t\t...SeriesFields\n\t\t\tlibraryStatus\n\t\t\tisFavorite\n\t\t}\n\t}\n',
	RECORD_VIEW:
		'\n\tmutation RecordView($seriesId: ID!) {\n\t\trecordView(seriesId: $seriesId)\n\t}\n',
	POST_COMMENT:
		'\n\t\n\tfragment UserRefFields on UserRef {\n\t\tid\n\t\tusername\n\t\tavatarUrl\n\t}\n\n\tmutation PostComment($input: PostCommentInput!) {\n\t\tpostComment(input: $input) {\n\t\t\tid\n\t\t\ttargetType\n\t\t\ttargetId\n\t\t\tparentId\n\t\t\tauthor {\n\t\t\t\t...UserRefFields\n\t\t\t}\n\t\t\tbody\n\t\t\thasSpoiler\n\t\t\tmediaUrl\n\t\t\tmediaWidth\n\t\t\tmediaHeight\n\t\t\tcreatedAt\n\t\t\tlikes\n\t\t\tdislikes\n\t\t\tmyVote\n\t\t}\n\t}\n',
	DELETE_COMMENT:
		'\n\tmutation DeleteComment($commentId: ID!) {\n\t\tdeleteComment(commentId: $commentId)\n\t}\n',
	COMMENTS:
		'\n\t\n\tfragment UserRefFields on UserRef {\n\t\tid\n\t\tusername\n\t\tavatarUrl\n\t}\n\n\tquery Comments($targetType: String!, $targetId: ID!, $page: Int) {\n\t\tcomments(targetType: $targetType, targetId: $targetId, page: $page) {\n\t\t\titems {\n\t\t\t\tid\n\t\t\t\ttargetType\n\t\t\t\ttargetId\n\t\t\t\tparentId\n\t\t\t\tdeletedBy\n\t\t\t\tauthor {\n\t\t\t\t\t...UserRefFields\n\t\t\t\t}\n\t\t\t\tbody\n\t\t\t\thasSpoiler\n\t\t\t\tmediaUrl\n\t\t\t\tmediaWidth\n\t\t\t\tmediaHeight\n\t\t\t\tcreatedAt\n\t\t\t\tlikes\n\t\t\t\tdislikes\n\t\t\t\tmyVote\n\t\t\t}\n\t\t\tpage\n\t\t\thasNextPage\n\t\t\ttotal\n\t\t}\n\t}\n',
	REVIEWS:
		'\n\t\n\tfragment UserRefFields on UserRef {\n\t\tid\n\t\tusername\n\t\tavatarUrl\n\t}\n\n\tquery Reviews($seriesId: ID!, $page: Int) {\n\t\treviews(seriesId: $seriesId, page: $page) {\n\t\t\titems {\n\t\t\t\tid\n\t\t\t\tseriesId\n\t\t\t\tauthor {\n\t\t\t\t\t...UserRefFields\n\t\t\t\t}\n\t\t\t\tscore\n\t\t\t\tbody\n\t\t\t\thasSpoiler\n\t\t\t\tcreatedAt\n\t\t\t\tupdatedAt\n\t\t\t}\n\t\t\tpage\n\t\t\thasNextPage\n\t\t\ttotal\n\t\t}\n\t}\n',
	MY_REVIEW:
		'\n\t\n\tfragment UserRefFields on UserRef {\n\t\tid\n\t\tusername\n\t\tavatarUrl\n\t}\n\n\tquery MyReview($seriesId: ID!) {\n\t\tmyReview(seriesId: $seriesId) {\n\t\t\tid\n\t\t\tseriesId\n\t\t\tauthor {\n\t\t\t\t...UserRefFields\n\t\t\t}\n\t\t\tscore\n\t\t\tbody\n\t\t\thasSpoiler\n\t\t\tcreatedAt\n\t\t\tupdatedAt\n\t\t}\n\t}\n',
	CANONICAL_PAGES:
		'\n\tquery CanonicalPages($chapterId: ID!) {\n\t\tcanonicalPages(chapterId: $chapterId) {\n\t\t\tindex\n\t\t\tsourceUrl\n\t\t\twidth\n\t\t\theight\n\t\t}\n\t}\n',
	PAGES:
		'\n\tquery Pages($chapterId: ID!) {\n\t\tpages(chapterId: $chapterId) {\n\t\t\tindex\n\t\t\tsourceUrl\n\t\t\twidth\n\t\t\theight\n\t\t}\n\t}\n',
	SESSION:
		'\n\t\n\t\n\tfragment SessionUserFields on SessionUser {\n\t\tid\n\t\tusername\n\t\tdisplayName\n\t\tbio\n\t\tavatarUrl\n\t\tisAdmin\n\t\tshowNsfw\n\t\tjoinedAt\n\t\tmustChangePassword\n\t}\n\n\tfragment SessionFields on Session {\n\t\ttoken\n\t\tuser {\n\t\t\t...SessionUserFields\n\t\t}\n\t}\n\n\tquery Session {\n\t\tsession {\n\t\t\t...SessionFields\n\t\t}\n\t}\n',
	LOGIN:
		'\n\t\n\t\n\tfragment SessionUserFields on SessionUser {\n\t\tid\n\t\tusername\n\t\tdisplayName\n\t\tbio\n\t\tavatarUrl\n\t\tisAdmin\n\t\tshowNsfw\n\t\tjoinedAt\n\t\tmustChangePassword\n\t}\n\n\tfragment SessionFields on Session {\n\t\ttoken\n\t\tuser {\n\t\t\t...SessionUserFields\n\t\t}\n\t}\n\n\tmutation Login($username: String!, $password: String!) {\n\t\tlogin(username: $username, password: $password) {\n\t\t\t...SessionFields\n\t\t}\n\t}\n',
	REGISTER:
		'\n\t\n\t\n\tfragment SessionUserFields on SessionUser {\n\t\tid\n\t\tusername\n\t\tdisplayName\n\t\tbio\n\t\tavatarUrl\n\t\tisAdmin\n\t\tshowNsfw\n\t\tjoinedAt\n\t\tmustChangePassword\n\t}\n\n\tfragment SessionFields on Session {\n\t\ttoken\n\t\tuser {\n\t\t\t...SessionUserFields\n\t\t}\n\t}\n\n\tmutation Register($input: RegisterInput!) {\n\t\tregister(input: $input) {\n\t\t\t...SessionFields\n\t\t}\n\t}\n',
	UNREAD_NOTIFICATION_COUNT:
		'\n\tquery UnreadNotificationCount {\n\t\tunreadNotificationCount\n\t}\n',
};

// ---------------------------------------------------------------------------
// SESSION MODEL: what one active reader does, and where the reader code says so
// ---------------------------------------------------------------------------
// apps/reader/src, checked 2026-10-13:
//   home open        discovery + search(sort LATEST_ADDED) + updatesFeed(page 1), in
//                    parallel                      lib/data/source.ts:1301-1315,
//                                                  lib/data/home-latest-added.ts:8-12
//   series open      seriesPage(withAggregated: true)       lib/data/source.ts:2300, 783
//                    then reviews, then myReview            lib/data/social-repo.ts:207-216
//   chapter open     seriesPage(withAggregated: true) again: the reader route keeps its
//                    own memo, 60 s long, so a chapter read for longer than a minute
//                    refetches it                 lib/data/source.ts:3243, 3282-3299
//                    then recordView (once per chapter)  routes/read/[slug]/+page.svelte:254-262
//                    and the chapter's comment thread    routes/read/[slug]/+page.svelte:1469-1474
//                    The page list (canonicalPages / pages, lib/data/source.ts:3059-3063)
//                    is NOT sent unless --with-pages: it makes the server call MangaDex
//                    or a source site, which measures them, not this server.
//   progress writes  The reader has NO per-page write and NO timer. setProgress is sent
//                    - once when the last page is reached (read: true)
//                                                  routes/read/[slug]/+page.svelte:367-375
//                    - on every navigation away    routes/read/[slug]/+page.svelte:381-385
//                    - when the tab is hidden      routes/read/[slug]/+page.svelte:386-392
//                    - when the reader page is destroyed    routes/read/[slug]/+page.svelte:393
//                    So a chapter read to the end costs 2 writes (end + next chapter), 3
//                    when the reader then leaves the reader page, and a chapter left
//                    half-way costs 2 (navigation + destroy, read: false).
//   library mark     mark(seriesId, marked)                 lib/data/source.ts:2499
// The time a chapter takes is therefore what sets the write rate. The reader code has
// no number for it: `--page-seconds` (default 6) times the chapter's real page count
// (`--chapter-pages`, default 20, when the server reports none) is an ASSUMPTION.

// ---------------------------------------------------------------------------
// IMAGE MODEL: which image addresses the reader loads, and how they are built
// ---------------------------------------------------------------------------
// packages/api/src/image-provider.ts (WebImageProvider), checked 2026-10-14:
//   own-origin assets  relative `/covers/<f>.webp`, `/avatars/<f>`, and the engine proxy
//                      `/api/v1/manga/<id>/thumbnail` and
//                      `/api/v1/manga/<m>/chapter/<c>/page/<p>` load straight from the API
//                      origin with `cache_policy=2` added (isOwnAsset, versionedOwnAsset,
//                      CACHE_POLICY_REVISION). Server side: apps/server/src/main.rs
//                      public_router (`/covers/{file}`, `/avatars/{file}`,
//                      `/api/v1/manga/{*rest}` limited by is_suwayomi_image_path).
//   card covers        a resizable own cover is drawn at `?w=192` on cards
//                      (coverCardSrcSet, CARD_COVER_WIDTH, RESIZABLE_COVER_PATHS).
//   foreign hosts      any other absolute address (MangaDex) goes through a proxy as
//                      `<proxy>/img?src=<encoded>&cache_policy=2` (resolve). The hosted
//                      site's proxy is the image Worker; a self-host server mounts `/img`
//                      itself (img_proxy, KOMIKA_IMG_PROXY=on). `--img-base` picks it,
//                      default `--target`.
//   page lists         canonicalPages (MangaDex ids) / pages (numeric source ids) give
//                      each page's `sourceUrl` (lib/data/source.ts:3059-3063).
// An absolute own-asset address on a host in `--own-hosts` (the server's public URL, when
// it differs from `--target`) is loaded from `--target`. One on a production host is
// rewritten to `--target` too, and never fetched where it points.

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

const SCENARIOS = {
	read: { shape: 'loop', accounts: false, images: false, pool: true },
	write: { shape: 'loop', accounts: true, images: false, pool: true },
	session: { shape: 'users', accounts: true, images: false, pool: true },
	'image-warm': { shape: 'loop', accounts: false, images: true, pool: true },
	'image-cold': { shape: 'loop', accounts: false, images: false, pool: false, fixture: true },
	mixed: { shape: 'users', accounts: true, images: true, pool: true },
	'release-burst': { shape: 'users', accounts: true, images: true, pool: true },
};
const SCENARIO_NAMES = Object.keys(SCENARIOS).join(' | ');

const FLAGS = {
	'--target': ['target', 'str', 'http://127.0.0.1:8080', 'base URL of the server'],
	'--scenario': ['scenario', 'str', 'read', SCENARIO_NAMES],
	'--suite': ['suite', 'str', null, 'JSON file: steps run in order into one --json file'],
	'--label': ['label', 'str', null, 'step name in the output (default: the scenario)'],
	'--vus': ['vus', 'list', null, 'closed-loop ramp for loop scenarios (default 1,2,4,8,16,32,64)'],
	'--rate': ['rate', 'list', null, 'open-loop ramp for loop scenarios, requests per second'],
	'--users': ['users', 'list', null, 'ramp for session/mixed/release-burst (default 10,25,50,100)'],
	'--duration': ['duration', 'num', 20, 'measured seconds per step'],
	'--warmup': ['warmup', 'num', 5, 'unmeasured seconds before each step'],
	'--max-p95': ['maxP95', 'num', 750, 'stop the ramp above this p95, ms (reads or writes)'],
	'--max-image-p95': ['maxImageP95', 'num', 5000, 'stop the ramp above this image p95, ms'],
	'--max-errors': ['maxErrors', 'num', 0.01, 'stop the ramp above this error share'],
	'--timeout': ['timeout', 'num', 30, 'request timeout, seconds'],
	'--json': [
		'json',
		'str',
		null,
		'write the full result to this file (suite default: ./capacity-<run>.json)',
	],
	'--ops': ['ops', 'bool', false, 'print a per-operation table after each step'],
	'--seed': [
		'seed',
		'str',
		null,
		'seed for pool choice and user paths (default: random, recorded)',
	],
	'--metrics-url': ['metricsUrl', 'str', null, 'Prometheus text endpoint scraped around each step'],
	'--metrics-interval': ['metricsInterval', 'num', 5, 'seconds between metrics samples in a step'],
	'--host-cmd': [
		'hostCmd',
		'str',
		null,
		'shell command per step, {label} {seconds}; JSON stdout kept',
	],
	'--host-timeout': ['hostTimeout', 'num', 120, 'seconds a host command may run past its window'],
	'--host-ready-timeout': [
		'hostReadyTimeout',
		'num',
		30,
		'seconds to wait for the host command READY line',
	],
	'--image-digest': ['imageDigest', 'str', null, 'server image digest, recorded in the manifest'],
	'--settings-file': ['settingsFile', 'str', null, "the server's effective environment, recorded"],
	'--accounts': ['accounts', 'num', null, 'test accounts (default: the largest step)'],
	'--prefix': ['prefix', 'str', 'cap', 'account name prefix (names are <prefix>00001 ...)'],
	'--password': ['password', 'str', null, 'account password (default: generated, kept in --state)'],
	'--state': ['state', 'str', './capacity-state.json', 'file for the password and tokens'],
	'--share-accounts': ['shareAccounts', 'bool', false, 'run even with fewer accounts than users'],
	'--pool': ['pool', 'num', 200, 'work ids to collect'],
	'--sample': ['sample', 'num', null, 'works to open for chapter ids (default: the whole pool)'],
	'--setup-rate': ['setupRate', 'num', 50, 'cap on setup requests per second'],
	'--setup-concurrency': ['setupConcurrency', 'num', 2, 'parallel series page calls in setup'],
	'--prewarm-max-ms': ['prewarmMax', 'num', 3000, 'leave out works whose series page stays slower'],
	'--setup-only': ['setupOnly', 'bool', false, 'do the setup, print it, send no load'],
	'--hot-works': ['hotWorks', 'num', null, 'concentrate every virtual user on this many works'],
	'--progress-pattern': [
		'progressPattern',
		'str',
		'random',
		'write: sequential | random progress writes',
	],
	'--typo-share': ['typoShare', 'num', 0.1, 'share of searches with a one-letter typo'],
	'--mix': ['mix', 'str', null, 'weights, e.g. discovery=4,series=5 (see --list)'],
	'--anon-method': ['anonMethod', 'str', 'get', 'get | post for anonymous reads'],
	'--no-gzip': ['noGzip', 'bool', false, 'do not ask for gzip responses'],
	'--page-seconds': ['pageSeconds', 'num', 6, 'session: seconds a reader spends on one page'],
	'--chapter-pages': ['chapterPages', 'num', 20, 'session: pages when a chapter reports none'],
	'--chapters': ['chapters', 'num', 3, 'session: chapters read per series visit'],
	'--think': ['think', 'num', 8, 'session: seconds between page opens'],
	'--next-seconds': [
		'nextSeconds',
		'num',
		3,
		"session: seconds from a chapter's last page to the next chapter",
	],
	'--quit-share': ['quitShare', 'num', 0.15, 'session: chapters left half-way'],
	'--mark-share': ['markShare', 'num', 0.1, 'session: series opens followed by a library mark'],
	'--search-share': ['searchShare', 'num', 0.2, 'session: home opens followed by a search'],
	'--comment-share': [
		'commentShare',
		'num',
		0.02,
		'session: chapters with a comment (posted, then deleted)',
	],
	'--with-pages': [
		'withPages',
		'bool',
		false,
		'session: also request the page list (calls upstream)',
	],
	'--img-base': [
		'imgBase',
		'str',
		null,
		'image proxy origin for foreign image URLs (default --target)',
	],
	'--own-hosts': [
		'ownHosts',
		'str',
		null,
		'hosts whose own-asset URLs load from --target (comma list)',
	],
	'--image-chapters': [
		'imageChapters',
		'num',
		12,
		'image steps: chapters whose page list is fetched',
	],
	'--image-urls': [
		'imageUrls',
		'num',
		400,
		'image steps: cap on harvested image URLs (= warm pass)',
	],
	'--prefetch-pages': [
		'prefetchPages',
		'num',
		3,
		'mixed/release-burst: page images loaded at chapter open',
	],
	'--fixture-images': [
		'fixtureImages',
		'str',
		null,
		'fixture images: URL file, or a URL template with {n} / {run}',
	],
	'--fixture-start': ['fixtureStart', 'num', 1, 'first {n} of a --fixture-images template'],
	'--real-source-sample': [
		'realSourceSample',
		'num',
		0,
		`real-source image fetches allowed in the whole run (max ${200})`,
	],
	'--burst-works': [
		'burstWorks',
		'num',
		1,
		'release-burst: works whose newest chapter everyone opens',
	],
	'--bursts': ['bursts', 'num', 1, 'release-burst: openings per user'],
	'--burst-gap': ['burstGap', 'num', 60, 'release-burst: seconds between openings'],
	'--stop-file': ['stopFile', 'str', null, 'stop sending as soon as this file exists'],
	'--queries': ['queries', 'str', null, 'JSON file overriding operation text'],
	'--list': ['list', 'bool', false, 'print scenarios and mixes, then exit'],
};
const KEY_TO_FLAG = Object.fromEntries(Object.entries(FLAGS).map(([flag, [key]]) => [key, flag]));

// Run-wide options: one value for the whole run. A suite may set them in `defaults`,
// never in a step, and an explicit command-line flag wins.
const GLOBAL_KEYS = new Set([
	'target',
	'suite',
	'json',
	'seed',
	'metricsUrl',
	'metricsInterval',
	'hostCmd',
	'hostTimeout',
	'imageDigest',
	'settingsFile',
	'accounts',
	'prefix',
	'password',
	'state',
	'shareAccounts',
	'pool',
	'sample',
	'setupRate',
	'setupConcurrency',
	'prewarmMax',
	'setupOnly',
	'imgBase',
	'ownHosts',
	'imageChapters',
	'imageUrls',
	'fixtureImages',
	'fixtureStart',
	'stopFile',
	'queries',
	'list',
	'realSourceSample',
	'hostReadyTimeout',
	'anonMethod',
	'noGzip',
]);
// What a suite step defines; the command line may not set these next to --suite.
const STEP_ONLY_KEYS = new Set(['scenario', 'label', 'vus', 'rate', 'users']);

class UsageError extends Error {}

function die(msg, code = 1) {
	console.error(`error: ${msg}`);
	process.exit(code);
}

function usage() {
	console.log('komika capacity probe\n');
	for (const [flag, [, type, def, help]] of Object.entries(FLAGS)) {
		const arg =
			type === 'bool' ? '' : type === 'list' ? ' a,b,c' : type === 'num' ? ' <n>' : ' <s>';
		const d = def === null || def === false ? '' : `  (default ${def})`;
		console.log(`  ${(flag + arg).padEnd(26)} ${help}${d}`);
	}
}

function defaults() {
	const out = {};
	for (const [, [key, , def]] of Object.entries(FLAGS)) out[key] = def;
	return out;
}

/** Set one option from a flag (CLI) or a suite entry. Returns the internal key. */
function setOption(out, flag, v, where = '') {
	const spec = FLAGS[flag];
	if (!spec) throw new UsageError(`${where}unknown option: ${flag}`);
	const [key, type] = spec;
	if (type === 'bool') {
		out[key] = v === undefined ? true : Boolean(v);
		return key;
	}
	if (v === undefined || v === null) throw new UsageError(`${where}${flag} needs a value`);
	if (type === 'num') {
		const n = Number(v);
		if (!Number.isFinite(n) || n < 0) throw new UsageError(`${where}${flag}: not a number: ${v}`);
		out[key] = n;
	} else if (type === 'list') {
		const parts = Array.isArray(v) ? v : String(v).split(',');
		const nums = parts.map(Number);
		if (!nums.length || nums.some((n) => !Number.isFinite(n) || n <= 0))
			throw new UsageError(`${where}${flag}: expected positive numbers, got ${v}`);
		out[key] = nums;
	} else out[key] = String(v);
	return key;
}

/** The command line: only the options given explicitly. */
export function parseCli(argv) {
	const explicit = {};
	let help = false;
	for (let i = 0; i < argv.length; i++) {
		const a = argv[i];
		if (a === '-h' || a === '--help') {
			help = true;
			continue;
		}
		const spec = FLAGS[a];
		if (!spec) throw new UsageError(`unknown arg: ${a}`);
		if (spec[1] === 'bool') setOption(explicit, a, true);
		else setOption(explicit, a, argv[++i]);
	}
	return { explicit, help };
}

/** A suite entry: `{"progress-pattern": "sequential"}` or `{"progressPattern": ...}`. */
function suiteOptions(obj, where, allowGlobal) {
	const out = {};
	for (const [k, v] of Object.entries(obj ?? {})) {
		if (['name', 'requires', 'note'].includes(k)) continue;
		const flag = FLAGS[`--${k}`] ? `--${k}` : KEY_TO_FLAG[k];
		if (!flag) throw new UsageError(`${where}unknown option: ${k}`);
		const key = setOption(out, flag, v, where);
		if (!allowGlobal && GLOBAL_KEYS.has(key))
			throw new UsageError(
				`${where}${flag} is run-wide: put it in the suite's "defaults" or on the command line`,
			);
	}
	return out;
}

/** Validate one step's effective options. Throws UsageError. */
function validateStep(o) {
	const sc = SCENARIOS[o.scenario];
	if (!sc) throw new UsageError(`unknown scenario '${o.scenario}' (${SCENARIO_NAMES})`);
	if (!['get', 'post'].includes(o.anonMethod)) throw new UsageError('--anon-method is get or post');
	if (!['sequential', 'random'].includes(o.progressPattern))
		throw new UsageError('--progress-pattern is sequential or random');
	if (!Number.isInteger(o.realSourceSample) || o.realSourceSample > REAL_SOURCE_MAX)
		throw new UsageError(
			`--real-source-sample is a whole number from 0 to ${REAL_SOURCE_MAX} (real-source image fetches in the whole run)`,
		);
	if (!/^[A-Za-z0-9_.-]{1,24}$/.test(o.prefix))
		throw new UsageError('--prefix: 1-24 characters from letters, digits and _ - .');
	if (sc.shape === 'users' && (o.vus || o.rate))
		throw new UsageError(`${o.scenario} is driven by --users, not --vus/--rate`);
	if (sc.shape === 'loop' && o.users)
		throw new UsageError(`--users belongs to session, mixed and release-burst`);
	if (o.vus && o.rate) throw new UsageError('--vus and --rate are mutually exclusive');
	if (o.hotWorks !== null && o.hotWorks < 1) throw new UsageError('--hot-works needs at least 1');
	if (o.mix && !['read', 'write'].includes(o.scenario))
		throw new UsageError('--mix applies to read and write');
	if (o.mix) mixFor(o);
}

// Weights. `read` and `write` pick ONE request per turn from these.
const BASE_MIX = {
	read: { discovery: 4, updates: 4, latestAdded: 4, browse: 3, series: 5, search: 3 },
	write: { progress: 12, mark: 4, comment: 1 },
};
function mixFor(o) {
	const base = BASE_MIX[o.scenario];
	if (!base) return null;
	const table = { ...base };
	if (o.mix) {
		for (const part of o.mix.split(',')) {
			const [k, v] = part.split('=');
			if (!(k in table) || !Number.isFinite(Number(v)) || Number(v) < 0)
				throw new UsageError(`--mix: bad entry '${part}' (keys: ${Object.keys(table).join(', ')})`);
			table[k] = Number(v);
		}
	}
	if (!Object.values(table).some((w) => w > 0))
		throw new UsageError('every weight in the mix is zero');
	return table;
}

function loadsFor(o) {
	const sc = SCENARIOS[o.scenario];
	if (sc.shape === 'users')
		return (o.users ?? [10, 25, 50, 100]).map((n) => ({ users: Math.round(n) }));
	if (o.rate) return o.rate.map((n) => ({ rate: n }));
	return (o.vus ?? [1, 2, 4, 8, 16, 32, 64]).map((n) => ({ vus: Math.round(n) }));
}

// ---------------------------------------------------------------------------
// Safety: production is never a target
// ---------------------------------------------------------------------------

// The production addresses come from configuration, never from the source:
// KOMIKA_PRODUCTION_IPS and deploy/production-ips.local (see SAFETY at the top).
export const PRODUCTION_IPS_ENV = 'KOMIKA_PRODUCTION_IPS';
export const PRODUCTION_IPS_FILE = resolvePath(
	dirname(fileURLToPath(import.meta.url)),
	'../../../deploy/production-ips.local',
);
export const PRODUCTION_UNCONFIGURED_WARNING =
	`warning: no production address is configured (${PRODUCTION_IPS_ENV} or deploy/production-ips.local);\n` +
	'         only komiq.cc and *.komiq.cc are refused.';

export class ProductionConfigError extends Error {}

/** The four bytes of a plain dotted IPv4 address (no leading zeros), or null. */
function ipv4Literal(text) {
	if (!/^(0|[1-9]\d{0,2})(\.(0|[1-9]\d{0,2})){3}$/.test(text)) return null;
	const b = text.split('.').map(Number);
	return b.every((x) => x <= 255) ? b : null;
}

/**
 * The configured production addresses, deduplicated: the union of the environment
 * variable (commas and/or whitespace between entries) and the local file (one per line,
 * `#` starts a comment). A missing file is no addresses; any entry that is not an IPv4
 * address, or a file that cannot be read, throws ProductionConfigError.
 */
export function readProductionAddresses({ env = process.env, file = PRODUCTION_IPS_FILE } = {}) {
	const found = new Set();
	const bad = [];
	const add = (entry, where) => {
		const b = ipv4Literal(entry);
		if (b) found.add(b.join('.'));
		else bad.push(`${where}: '${entry}'`);
	};
	for (const entry of String(env[PRODUCTION_IPS_ENV] ?? '').split(/[\s,]+/))
		if (entry) add(entry, PRODUCTION_IPS_ENV);
	let text = null;
	try {
		text = readFileSync(file, 'utf8');
	} catch (e) {
		if (e.code !== 'ENOENT')
			throw new ProductionConfigError(`${file} cannot be read: ${e.code || e.message}`);
	}
	if (text !== null)
		text.split(/\r?\n/).forEach((line, i) => {
			const entry = line.replace(/#.*/, '').trim();
			if (entry) add(entry, `${file}:${i + 1}`);
		});
	if (bad.length) throw new ProductionConfigError(`not an IPv4 address: ${bad.join(', ')}`);
	return [...found];
}

let productionBytes = null;
let productionAddressText = null;

/** Use these addresses (dotted IPv4 strings) as production from now on. */
export function setProductionAddresses(list) {
	const bytes = list.map((a) => {
		const b = ipv4Literal(String(a).trim());
		if (!b) throw new ProductionConfigError(`not an IPv4 address: '${a}'`);
		return b;
	});
	productionBytes = bytes;
	const escaped = bytes.map((b) => b.join('\\.'));
	productionAddressText = escaped.length
		? new RegExp(`(^|[^a-z0-9-])(${escaped.join('|')})(?![a-z0-9-])`, 'i')
		: null;
}

/**
 * Read the configuration, use it, and warn once (through `warn`) when it names no
 * address. Throws ProductionConfigError on a bad entry.
 */
export function loadProductionConfig({
	env = process.env,
	file = PRODUCTION_IPS_FILE,
	warn = (m) => console.error(m),
} = {}) {
	const list = readProductionAddresses({ env, file });
	setProductionAddresses(list);
	if (!list.length) warn(PRODUCTION_UNCONFIGURED_WARNING);
	return list;
}

/** The configured addresses as bytes; read from the defaults on first use. */
function productionAddresses() {
	if (!productionBytes) setProductionAddresses(readProductionAddresses());
	return productionBytes;
}

const isProductionBytes = (b) => !!b && productionAddresses().some((p) => sameBytes(b, p));

/** 16 bytes of an IPv6 address (with or without brackets), or null. */
function ipv6Bytes(text) {
	let h = String(text)
		.replace(/^\[|\]$/g, '')
		.split('%')[0];
	if (!h.includes(':')) return null;
	let tail = [];
	const v4 = /(\d+\.\d+\.\d+\.\d+)$/.exec(h);
	if (v4) {
		const parts = v4[1].split('.').map(Number);
		if (parts.some((x) => x > 255)) return null;
		tail = parts;
		h = h.slice(0, -v4[1].length) + '0:0';
	}
	const halves = h.split('::');
	if (halves.length > 2) return null;
	const words = (x) => (x ? x.split(':') : []);
	const left = words(halves[0]);
	const right = halves.length === 2 ? words(halves[1]) : [];
	const fill = 8 - left.length - right.length;
	if (halves.length === 1 ? left.length !== 8 : fill < 0) return null;
	const all = [...left, ...Array(halves.length === 2 ? fill : 0).fill('0'), ...right];
	const out = [];
	for (const w of all) {
		if (!/^[0-9a-f]{1,4}$/i.test(w)) return null;
		const n = parseInt(w, 16);
		out.push(n >> 8, n & 255);
	}
	if (tail.length) out.splice(12, 4, ...tail);
	return out.length === 16 ? out : null;
}

/** The IPv4 address an IPv6 address carries: mapped, compatible, SIIT, NAT64, 6to4. */
function embeddedIpv4(b) {
	const zero = (from, to) => b.slice(from, to).every((x) => x === 0);
	if (zero(0, 10) && b[10] === 0xff && b[11] === 0xff) return b.slice(12); // ::ffff:a.b.c.d
	if (zero(0, 12)) return b.slice(12); // ::a.b.c.d (compatible)
	if (zero(0, 8) && b[8] === 0xff && b[9] === 0xff && zero(10, 12)) return b.slice(12); // ::ffff:0:a.b.c.d
	if (b[0] === 0x00 && b[1] === 0x64 && b[2] === 0xff && b[3] === 0x9b && zero(4, 12))
		return b.slice(12); // 64:ff9b::/96
	if (b[0] === 0x20 && b[1] === 0x02) return b.slice(2, 6); // 2002:AABB:CCDD::/16
	return null;
}

const sameBytes = (a, b) => !!a && a.length === b.length && a.every((x, i) => x === b[i]);

/**
 * True for komiq.cc, any *.komiq.cc and the production address in every spelling:
 * dotted, one number, hex or octal parts (normalised by WHATWG URL parsing), and IPv6
 * forms that carry it (::ffff:…, ::…, 64:ff9b::…, 2002:…). Addresses are compared as
 * bytes, never as text.
 */
export function isProductionHost(host) {
	let h = String(host ?? '')
		.trim()
		.toLowerCase()
		.replace(/\.+$/, '');
	if (!h) return false;
	const bare = h.replace(/^\[|\]$/g, '');
	if (bare.includes(':')) {
		const b = ipv6Bytes(bare);
		return b ? isProductionBytes(embeddedIpv4(b)) : false;
	}
	try {
		h = new URL(`http://${bare}/`).hostname.replace(/\.+$/, '');
	} catch {
		return false;
	}
	if (/^\d+\.\d+\.\d+\.\d+$/.test(h)) return isProductionBytes(h.split('.').map(Number));
	return h === 'komiq.cc' || h.endsWith('.komiq.cc');
}

const PRODUCTION_NAME_TEXT = /(^|[^a-z0-9-])((?:[a-z0-9-]+\.)*komiq\.cc)(?![a-z0-9-])/i;

/** True when text names komiq.cc or a configured address as written. */
function namesProductionText(text) {
	productionAddresses();
	return PRODUCTION_NAME_TEXT.test(text) || (productionAddressText?.test(text) ?? false);
}

/**
 * True when command text names production in any spelling. The command is NOT parsed
 * as ssh or a shell: every word-like piece of it is tried as a host. The operator owns
 * what the command does; this only refuses one that visibly points at production.
 */
export function commandNamesProduction(cmd) {
	const text = String(cmd ?? '');
	if (namesProductionText(text)) return true;
	for (const raw of text.split(/[\s'"`=@,;|&()<>{}]+/)) {
		if (!raw) continue;
		const tok = raw.replace(/^[a-z][a-z0-9+.-]*:\/\//i, '');
		const pieces = new Set([tok, tok.split('/')[0]]);
		for (const m of tok.matchAll(/\[([^\]]+)\]/g)) pieces.add(m[1]);
		for (const p of [...pieces]) {
			if ((p.match(/:/g) ?? []).length === 1) pieces.add(p.split(':')[0]);
			for (const part of p.split(/[/:]/)) if (part) pieces.add(part);
		}
		for (const p of pieces) if (isProductionHost(p)) return true;
	}
	return false;
}

/** The host of a URL, or null. */
function hostOfUrl(value) {
	try {
		return new URL(value).hostname;
	} catch {
		return null;
	}
}

/**
 * Null, or why this run must not start. Every address the tool will contact is checked
 * here, before any request: target, metrics, image proxy, own hosts and every fixture
 * origin (`fixtureOrigins`, read from --fixture-images before this is called).
 */
export function productionRefusal(o, fixtureOrigins = []) {
	const addresses = [
		['--target', o.target],
		['--metrics-url', o.metricsUrl],
		['--img-base', o.imgBase],
	];
	if (o.fixtureImages && /^https?:\/\//i.test(o.fixtureImages))
		addresses.push(['--fixture-images', o.fixtureImages.replace(/\{(n|run)\}/g, '0')]);
	for (const origin of fixtureOrigins) addresses.push(['--fixture-images', origin]);
	for (const [flag, value] of addresses) {
		if (!value) continue;
		const host = hostOfUrl(value);
		if (host === null) continue; // not a URL: the normal validation reports it
		if (isProductionHost(host)) return `${flag} ${redactUrl(value)} is production (${host})`;
		if (urlQueryNamesProduction(value))
			return `${flag} ${redactUrl(value)} carries a production address in its query`;
	}
	for (const h of String(o.ownHosts ?? '').split(',')) {
		if (h.trim() && isProductionHost(h)) return `--own-hosts names production (${h.trim()})`;
	}
	if (o.hostCmd && commandNamesProduction(o.hostCmd)) return '--host-cmd names a production host';
	return null;
}

/** A query value that is (or contains) a production address, e.g. `/img?src=https://api.komiq.cc/…`. */
export function urlQueryNamesProduction(value) {
	let u;
	try {
		u = new URL(value, 'http://query-check.invalid');
	} catch {
		return false;
	}
	for (const [, v] of u.searchParams) {
		if (namesProductionText(v)) return true;
		const h = hostOfUrl(v);
		if (h && isProductionHost(h)) return true;
		if (commandNamesProduction(v)) return true;
	}
	return false;
}

/** A URL with any password in it replaced. */
export function redactUrl(value) {
	return String(value ?? '').replace(/(\/\/[^:/@\s]+:)[^@\s/]+@/g, '$1<redacted>@');
}

const SECRET_ASSIGNMENT =
	/\b([A-Za-z0-9_]*(?:pass|secret|token|key|credential|auth|private|pwd)[A-Za-z0-9_]*)=("[^"]*"|'[^']*'|\S+)/gi;

/**
 * Command text safe to record: secret-looking assignments (PGPASSWORD=…, TOKEN=…), the
 * value after `sshpass -p`, `--password …` and passwords inside URLs are replaced. The
 * raw command is never written anywhere.
 */
export function redactCommand(cmd) {
	if (cmd === null || cmd === undefined) return cmd;
	return redactUrl(String(cmd))
		.replace(SECRET_ASSIGNMENT, '$1=<redacted>')
		.replace(/(\bsshpass\b(?:\s+-[a-oq-zA-Z]+)*\s+-p)\s*("[^"]*"|'[^']*'|\S+)/g, '$1 <redacted>')
		.replace(
			/(--?(?:password|passwd|pass|token|secret)(?:=|\s+))("[^"]*"|'[^']*'|\S+)/gi,
			'$1<redacted>',
		);
}

function refuseProduction(why) {
	console.error(
		`refusing to run: ${why}.\n` +
			'       This tool registers accounts and sends load. Production (komiq.cc, *.komiq.cc and\n' +
			'       the configured production addresses) is never a target, and there is no override.\n' +
			'       Use the test host.',
	);
	process.exit(3);
}

/** The names may still point at production: look every origin up. A failed lookup is recorded. */
async function productionByDns(urls) {
	const looked = {};
	for (const u of urls) {
		if (!u) continue;
		let host;
		try {
			host = new URL(u).hostname.replace(/^\[|\]$/g, '');
		} catch {
			continue;
		}
		if (host in looked) continue;
		try {
			const all = await dns.promises.lookup(host, { all: true });
			looked[host] = all.map((a) => a.address);
			if (looked[host].some(isProductionHost))
				return { why: `${redactUrl(u)} resolves to production (${host})`, looked };
		} catch (e) {
			looked[host] = `lookup failed: ${e.code || e.message}`;
		}
	}
	return { why: null, looked };
}

// ---------------------------------------------------------------------------
// Seeded randomness
// ---------------------------------------------------------------------------

function hash32(s) {
	let h = 2166136261 >>> 0;
	for (let i = 0; i < s.length; i++) {
		h ^= s.charCodeAt(i);
		h = Math.imul(h, 16777619);
	}
	return h >>> 0;
}

/** mulberry32, one stream per (seed, name): the same seed gives the same choices. */
export function makeRng(seed, stream = '') {
	let a = hash32(`${seed}\u0000${stream}`);
	return () => {
		a = (a + 0x6d2b79f5) | 0;
		let t = Math.imul(a ^ (a >>> 15), 1 | a);
		t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
		return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
	};
}

const pickOf = (a, r) => a[(r() * a.length) | 0];
const randInt = (n, r) => (r() * n) | 0;
const jitter = (spread, r) => 1 - spread + r() * 2 * spread;
const shuffle = (a, r) => {
	for (let i = a.length - 1; i > 0; i--) {
		const j = (r() * (i + 1)) | 0;
		[a[i], a[j]] = [a[j], a[i]];
	}
	return a;
};
const sha256 = (data) => createHash('sha256').update(data).digest('hex');

// ---------------------------------------------------------------------------
// Run state. `args` is the CURRENT step's effective options; it is replaced per step.
// ---------------------------------------------------------------------------

let args = defaults();
let GLOBAL = null; // run-wide options
let CTX = null; // the current step: mix, chosen works, images
let POOL = null;
let IMAGES = null;
let base = null;
let GQL_PATH = '/graphql';
let OWN_HOSTS = new Set();
let IMG_BASE = null;
const RUN_ID = `${new Date().toISOString().replace(/[-:]/g, '').slice(0, 15)}-${randomBytes(3).toString('hex')}`;

// ---------------------------------------------------------------------------
// HTTP driver
// ---------------------------------------------------------------------------

// One keep-alive Agent per origin, no socket cap: a cap would queue requests inside the
// client and we would be measuring our own generator.
const agents = new Map();
function agentFor(u) {
	const key = `${u.protocol}//${u.host}`;
	let a = agents.get(key);
	if (!a) {
		const m = u.protocol === 'https:' ? https : http;
		a = new m.Agent({ keepAlive: true, maxSockets: Infinity, maxFreeSockets: 4096 });
		agents.set(key, a);
	}
	return a;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** 10.a.b.c for virtual user number n (1-based), so each one is its own client. */
function xffFor(n) {
	const v = (n % 0xfffffe) + 1;
	return `10.${(v >> 16) & 255}.${(v >> 8) & 255}.${v & 255}`;
}

// What the reader's GET builds: encodeURIComponent, then WHATWG normalisation, whose
// only difference inside a query string is the apostrophe (graphql-backend.ts:146).
const enc = (s) => encodeURIComponent(s).replace(/'/g, '%27');
const QC = {};
function compiled(name) {
	let c = QC[name];
	if (!c) {
		if (typeof Q[name] !== 'string') die(`no operation text for ${name}`);
		c = QC[name] = { json: JSON.stringify(Q[name]), enc: enc(Q[name]) };
	}
	return c;
}

/**
 * One GraphQL request for a virtual user. Transport follows the reader
 * (graphql-backend.ts:292-322): an anonymous read is a GET, everything else a POST,
 * and a signed-in user sends `Authorization: Bearer <token>`.
 */
function gql(vu, name, label, kind, variables, o = {}) {
	const c = compiled(name);
	const token = o.anonymous ? null : vu.token;
	const req = {
		label,
		kind,
		xff: vu.xff,
		want: !!o.want,
		timeoutMs: o.timeoutMs,
		work: o.work ?? null,
	};
	if (!token && kind === 'read' && GLOBAL.anonMethod === 'get' && !o.post) {
		req.method = 'GET';
		req.path = `${GQL_PATH}?query=${c.enc}`;
		if (variables) req.path += `&variables=${enc(JSON.stringify(variables))}`;
	} else {
		req.method = 'POST';
		req.path = GQL_PATH;
		req.token = token;
		req.body = `{"query":${c.json}${variables ? `,"variables":${JSON.stringify(variables)}` : ''}}`;
	}
	return req;
}

const IMAGE_ACCEPT = 'image/avif,image/webp,image/apng,image/*,*/*;q=0.8';

/** First bytes of the known raster formats. Unknown is counted, not an error. */
function magicOf(head) {
	if (!head || head.length < 4) return 'short';
	if (head[0] === 0xff && head[1] === 0xd8) return 'jpeg';
	if (head[0] === 0x89 && head[1] === 0x50 && head[2] === 0x4e && head[3] === 0x47) return 'png';
	if (head.subarray(0, 4).toString('latin1') === 'GIF8') return 'gif';
	if (
		head.subarray(0, 4).toString('latin1') === 'RIFF' &&
		head.subarray(8, 12).toString('latin1') === 'WEBP'
	)
		return 'webp';
	if (head.subarray(4, 8).toString('latin1') === 'ftyp') return 'isobmff';
	return 'unknown';
}

/**
 * Send one request. Resolves (never rejects) with
 *   { ms, status, bytes, kind, messages, json, image }
 * `kind` is null for a good answer, else `http_<status>`, `graphql`, `timeout`,
 * `net_<code>`, `empty_body`, `not_image` or `refused_production`. The clock stops at
 * the last response byte, before gunzip and parsing. Image bodies are read to the end
 * and counted, but not kept.
 */
function send(req) {
	return new Promise((resolve) => {
		const u = req.url ?? null;
		const where = u ?? base;
		if (u && isProductionHost(u.hostname)) {
			resolve({ ms: 0, status: 0, bytes: 0, kind: 'refused_production', messages: [u.hostname] });
			return;
		}
		const t0 = process.hrtime.bigint();
		const elapsed = () => Number(process.hrtime.bigint() - t0) / 1e6;
		const headers = { 'x-forwarded-for': req.xff, connection: 'keep-alive' };
		if (!GLOBAL.noGzip) headers['accept-encoding'] = 'gzip';
		if (req.image) headers.accept = IMAGE_ACCEPT;
		if (req.token) headers.authorization = `Bearer ${req.token}`;
		if (req.body) {
			headers['content-type'] = 'application/json';
			headers['content-length'] = Buffer.byteLength(req.body);
		}
		let settled = false;
		let timer = null;
		const settle = (r) => {
			if (settled) return;
			settled = true;
			clearTimeout(timer);
			resolve(r);
		};
		const r = (where.protocol === 'https:' ? https : http).request(
			{
				protocol: where.protocol,
				hostname: where.hostname.replace(/^\[|\]$/g, ''),
				port: where.port || (where.protocol === 'https:' ? 443 : 80),
				path: u ? `${u.pathname}${u.search}` : req.path,
				method: req.method ?? 'GET',
				headers,
				agent: agentFor(where),
			},
			(res) => {
				const chunks = [];
				let n = 0;
				let head = null;
				res.on('data', (c) => {
					if (req.image) {
						if (!head) head = Buffer.from(c.subarray(0, 16));
						else if (head.length < 16)
							head = Buffer.concat([head, c.subarray(0, 16 - head.length)]);
					} else chunks.push(c);
					n += c.length;
				});
				res.on('error', (e) =>
					settle({
						ms: elapsed(),
						status: 0,
						bytes: n,
						kind: `net_${e.code || 'ERR'}`,
						messages: [],
					}),
				);
				res.on('end', () => {
					const ms = elapsed();
					clearTimeout(timer);
					if (req.image) {
						settle(inspectImage(req, res, ms, n, head));
						return;
					}
					const raw = chunks.length === 1 ? chunks[0] : Buffer.concat(chunks, n);
					const done = (body) => settle(inspect(req, res.statusCode, ms, n, body));
					if (res.headers['content-encoding'] === 'gzip') {
						// Off the event loop (libuv pool), so inflating does not delay other sends.
						zlib.gunzip(raw, (err, body) => done(err ? Buffer.alloc(0) : body));
					} else done(raw);
				});
			},
		);
		timer = setTimeout(
			() => {
				const e = new Error('timeout');
				e.code = 'TIMEOUT';
				r.destroy(e);
			},
			req.timeoutMs ?? args.timeout * 1000,
		);
		r.on('error', (e) => {
			const kind = e.code === 'TIMEOUT' ? 'timeout' : `net_${e.code || 'ERR'}`;
			settle({ ms: elapsed(), status: 0, bytes: 0, kind, messages: [] });
		});
		if (req.body) r.write(req.body);
		r.end();
	});
}

/**
 * Classify a GraphQL answer. Good means 2xx AND a JSON object with `data` or `errors`
 * (the GraphQL envelope) AND no `errors`. Every body is parsed: an empty, HTML or
 * truncated 200, or a 3xx, is an error (`bad_envelope`, `http_3xx`), never a success.
 */
export function inspect(req, status, ms, bytes, body) {
	let json;
	let parsed = false;
	try {
		json = JSON.parse(body.toString('utf8'));
		parsed = true;
	} catch {
		json = undefined;
	}
	const envelope =
		parsed &&
		json !== null &&
		typeof json === 'object' &&
		!Array.isArray(json) &&
		('data' in json || 'errors' in json);
	const listed = Array.isArray(json?.errors)
		? json.errors.map((e) => String(e?.message ?? '(no message)'))
		: [];
	const snippet = () =>
		body.subarray(0, 160).toString('utf8').replace(/\s+/g, ' ').trim() || '(empty body)';
	let kind = null;
	let messages = [];
	if (status < 200 || status >= 300) {
		kind = `http_${status}`;
		messages = listed.length ? listed : [snippet()];
	} else if (!envelope) {
		kind = 'bad_envelope';
		messages = [`${parsed ? 'JSON without data/errors' : 'not JSON'}: ${snippet()}`];
	} else if (listed.length || (json.errors !== undefined && json.errors !== null)) {
		kind = 'graphql';
		messages = listed.length ? listed : ['(errors that are not a list)'];
	}
	return { ms, status, bytes, kind, messages, json: req.want || kind ? json : undefined };
}

function inspectImage(req, res, ms, bytes, head) {
	const status = res.statusCode;
	const h = res.headers;
	const contentType = String(h['content-type'] ?? '')
		.split(';')[0]
		.trim()
		.toLowerCase();
	let kind = null;
	let messages = [];
	if (status >= 300 && status !== 304) {
		kind = `http_${status}`;
		messages = [`${req.route ?? 'image'} answered ${status}`];
	} else if (status !== 304 && bytes === 0) {
		kind = 'empty_body';
		messages = [`${req.route ?? 'image'}: empty body`];
	} else if (status !== 304 && !contentType.startsWith('image/')) {
		kind = 'not_image';
		messages = [`${req.route ?? 'image'}: content-type ${contentType || '(none)'}`];
	}
	return {
		ms,
		status,
		bytes,
		kind,
		messages,
		image: {
			contentType: contentType || '(none)',
			magic: magicOf(head),
			cfCacheStatus: h['cf-cache-status'] ?? null,
			xCache: h['x-cache'] ?? null,
			age: h.age ?? null,
			cacheControl: h['cache-control'] ?? null,
			etag: h.etag ? true : false,
		},
	};
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

const bucket = () => ({ n: 0, ok: 0, errors: 0, bytes: 0, lat: [] });
const tally = (obj, key) => {
	const k = String(key);
	obj[k] = (obj[k] || 0) + 1;
};

class Stats {
	constructor() {
		this.total = 0;
		this.ok = 0;
		this.bytes = 0;
		this.read = bucket();
		this.write = bucket();
		this.image = bucket();
		this.ops = new Map();
		this.errors = {};
		this.messages = new Map();
		this.works = new Map();
		this.img = {
			statuses: {},
			contentTypes: {},
			magic: {},
			cfCacheStatus: {},
			xCache: {},
			cacheControl: {},
			withAge: 0,
			withEtag: 0,
			byRoute: {},
		};
	}
	add(req, r) {
		this.total++;
		this.bytes += r.bytes;
		let op = this.ops.get(req.label);
		if (!op) this.ops.set(req.label, (op = { kind: req.kind, ...bucket() }));
		const cls = this[req.kind];
		cls.n++;
		op.n++;
		cls.bytes += r.bytes;
		op.bytes += r.bytes;
		if (req.work) this.works.set(req.work, (this.works.get(req.work) || 0) + 1);
		if (req.kind === 'image') this.addImage(req, r);
		if (r.kind === null) {
			this.ok++;
			cls.ok++;
			op.ok++;
			cls.lat.push(r.ms);
			op.lat.push(r.ms);
			return;
		}
		op.errors++;
		cls.errors++;
		this.errors[r.kind] = (this.errors[r.kind] || 0) + 1;
		if (r.kind === 'timeout') {
			cls.lat.push(r.ms);
			op.lat.push(r.ms);
		}
		for (const m of r.messages.slice(0, 2)) {
			// "try again in 37s" and "… in 36s" are one message.
			const key = `${req.label}: ${m.replace(/\b\d+s\b/g, 'Ns').slice(0, 200)}`;
			this.messages.set(key, (this.messages.get(key) || 0) + 1);
		}
	}
	addImage(req, r) {
		const s = this.img;
		tally(s.statuses, r.status);
		const route = (s.byRoute[req.route ?? 'other'] ??= { requests: 0, ok: 0, bytes: 0 });
		route.requests++;
		route.bytes += r.bytes;
		if (r.kind === null) route.ok++;
		const i = r.image;
		if (!i) return;
		tally(s.contentTypes, i.contentType);
		tally(s.magic, i.magic);
		if (i.cfCacheStatus) tally(s.cfCacheStatus, i.cfCacheStatus);
		if (i.xCache) tally(s.xCache, i.xCache);
		if (i.cacheControl && Object.keys(s.cacheControl).length < 12)
			tally(s.cacheControl, i.cacheControl);
		else if (i.cacheControl && s.cacheControl[i.cacheControl])
			tally(s.cacheControl, i.cacheControl);
		if (i.age !== null) s.withAge++;
		if (i.etag) s.withEtag++;
	}
}

function dist(lat) {
	const a = Float64Array.from(lat).sort();
	const q = (p) => (a.length ? a[Math.min(a.length - 1, Math.floor((p / 100) * a.length))] : null);
	const round = (v) => (v === null ? null : +v.toFixed(2));
	return {
		samples: a.length,
		p50: round(q(50)),
		p95: round(q(95)),
		p99: round(q(99)),
		max: round(a.length ? a[a.length - 1] : null),
		mean: round(a.length ? a.reduce((s, x) => s + x, 0) / a.length : null),
	};
}

function classSummary(b) {
	return {
		requests: b.n,
		ok: b.ok,
		errors: b.errors,
		errorShare: b.n ? +(b.errors / b.n).toFixed(5) : 0,
		bytes: b.bytes,
		...dist(b.lat),
	};
}

/** How the requests that name a work spread over works. */
function workDistribution(map) {
	const rows = [...map].sort((a, b) => b[1] - a[1] || (a[0] < b[0] ? -1 : 1));
	const total = rows.reduce((s, [, n]) => s + n, 0);
	const share = (k) =>
		total ? +(rows.slice(0, k).reduce((s, [, n]) => s + n, 0) / total).toFixed(4) : 0;
	return {
		requests: total,
		distinctWorks: rows.length,
		top1Share: share(1),
		top20Share: share(20),
		top: rows.slice(0, 20).map(([id, n]) => ({ id, requests: n })),
	};
}

function summarize(stats, wallSec, lagMs) {
	const errors = stats.total - stats.ok;
	const per = (n) => +(n / wallSec).toFixed(2);
	const ops = {};
	for (const [label, o] of [...stats.ops].sort((a, b) => b[1].n - a[1].n || (a[0] < b[0] ? -1 : 1)))
		ops[label] = {
			kind: o.kind,
			requests: o.n,
			perSec: per(o.n),
			errors: o.errors,
			bytes: o.bytes,
			...dist(o.lat),
		};
	return {
		seconds: +wallSec.toFixed(2),
		requests: stats.total,
		rps: per(stats.total),
		okPerSec: per(stats.ok),
		readsPerSec: per(stats.read.n),
		writesPerSec: per(stats.write.n),
		imagesPerSec: per(stats.image.n),
		bytesPerSec: Math.round(stats.bytes / wallSec),
		responseBytes: stats.bytes,
		errors,
		errorShare: stats.total ? +(errors / stats.total).toFixed(5) : 0,
		errorsByKind: stats.errors,
		errorMessages: [...stats.messages]
			.sort((a, b) => b[1] - a[1])
			.slice(0, 12)
			.map(([message, count]) => ({ message, count })),
		read: classSummary(stats.read),
		write: classSummary(stats.write),
		image: { ...classSummary(stats.image), ...(stats.image.n ? stats.img : {}) },
		ops,
		works: workDistribution(stats.works),
		generatorLagP99Ms: lagMs,
	};
}

// ---------------------------------------------------------------------------
// Server metrics: generic Prometheus text
// ---------------------------------------------------------------------------

function parseValue(v) {
	if (v === undefined) return undefined;
	if (v === 'NaN') return NaN;
	if (v === '+Inf' || v === 'Inf') return Infinity;
	if (v === '-Inf') return -Infinity;
	const n = Number(v);
	return Number.isNaN(n) ? undefined : n;
}

function parseSampleLine(line) {
	const m = /^[a-zA-Z_:][a-zA-Z0-9_:]*/.exec(line);
	if (!m) return null;
	const name = m[0];
	let i = name.length;
	const labels = {};
	if (line[i] === '{') {
		i++;
		for (;;) {
			while (line[i] === ' ' || line[i] === ',') i++;
			if (line[i] === '}') {
				i++;
				break;
			}
			const lm = /^[a-zA-Z_][a-zA-Z0-9_]*/.exec(line.slice(i));
			if (!lm) return null;
			const key = lm[0];
			i += key.length;
			while (line[i] === ' ') i++;
			if (line[i] !== '=') return null;
			i++;
			while (line[i] === ' ') i++;
			if (line[i] !== '"') return null;
			i++;
			let val = '';
			while (i < line.length && line[i] !== '"') {
				if (line[i] === '\\' && i + 1 < line.length) {
					const c = line[i + 1];
					val += c === 'n' ? '\n' : c;
					i += 2;
				} else val += line[i++];
			}
			if (line[i] !== '"') return null;
			i++;
			labels[key] = val;
		}
	}
	const value = parseValue(line.slice(i).trim().split(/\s+/)[0]);
	if (value === undefined) return null;
	return { name, labels, value };
}

/** `name{a="x",b="y"}` with the labels sorted: one stable key per series. */
export function seriesKey(name, labels) {
	const keys = Object.keys(labels).sort();
	if (!keys.length) return name;
	const esc = (v) => v.replace(/\\/g, '\\\\').replace(/"/g, '\\"').replace(/\n/g, '\\n');
	return `${name}{${keys.map((k) => `${k}="${esc(labels[k])}"`).join(',')}}`;
}

const COUNTER_SUFFIXES = ['_total', '_sum', '_count', '_bucket'];

function sampleType(name, types) {
	const own = types.get(name);
	if (own === 'counter') return 'counter';
	if (own === 'gauge') return 'gauge';
	if (own === 'summary' || own === 'histogram') return 'gauge'; // a summary's quantile lines
	for (const suffix of [...COUNTER_SUFFIXES, '_created']) {
		if (!name.endsWith(suffix)) continue;
		const family = types.get(name.slice(0, -suffix.length));
		if (!family || family === 'untyped') continue;
		if (suffix === '_created') return 'gauge';
		if (family === 'counter' || family === 'histogram' || family === 'summary') return 'counter';
		return 'gauge';
	}
	return COUNTER_SUFFIXES.some((s) => name.endsWith(s)) ? 'counter' : 'gauge';
}

/**
 * Parse Prometheus text exposition. No metric name is assumed. `# TYPE` decides
 * counter or gauge; without it, `_total`, `_sum`, `_count` and `_bucket` are counters
 * and the rest gauges. (A `# TYPE x gauge` named `..._total` stays a gauge.)
 */
export function parsePrometheus(text) {
	const types = new Map();
	const samples = [];
	for (const raw of String(text).split('\n')) {
		const line = raw.trim();
		if (!line) continue;
		if (line[0] === '#') {
			const m = /^#\s*TYPE\s+(\S+)\s+(\S+)/.exec(line);
			if (m) types.set(m[1], m[2].toLowerCase());
			continue;
		}
		const s = parseSampleLine(line);
		if (s) samples.push(s);
	}
	const series = new Map();
	for (const s of samples)
		series.set(seriesKey(s.name, s.labels), {
			name: s.name,
			labels: s.labels,
			value: s.value,
			type: sampleType(s.name, types),
		});
	return { series, types };
}

const num = (v) =>
	Number.isFinite(v) ? +v.toFixed(6) : v === Infinity ? '+Inf' : v === -Infinity ? '-Inf' : null;

/**
 * One measured window from its scrapes (ordered by time): counter deltas between the
 * first and the last good scrape, gauge first/last/min/max over every good one. A
 * counter that went DOWN was reset (a restart): its delta is its last value, and it is
 * listed in `resets`.
 */
export function metricsWindow(scrapes) {
	const good = scrapes.filter((s) => s.ok);
	const out = {
		scrapes: scrapes.length,
		good: good.length,
		failures: scrapes
			.filter((s) => !s.ok)
			.map((s) => ({ phase: s.phase, at: s.at, error: s.error })),
		from: good[0] ? new Date(good[0].at).toISOString() : null,
		to: good.length ? new Date(good[good.length - 1].at).toISOString() : null,
		counters: null,
		gauges: null,
		resets: [],
		appeared: [],
		derived: null,
	};
	if (!good.length) return out;
	const meta = new Map();
	if (good.length >= 2) {
		// Reset-aware increase over CONSECUTIVE good scrapes (as Prometheus `increase`):
		// a value lower than the one before means the counter restarted from zero, so the
		// new value itself is the increase for that interval. 100 -> 150 -> 20 -> 120 is
		// 50 + 20 + 100 = 170, and the series is listed in `resets`.
		out.counters = {};
		const prev = new Map();
		const resets = new Set();
		const appeared = new Set();
		for (const [i, g] of good.entries()) {
			for (const [key, s] of g.parsed.series) {
				if (s.type !== 'counter') continue;
				meta.set(key, s);
				const before = prev.get(key);
				prev.set(key, s.value);
				if (before === undefined) {
					if (i === 0) {
						out.counters[key] = 0;
						continue;
					}
					appeared.add(key); // new mid-window: counted from zero
					out.counters[key] = (out.counters[key] ?? 0) + s.value;
				} else if (s.value < before) {
					resets.add(key);
					out.counters[key] += s.value;
				} else out.counters[key] += s.value - before;
			}
		}
		for (const key of Object.keys(out.counters)) out.counters[key] = num(out.counters[key]);
		out.resets = [...resets];
		out.appeared = [...appeared];
		out.reset = resets.size > 0;
	}
	out.gauges = {};
	for (const g of good) {
		for (const [key, s] of g.parsed.series) {
			if (s.type !== 'gauge') continue;
			const v = s.value;
			const e = out.gauges[key];
			if (!e)
				out.gauges[key] = { first: num(v), last: num(v), min: num(v), max: num(v), samples: 1 };
			else {
				e.last = num(v);
				if (Number.isFinite(v)) {
					if (e.min === null || v < e.min) e.min = num(v);
					if (e.max === null || v > e.max) e.max = num(v);
				}
				e.samples++;
			}
		}
	}
	if (out.counters) out.derived = deriveMetrics(out.counters, meta);
	return out;
}

const HIT = /^(hit|hits)$/i;
const MISS_WORDS = ['miss', 'misses'];

/**
 * What the counters say, when matching names exist:
 *   hitRatios  hits / (hits + misses) per cache: `..._hit[s]_...` beside
 *              `..._miss[es]_...` with the same labels, or a label whose value is
 *              hit/miss. `unit` is bytes when the name says bytes.
 *   bytes      every counter with `bytes` in its name, largest first (media routes).
 *   refused    counters named refused / rejected / shed / denied, or with such a label
 *              value (`outcome="refused"`), that moved, with the lane (or the whole
 *              label set) that refused.
 */
export function deriveMetrics(counters, meta) {
	const hitRatios = [];
	const seen = new Set();
	for (const [key, s] of meta) {
		const tokens = s.name.split('_');
		const at = tokens.findIndex((t) => HIT.test(t));
		if (at >= 0) {
			for (const miss of MISS_WORDS) {
				const missName = [...tokens.slice(0, at), miss, ...tokens.slice(at + 1)].join('_');
				const missKey = seriesKey(missName, s.labels);
				if (!(missKey in counters)) continue;
				const id = seriesKey(
					[...tokens.slice(0, at), '*', ...tokens.slice(at + 1)].join('_'),
					s.labels,
				);
				if (seen.has(id)) continue;
				seen.add(id);
				hitRatios.push(ratio(id, s.name, counters[key], counters[missKey]));
			}
		}
		for (const [label, value] of Object.entries(s.labels)) {
			if (!HIT.test(value)) continue;
			for (const miss of MISS_WORDS) {
				const missKey = seriesKey(s.name, { ...s.labels, [label]: miss });
				if (!(missKey in counters)) continue;
				const id = seriesKey(s.name, { ...s.labels, [label]: '*' });
				if (seen.has(id)) continue;
				seen.add(id);
				hitRatios.push(ratio(id, s.name, counters[key], counters[missKey]));
			}
		}
	}
	const bytes = Object.entries(counters)
		.filter(([key]) => /bytes/i.test(meta.get(key)?.name ?? ''))
		.map(([series, delta]) => ({ series, delta }))
		.sort((a, b) => (b.delta ?? 0) - (a.delta ?? 0));
	const refused = [];
	for (const [key, delta] of Object.entries(counters)) {
		const s = meta.get(key);
		const refusal = /refus|reject|shed|denied/i;
		if (!s || !(delta > 0)) continue;
		if (!refusal.test(s.name) && !Object.values(s.labels).some((v) => refusal.test(v))) continue;
		refused.push({ series: key, lane: s.labels.lane ?? (seriesKey('', s.labels) || null), delta });
	}
	refused.sort((a, b) => b.delta - a.delta);
	return {
		hitRatios: hitRatios.sort((a, b) => (a.key < b.key ? -1 : 1)),
		bytes,
		refused,
		refusingLanes: [...new Set(refused.map((r) => r.lane))],
	};
}

function ratio(key, name, hits, misses) {
	const total = (hits ?? 0) + (misses ?? 0);
	return {
		key,
		unit: /bytes/i.test(name) ? 'bytes' : 'requests',
		hits,
		misses,
		ratio: total > 0 ? +(hits / total).toFixed(5) : null,
	};
}

function fetchText(url, timeoutMs = 5000) {
	return new Promise((resolve, reject) => {
		const u = new URL(url);
		if (isProductionHost(u.hostname)) {
			reject(new Error('production host'));
			return;
		}
		const r = (u.protocol === 'https:' ? https : http).get(
			{
				hostname: u.hostname.replace(/^\[|\]$/g, ''),
				port: u.port || undefined,
				path: `${u.pathname}${u.search}`,
				protocol: u.protocol,
				timeout: timeoutMs,
			},
			(res) => {
				const chunks = [];
				res.on('data', (c) => chunks.push(c));
				res.on('error', reject);
				res.on('end', () => {
					const body = Buffer.concat(chunks).toString('utf8');
					if (res.statusCode !== 200) reject(new Error(`HTTP ${res.statusCode}`));
					else resolve(body);
				});
			},
		);
		r.on('timeout', () => r.destroy(new Error('timeout')));
		r.on('error', reject);
	});
}

async function scrape(phase) {
	const at = Date.now();
	try {
		const text = await fetchText(GLOBAL.metricsUrl);
		const parsed = parsePrometheus(text);
		return { phase, at, ok: true, parsed, seriesCount: parsed.series.size };
	} catch (e) {
		return { phase, at, ok: false, error: e.message };
	}
}

/** Scrapes for one measured window: `begin` when it opens, `end` when it closes. */
function metricsRecorder() {
	if (!GLOBAL.metricsUrl) return null;
	const scrapes = [];
	const pending = new Set();
	let timer = null;
	let closed = false;
	const take = (phase) => {
		const p = scrape(phase).then((s) => scrapes.push(s));
		pending.add(p);
		p.finally(() => pending.delete(p));
		return p;
	};
	return {
		begin() {
			if (closed) return;
			take('before');
			timer = setInterval(() => take('sample'), Math.max(0.1, GLOBAL.metricsInterval) * 1000);
		},
		async end() {
			closed = true;
			clearInterval(timer);
			// The closing scrape starts at the boundary itself, not after slower ones finish.
			take('after');
			await Promise.allSettled([...pending]);
			scrapes.sort((a, b) => a.at - b.at || (a.phase === 'before' ? -1 : 1));
			return { url: redactUrl(GLOBAL.metricsUrl), ...metricsWindow(scrapes) };
		},
	};
}

// ---------------------------------------------------------------------------
// Host measurements: one shell command per load step
// ---------------------------------------------------------------------------

/** The last JSON object in a command's stdout. */
function lastJson(stdout) {
	const text = stdout.trim();
	if (!text) return null;
	try {
		return JSON.parse(text);
	} catch {
		/* fall through */
	}
	const lines = text.split('\n');
	for (let i = lines.length - 1; i >= 0; i--) {
		if (!lines[i].trim().startsWith('{')) continue;
		try {
			return JSON.parse(lines.slice(i).join('\n'));
		} catch {
			/* try an earlier line */
		}
	}
	return null;
}

/**
 * Start the host command for one load step. `{label}` and `{seconds}` are filled in.
 * The command should print a line `READY` (stderr or stdout) when its baseline is taken
 * and its window starts (host/measure.sh does); the measured window opens then, so the
 * host window and the load window start together. `ready` resolves at READY, or after
 * `--host-ready-timeout` seconds with `ready: false`. Only redacted command text is
 * ever recorded.
 */
function hostCommand(label, seconds) {
	const safeLabel = label.replace(/[^A-Za-z0-9_.-]/g, '_');
	const command = GLOBAL.hostCmd
		.replaceAll('{label}', safeLabel)
		.replaceAll('{seconds}', String(Math.ceil(seconds)));
	const shown = redactCommand(command);
	const started = Date.now();
	let readyResolve;
	const ready = new Promise((r) => (readyResolve = r));
	let readyInfo = null;
	const markReady = (info) => {
		if (readyInfo) return;
		readyInfo = info;
		readyResolve(info);
	};
	const readyTimer = setTimeout(
		() => markReady({ ready: false, ms: Date.now() - started, timedOut: true }),
		GLOBAL.hostReadyTimeout * 1000,
	);
	const done = new Promise((resolve) => {
		let child;
		try {
			child = spawn('/bin/sh', ['-c', command], { stdio: ['ignore', 'pipe', 'pipe'] });
		} catch (e) {
			clearTimeout(readyTimer);
			markReady({ ready: false, ms: 0, error: e.message });
			resolve({ command: shown, ok: false, error: redactCommand(e.message), ready: readyInfo });
			return;
		}
		let out = '';
		let err = '';
		const seeReady = (text) => {
			if (!readyInfo && /(^|\n)READY\b/.test(text)) {
				clearTimeout(readyTimer);
				markReady({ ready: true, ms: Date.now() - started });
			}
		};
		child.stdout.on('data', (c) => {
			if (out.length < 8 << 20) out += c;
			seeReady(out);
		});
		child.stderr.on('data', (c) => {
			err = (err + c).slice(-4000);
			seeReady(err);
		});
		const kill = setTimeout(
			() => child.kill('SIGKILL'),
			(Math.ceil(seconds) + GLOBAL.hostReadyTimeout + GLOBAL.hostTimeout) * 1000,
		);
		const finish = (r) => {
			clearTimeout(kill);
			clearTimeout(readyTimer);
			markReady({ ready: false, ms: Date.now() - started, exitedFirst: true });
			resolve({ command: shown, ...r, ready: readyInfo, ms: Date.now() - started });
		};
		child.on('error', (e) => finish({ ok: false, error: redactCommand(e.message) }));
		child.on('close', (code, signal) => {
			const json = lastJson(out.replace(/(^|\n)READY\b[^\n]*/g, '$1'));
			const ok = code === 0 && json !== null;
			finish({
				ok,
				exitCode: code,
				signal,
				...(json !== null
					? { json }
					: { error: code === 0 ? 'no JSON on stdout' : `exit ${code ?? signal}` }),
				...(ok ? {} : { stderrTail: redactCommand(err.slice(-1000)) }),
			});
		});
	});
	return { ready, done };
}

// ---------------------------------------------------------------------------
// Step control
// ---------------------------------------------------------------------------

let interrupted = false;

// --stop-file: a controlling process creates the file to end the load early.
let stopSeen = false;
let stopCheckedAt = 0;
function stopRequested() {
	if (interrupted) return true;
	if (!GLOBAL?.stopFile || stopSeen) return stopSeen;
	const now = Date.now();
	if (now - stopCheckedAt >= 50) {
		stopCheckedAt = now;
		stopSeen = existsSync(GLOBAL.stopFile);
	}
	return stopSeen;
}

const NOT_SENT = Object.freeze({ ms: 0, status: 0, bytes: 0, kind: 'not_sent', messages: [] });

/**
 * One ramp step. Load runs from the start; requests are thrown away until the measured
 * window is OPENED (after the warm-up, and after the host command said READY), and the
 * window closes `seconds` later. A request belongs to the window it was STARTED in. No
 * new request is sent once the window has closed; requests still in flight then finish
 * and are counted (`inflightAtBoundary`, `completedAfterBoundary`).
 */
class Run {
	constructor() {
		this.from = Infinity;
		this.until = Infinity;
		this.opened = false;
		this.boundary = null;
		this.inflight = 0;
		this.lateCompletions = 0;
		this.notSent = 0;
		this.stats = new Stats();
		this.naps = new Set();
		this.waiters = [];
		this.watch = setInterval(() => {
			if (stopRequested()) this.wake();
		}, 200);
	}
	open(at, seconds) {
		this.from = at;
		this.until = at + seconds * 1000;
		this.opened = true;
		for (const w of this.waiters.splice(0)) w();
	}
	/** Resolves when the measured window opens. */
	whenOpen() {
		return this.opened ? Promise.resolve() : new Promise((r) => this.waiters.push(r));
	}
	over() {
		return Date.now() >= this.until || stopRequested();
	}
	/** Sleep, cut short at the end of the step. False when the step is over. */
	nap(ms) {
		if (this.over()) return Promise.resolve(false);
		const left = this.until - Date.now();
		return new Promise((resolve) => {
			const entry = { resolve, timer: null };
			entry.timer = setTimeout(
				() => {
					this.naps.delete(entry);
					resolve(!this.over());
				},
				Math.max(0, Math.min(ms, left)),
			);
			this.naps.add(entry);
		});
	}
	wake() {
		for (const e of this.naps) {
			clearTimeout(e.timer);
			e.resolve(false);
		}
		this.naps.clear();
	}
	async perform(req) {
		if (Date.now() >= this.until) {
			this.notSent++;
			return NOT_SENT;
		}
		const started = Date.now();
		this.inflight++;
		const r = await send(req);
		this.inflight--;
		// In flight when the window closed: it finished after the boundary.
		if (this.boundary !== null && started <= this.boundary) this.lateCompletions++;
		if (started >= this.from && started < this.until) this.stats.add(req, r);
		// A refused connection answers in microseconds; do not spin on it.
		if (r.status === 0 && r.ms < 5 && r.kind !== 'refused_production') await sleep(50);
		return r;
	}
	close() {
		clearInterval(this.watch);
		this.wake();
	}
}

// ---------------------------------------------------------------------------
// Setup: accounts
// ---------------------------------------------------------------------------

const SETUP_TIMEOUT_MS = 60_000;
let nextSetupSlot = 0;
async function pace() {
	const now = Date.now();
	const at = Math.max(now, nextSetupSlot);
	nextSetupSlot = at + 1000 / Math.max(0.1, GLOBAL.setupRate);
	if (at > now) await sleep(at - now);
}

/** A setup request: paced, POST, parsed, never part of a measurement. */
async function call(vu, name, variables, kind = 'read') {
	await pace();
	return send(
		gql(vu, name, name, kind, variables, { want: true, post: true, timeoutMs: SETUP_TIMEOUT_MS }),
	);
}

async function pmap(items, concurrency, fn) {
	let next = 0;
	const worker = async () => {
		while (next < items.length) {
			const i = next++;
			await fn(items[i], i);
		}
	};
	await Promise.all(
		Array.from({ length: Math.max(1, Math.min(concurrency, items.length)) }, worker),
	);
}

function loadState() {
	if (!existsSync(GLOBAL.state)) return { version: 1, password: null, targets: {} };
	try {
		const s = JSON.parse(readFileSync(GLOBAL.state, 'utf8'));
		if (!s.targets) s.targets = {};
		return s;
	} catch (e) {
		die(`--state ${GLOBAL.state} is not readable JSON (${e.message}); move it away or fix it`);
	}
}

function saveState(state) {
	const tmp = `${GLOBAL.state}.tmp`;
	try {
		writeFileSync(tmp, JSON.stringify(state), { mode: 0o600 });
		renameSync(tmp, GLOBAL.state);
	} catch (e) {
		die(
			`cannot write --state ${GLOBAL.state}: ${e.message}. It holds the generated password; ` +
				'point --state at a writable path that survives the run.',
		);
	}
}

const AUTH_HELP = {
	closed:
		'Sign-up is closed on this server. Open it with KOMIKA_REGISTRATION_OPEN=true in the ' +
		'server environment (env wins), or the admin setting registration_open ' +
		'(setRegistrationOpen); a self-hosted install starts closed.',
	limited:
		'The auth rate limiter refused (keys register:<client ip>, login:<client ip>; default ' +
		'10 per 300 s). Every account here sends its own X-Forwarded-For, so this means the ' +
		'server does not trust it: set TRUSTED_PROXY_CIDRS to cover the address this generator ' +
		'connects from (127.0.0.1/32 with --network host), or raise AUTH_RATE_LIMIT_MAX / ' +
		'lower AUTH_RATE_LIMIT_WINDOW_SECS. Accounts made so far are kept in --state.',
	mismatch:
		'the account exists but the password in --state does not open it (state file lost or ' +
		'from another run?). Use the original --state file, pass --password, or pick a new --prefix.',
};

async function setupAccounts(accountsWanted, largestStep) {
	const state = loadState();
	if (GLOBAL.password) state.password = GLOBAL.password;
	if (!state.password) state.password = randomBytes(18).toString('base64url');
	const saved = (state.targets[GLOBAL.target] ??= {});
	saveState(state);

	const accounts = Array.from({ length: accountsWanted }, (_, i) => {
		const username = `${GLOBAL.prefix}${String(i + 1).padStart(5, '0')}`;
		return { i, username, xff: xffFor(i + 1), token: saved[username]?.token ?? null, userId: null };
	});
	const counts = { reused: 0, registered: 0, loggedIn: 0 };
	let stop = null;

	// 1. Tokens from an earlier run: one cheap `session` read each, no auth limiter.
	const withToken = accounts.filter((a) => a.token);
	await pmap(withToken, 8, async (a) => {
		const r = await call(a, 'SESSION');
		const user = r.json?.data?.session?.user;
		if (r.kind === null && user?.username === a.username) {
			a.userId = user.id;
			counts.reused++;
		} else if (r.kind === null || r.kind === 'graphql') {
			a.token = null; // expired, or a different database
		} else if (!stop) {
			stop = `cannot check sessions at ${GLOBAL.target}: ${r.kind} ${r.messages[0] ?? ''}`;
		}
	});
	if (stop) die(stop);

	// 2. Register, or log in when the name is taken. At most 3 at once: the server
	//    hashes passwords under a 4-permit semaphore and refuses rather than queues.
	const fresh = accounts.filter((a) => !a.token);
	const attempt = async (a, name, variables, field) => {
		for (let tries = 0; ; tries++) {
			const r = await call({ ...a, token: null }, name, variables, 'write');
			const msg = r.messages[0] ?? '';
			const busy = r.status === 503 || /busy/i.test(msg);
			if (busy && tries < 20) {
				await sleep(250 + Math.random() * 250);
				continue;
			}
			const session = r.json?.data?.[field];
			if (r.kind === null && session?.token) {
				a.token = session.token;
				a.userId = session.user?.id ?? null;
				return { ok: true };
			}
			return { ok: false, msg: msg || r.kind || 'no session in the answer', kind: r.kind };
		}
	};
	await pmap(fresh, 3, async (a) => {
		if (stop) return;
		const login = () =>
			attempt(a, 'LOGIN', { username: a.username, password: state.password }, 'login');
		let r = await attempt(
			a,
			'REGISTER',
			{
				input: {
					username: a.username,
					email: `${a.username}@capacity.invalid`,
					password: state.password,
				},
			},
			'register',
		);
		let how = 'registered';
		if (!r.ok && /already taken/i.test(r.msg)) {
			r = await login();
			how = 'loggedIn';
		} else if (!r.ok && /sign-up is closed/i.test(r.msg)) {
			// The account may exist from a run made while sign-up was open.
			const l = await login();
			if (l.ok) {
				r = l;
				how = 'loggedIn';
			} else if (/too many/i.test(l.msg)) r = l;
			else {
				stop ??= `register ${a.username}: "${r.msg}"\n       ${AUTH_HELP.closed}`;
				return;
			}
		}
		if (r.ok) {
			counts[how]++;
			saved[a.username] = { token: a.token, userId: a.userId };
			return;
		}
		if (/too many/i.test(r.msg)) stop ??= `${a.username}: "${r.msg}"\n       ${AUTH_HELP.limited}`;
		else if (/invalid username or password/i.test(r.msg))
			stop ??= `login ${a.username}: "${r.msg}"\n       ${AUTH_HELP.mismatch}`;
		else stop ??= `${a.username}: "${r.msg}"`;
	});
	saveState(state);

	const ready = accounts.filter((a) => a.token);
	console.log(
		`accounts  ${ready.length}/${accountsWanted} ready ` +
			`(${counts.reused} from --state, ${counts.registered} registered, ${counts.loggedIn} logged in)`,
	);
	if (stop) {
		console.error(`\nsetup stopped: ${stop}`);
		if (!ready.length || !GLOBAL.shareAccounts) {
			if (ready.length)
				console.error('       (--share-accounts runs with the accounts that are ready)');
			process.exit(2);
		}
	}
	if (ready.length < largestStep) {
		const msg = `${ready.length} accounts for up to ${largestStep} users`;
		if (!GLOBAL.shareAccounts)
			die(
				`${msg}: raise --accounts, or pass --share-accounts (users then queue on the per-user write lock).`,
				2,
			);
		console.log(`          ! ${msg}: accounts are shared, writes of one account run one at a time`);
	}
	return { accounts: ready, counts, problem: stop };
}

// ---------------------------------------------------------------------------
// Setup: real ids
// ---------------------------------------------------------------------------

// The variables the reader really sends (graphql-backend.ts:431-453 with
// browse-filters.ts toLegacyFilters/toFilterInput for the default Browse state).
const VARS = {
	browse: (page) => ({
		query: '',
		page,
		sort: 'NEWEST',
		filter: { sort: 'LATEST_UPDATE', direction: 'DESC' },
	}),
	latestAdded: () => ({ query: '', page: 1, sort: 'LATEST_ADDED' }),
	search: (query) => ({ query, page: 1, filter: { sort: 'RELEVANCE' } }),
	series: (workId) => ({ workId, withAggregated: true }),
};

const chapterNumber = (c) => {
	const n = Number(c.number);
	return Number.isFinite(n) ? n : -Infinity;
};

async function buildPool() {
	const rng = makeRng(GLOBAL.seed, 'pool');
	let setupClient = 0;
	// A fresh client address per setup call, so warming the series caches is not held
	// to one client's upstream budget (60 cold fetches a minute).
	const anon = () => ({ token: null, xff: xffFor(0xf00000 + setupClient++) });
	const works = new Map();
	const problems = [];
	const dataset = {
		updatesFeedTotal: null,
		latestAddedTotal: null,
		browseTotal: null,
		discoveryRows: null,
	};
	let other = 0;
	const take = (items, fromFeed = false) => {
		for (const it of items ?? []) {
			const id = it?.workId || it?.id;
			if (typeof id !== 'string') continue;
			// Only canonical works have a series page bundle.
			if (!id.startsWith('w_')) other++;
			else if (!works.has(id))
				works.set(id, { id, title: String(it.title ?? ''), fromFeed, cover: it.coverUrl ?? null });
			else if (fromFeed) works.get(id).fromFeed = true;
		}
	};
	const fetchList = async (name, variables, pick) => {
		const r = await call(anon(), name, variables);
		if (r.kind !== null) {
			problems.push(`${name}: ${r.kind} ${r.messages[0] ?? ''}`.trim());
			return null;
		}
		return pick(r.json?.data ?? {});
	};

	const rows = await fetchList('DISCOVERY', undefined, (d) => d.discovery ?? []);
	dataset.discoveryRows = rows ? rows.length : null;
	take((rows ?? []).flatMap((f) => f.items ?? []));
	// The updates feed lists works that HAVE chapters, which the writes need; browse
	// also lists the many works without any. Take up to half the pool from the feed.
	let feedWorks = 0;
	for (let page = 1; feedWorks < GLOBAL.pool / 2 && page <= 12; page++) {
		const res = await fetchList('UPDATES_FEED', { page }, (d) => d.updatesFeed);
		if (page === 1) dataset.updatesFeedTotal = res?.total ?? null;
		const before = works.size;
		take(res?.items, true);
		feedWorks += works.size - before;
		if (!res?.hasNextPage) break;
	}
	const latest = await fetchList('SEARCH', VARS.latestAdded(), (d) => d.search);
	dataset.latestAddedTotal = latest?.total ?? null;
	take(latest?.items);
	// Browse page 1 always, for the catalogue total; more pages only to fill the pool.
	for (let page = 1; page === 1 || (works.size < GLOBAL.pool && page <= 40); page++) {
		const res = await fetchList('SEARCH', VARS.browse(page), (d) => d.search);
		if (page === 1) dataset.browseTotal = res?.total ?? null;
		take(res?.items);
		if (!res?.hasNextPage) break;
	}
	if (!works.size) {
		die(
			`no work ids came back from discovery / updatesFeed / search.\n       ${problems.join('\n       ')}`,
		);
	}

	// Feed works first, so a pool or sample smaller than what came back keeps them.
	// (Array sort is stable, so the seeded shuffle decides the order inside each group.)
	const all = shuffle([...works.values()], rng)
		.sort((a, b) => Number(b.fromFeed) - Number(a.fromFeed))
		.slice(0, GLOBAL.pool);
	const sample = all.slice(0, Math.max(1, Math.round(GLOBAL.sample ?? all.length)));

	// Search words: what people type is the start of a title.
	const terms = new Set();
	for (const w of all) {
		const words = w.title
			.toLowerCase()
			.split(/[^\p{L}\p{N}]+/u)
			.filter((x) => x.length >= 2);
		if (words[0]?.length >= 4) terms.add(words[0]);
		if (words.length >= 2) terms.add(`${words[0]} ${words[1]}`);
		const long = words.find((x) => x.length >= 6);
		if (long) terms.add(long);
	}

	// Series pages: chapter ids, and warm caches. A slow first answer is usually a cold
	// source cache, so slow works get one more try before they are left out.
	const opened = new Array(sample.length).fill(null);
	const times = [];
	const excluded = [];
	const open = async (w) => {
		const r = await call(anon(), 'SERIES_PAGE', VARS.series(w.id));
		return { r, bundle: r.kind === null ? r.json?.data?.seriesPage : null };
	};
	await pmap(sample, GLOBAL.setupConcurrency, async (w, idx) => {
		let { r, bundle } = await open(w);
		if (bundle && r.ms > GLOBAL.prewarmMax) ({ r, bundle } = await open(w));
		if (!bundle || r.ms > GLOBAL.prewarmMax) {
			excluded.push({
				id: w.id,
				reason: bundle ? 'slow' : 'error',
				ms: +r.ms.toFixed(1),
				kind: r.kind,
				message: r.kind ? (r.messages[0] ?? '').slice(0, 160) : null,
			});
			if (r.kind !== null && problems.length < 8)
				problems.push(`seriesPage ${w.id}: ${r.kind} ${r.messages[0] ?? ''}`.trim());
			return;
		}
		times.push(r.ms);
		const seen = new Set();
		const chapters = [];
		const lists = [
			bundle.spineChapters ?? [],
			...(bundle.sources ?? []).map((s) => s.chapters ?? []),
		];
		for (const list of lists) {
			for (const c of list) {
				if (!c?.id || seen.has(c.id) || chapters.length >= 120) continue;
				seen.add(c.id);
				chapters.push({
					id: String(c.id),
					pages: Number(c.pageCount) || 0,
					number: c.number ?? null,
				});
			}
		}
		const covers = new Set();
		if (bundle.series?.coverUrl) covers.add(bundle.series.coverUrl);
		else if (w.cover) covers.add(w.cover);
		const newest = chapters.length
			? chapters.reduce((a, c) => (chapterNumber(c) > chapterNumber(a) ? c : a))
			: null;
		opened[idx] = {
			id: bundle.workId || w.id,
			fromFeed: w.fromFeed,
			chapters,
			covers: [...covers],
			newest,
		};
	});
	// In sample order, not in the order the answers arrived: picks by index stay seeded.
	const series = opened.filter(Boolean);
	excluded.sort((a, b) => (a.id < b.id ? -1 : 1));
	const readable = series.filter((s) => s.chapters.length);
	// The hot order: `--hot-works N` takes the first N, so smaller hot sets nest.
	const hotOrder = shuffle([...readable], makeRng(GLOBAL.seed, 'hot'));

	const workIds = all.map((w) => w.id);
	const info = {
		seed: GLOBAL.seed,
		worksFound: works.size,
		works: all.length,
		sampled: sample.length,
		nonCanonicalIdsSkipped: other,
		seriesPagesWarm: series.length,
		seriesPagesDropped: excluded.length,
		excluded,
		seriesPageMs: dist(times),
		worksWithChapters: readable.length,
		chapters: readable.reduce((n, s) => n + s.chapters.length, 0),
		searchTerms: terms.size,
		dataset,
		workIds,
		hotOrder: hotOrder.map((w) => w.id),
		digest: sha256(
			JSON.stringify({
				workIds,
				chapters: readable.map((s) => [s.id, s.chapters.map((c) => c.id)]),
			}),
		),
		problems,
	};
	console.log(
		`pool      ${info.works} works, ${info.seriesPagesWarm} series pages warm ` +
			`(p50 ${info.seriesPageMs.p50 ?? '-'} ms, max ${info.seriesPageMs.max ?? '-'} ms, ${excluded.length} left out), ` +
			`${info.chapters} chapters in ${info.worksWithChapters} works, ${info.searchTerms} search terms`,
	);
	console.log(`          seed ${GLOBAL.seed}, digest ${info.digest.slice(0, 16)}`);
	for (const p of problems) console.log(`          ! ${p}`);
	if (!series.length) die('no series page answered; nothing to run against');
	if (!terms.size) terms.add('the');
	return { series, readable, hotOrder, terms: [...terms], allWorks: all, info };
}

function imageRoute(pathname) {
	if (pathname.startsWith('/covers/')) return 'covers';
	if (pathname.startsWith('/avatars/')) return 'avatars';
	if (pathname.startsWith('/comment-media/')) return 'comment-media';
	if (pathname === '/img') return 'img-proxy';
	if (/^\/api\/v1\/manga\/\d+\/thumbnail$/.test(pathname)) return 'engine-thumbnail';
	if (/^\/api\/v1\/manga\/\d+\/chapter\/\d+\/page\/\d+$/.test(pathname)) return 'engine-page';
	return null;
}

/** True when a query value is itself an address (a proxy's `src=`, `source=`, `url=` …). */
function nestedSource(u) {
	for (const [, v] of u.searchParams) if (/^([a-z][a-z0-9+.-]*:)?\/\//i.test(v.trim())) return true;
	return false;
}

/**
 * IMAGE POLICY. The server has no page image cache: a page route goes to the engine and
 * `/img` fetches upstream on every request. So an address is one of
 *   store    a server cover route (`/covers/…`, also `?w=`), served from the cover store
 *   fixture  an address from --fixture-images
 *   real     everything else: engine pages and thumbnails, `/img?src=…`, any address with
 *            a nested source in its query. Fetched only under --real-source-sample N, and
 *            never more than N times in the whole run.
 */
export function imageClass(route, u) {
	if (route === 'fixture') return 'fixture';
	if ((route === 'covers' || route === 'covers:w192') && !nestedSource(u)) return 'store';
	return 'real';
}

/**
 * The address the web reader would load for an image `sourceUrl` (IMAGE MODEL above),
 * as `{ url, route, cls }`, or `{ skip }` with the reason it is not loaded. An address
 * that carries a production host anywhere in its query is always skipped.
 */
export function resolveImage(raw, target, imgBase, ownHosts = new Set()) {
	if (!raw || typeof raw !== 'string') return { skip: 'empty' };
	if (urlQueryNamesProduction(raw)) return { skip: 'production-in-query' };
	let abs = null;
	try {
		abs = new URL(raw);
	} catch {
		abs = null;
	}
	const done = (u, extra = {}) => {
		if (u.pathname !== '/img') u.searchParams.set('cache_policy', '2');
		if (urlQueryNamesProduction(u.href)) return { skip: 'production-in-query' };
		const route = imageRoute(u.pathname) ?? 'own-other';
		return { url: u.href, route, cls: imageClass(route, u), ...extra };
	};
	if (!abs) {
		const u = new URL(raw, target);
		if (!imageRoute(u.pathname)) return { skip: 'unknown-relative' };
		return done(u);
	}
	if (abs.protocol !== 'http:' && abs.protocol !== 'https:') return { skip: 'scheme' };
	const onTarget = abs.host === target.host;
	const elsewhere = !onTarget && (ownHosts.has(abs.hostname) || isProductionHost(abs.hostname));
	if (onTarget || elsewhere) {
		const route = imageRoute(abs.pathname);
		if (!onTarget && !route)
			return { skip: isProductionHost(abs.hostname) ? 'production-host' : 'unknown-own' };
		const u = new URL(`${abs.pathname}${abs.search}`, target);
		return done(u, onTarget ? {} : { rewrittenFrom: abs.hostname });
	}
	const p = new URL('/img', imgBase);
	p.search = `src=${encodeURIComponent(raw)}&cache_policy=2`;
	return { ...done(p), foreign: abs.hostname };
}

function imageItem(resolved, label, work) {
	const u = new URL(resolved.url);
	return {
		url: resolved.url,
		u,
		route: resolved.route,
		cls: resolved.cls ?? imageClass(resolved.route, u),
		label,
		work,
	};
}

// ---------------------------------------------------------------------------
// The real-source budget: --real-source-sample N, for the whole run
// ---------------------------------------------------------------------------

const REAL_SOURCE_MAX = 200;
const REAL = { cap: 0, used: 0, refusedAfterCap: 0, capHitAt: null };

/** May one more real-source image be fetched? Counts it when yes. */
function takeReal() {
	if (REAL.used < REAL.cap) {
		REAL.used++;
		return true;
	}
	REAL.refusedAfterCap++;
	REAL.capHitAt ??= new Date().toISOString();
	return false;
}
const realLeft = () => REAL.used < REAL.cap;

/** Fetch one image inside a step, honouring the policy. Null when not sent. */
function performImage(run, vu, img) {
	if (img.cls === 'real' && !takeReal()) return null;
	return run.perform(R.image(vu, img));
}

// ---------------------------------------------------------------------------
// Fixtures: --fixture-images
// ---------------------------------------------------------------------------

/** The request address: what is sent, so the fragment is not part of it. */
const canonicalUrl = (u) => `${u.origin}${u.pathname}${u.search}`;

/**
 * --fixture-images is a URL template with {n} (and optionally {run}), or a file of
 * addresses (one per line, or a JSON list). Validated before any request: every origin
 * goes through the production refusal and the DNS check like the target.
 *   page stand-ins  harvested chapter pages map to fixture addresses (page k -> n =
 *                   start + k, or the first part of the file), for image-warm, mixed and
 *                   release-burst. The same page is the same address every time.
 *   cold            image-cold takes addresses after the stand-ins, each once. They are
 *                   deduplicated on the request address (no fragment).
 */
export function loadFixtures(spec, runId, target, start = 1) {
	const item = (href) => {
		const u = new URL(href, target);
		if (u.protocol !== 'http:' && u.protocol !== 'https:')
			throw new UsageError(`--fixture-images: not http(s): ${href}`);
		if (urlQueryNamesProduction(u.href))
			refuseProduction(`--fixture-images address carries production in its query`);
		return {
			url: canonicalUrl(u),
			u: new URL(canonicalUrl(u)),
			route: 'fixture',
			cls: 'fixture',
			label: 'image:fixture',
			work: null,
		};
	};
	const isTemplate = spec.includes('{n}');
	if (/^https?:\/\//i.test(spec) && !isTemplate)
		throw new UsageError('--fixture-images: a URL template needs {n} so every cold address is new');
	if (isTemplate) {
		const expand = (n) => spec.replaceAll('{n}', String(n)).replaceAll('{run}', runId);
		const a = new URL(expand(1), target);
		const b = new URL(expand(2), target);
		if (a.origin !== b.origin)
			throw new UsageError('--fixture-images: {n} must not be in the host or port');
		if (canonicalUrl(a) === canonicalUrl(b))
			throw new UsageError(
				'--fixture-images: {n} only changes the #fragment, which is never sent; put it in the path or query',
			);
		let standIns = 0;
		let next = null;
		const sent = new Set();
		let duplicates = 0;
		return {
			kind: 'template',
			origins: [a.origin],
			spec,
			setStandIns(p) {
				standIns = p;
			},
			standIn: (k) => item(expand(start + k)),
			coldNext() {
				next ??= start + standIns;
				for (;;) {
					const it = item(expand(next++));
					if (sent.has(it.url)) {
						duplicates++;
						continue;
					}
					sent.add(it.url);
					return { ...it, label: 'image:cold' };
				}
			},
			report: () => ({
				source: 'template',
				spec: redactUrl(spec),
				origins: [a.origin],
				standIns,
				coldFirstN: start + standIns,
				coldNextN: next,
				coldUsed: sent.size,
				duplicates,
			}),
		};
	}
	let text;
	try {
		text = readFileSync(spec, 'utf8');
	} catch (e) {
		throw new UsageError(`--fixture-images ${spec}: ${e.message}`);
	}
	const trimmed = text.trim();
	let lines;
	try {
		lines = trimmed.startsWith('[')
			? JSON.parse(trimmed).map(String)
			: trimmed
					.split('\n')
					.map((l) => l.trim())
					.filter((l) => l && !l.startsWith('#'));
	} catch (e) {
		throw new UsageError(`--fixture-images ${spec}: ${e.message}`);
	}
	const seen = new Set();
	const items = [];
	let duplicates = 0;
	for (const l of lines) {
		const it = item(l.replaceAll('{run}', runId));
		if (seen.has(it.url)) {
			duplicates++;
			continue;
		}
		seen.add(it.url);
		items.push(it);
	}
	if (!items.length) throw new UsageError(`--fixture-images ${spec}: no addresses`);
	let standIns = 0;
	let i = null;
	return {
		kind: 'file',
		origins: [...new Set(items.map((x) => x.u.origin))],
		spec,
		// The stand-ins take at most half the file, so cold addresses remain.
		setStandIns(p) {
			standIns = Math.min(p, Math.ceil(items.length / 2));
		},
		standIn: (k) => (standIns ? items[k % standIns] : null),
		coldNext() {
			i ??= standIns;
			return i < items.length ? { ...items[i++], label: 'image:cold' } : null;
		},
		report: () => ({
			source: 'file',
			spec,
			sha256: sha256(text),
			origins: [...new Set(items.map((x) => x.u.origin))],
			urls: items.length,
			duplicatesDropped: duplicates,
			standIns,
			coldAvailable: items.length - standIns,
			coldUsed: i === null ? 0 : i - standIns,
			exhausted: i !== null && i >= items.length,
		}),
	};
}

let FIXTURES = null;

// ---------------------------------------------------------------------------
// Setup: image addresses (only when a step needs images)
// ---------------------------------------------------------------------------

async function harvestImages(maxBurstWorks, needPages) {
	const rng = makeRng(GLOBAL.seed, 'images');
	const skipped = {};
	const rewritten = {};
	const foreign = {};
	const resolveOne = (raw) => {
		const r = resolveImage(raw, base, IMG_BASE, OWN_HOSTS);
		if (r.skip) tally(skipped, r.skip);
		if (r.rewrittenFrom) tally(rewritten, r.rewrittenFrom);
		if (r.foreign) tally(foreign, r.foreign);
		if (!r.skip && r.cls === 'real' && REAL.cap === 0) {
			tally(skipped, 'real-source (no --real-source-sample)');
			return null;
		}
		return r.skip ? null : r;
	};

	// Burst works first (the feed's works, newest chapter), then the hot order.
	const feedFirst = [...POOL.hotOrder].sort((a, b) => Number(b.fromFeed) - Number(a.fromFeed));
	const burst = feedFirst.filter((w) => w.newest).slice(0, Math.max(0, maxBurstWorks));
	const chosen = [];
	const pickedIds = new Set();
	const add = (w, c) => {
		if (!c || pickedIds.has(c.id) || chosen.length >= GLOBAL.imageChapters) return;
		pickedIds.add(c.id);
		chosen.push({ work: w, ch: c });
	};
	if (needPages) {
		for (const w of burst) add(w, w.newest);
		for (const w of POOL.hotOrder) add(w, w.newest);
		for (let pass = 0; pass < 4 && chosen.length < GLOBAL.imageChapters; pass++)
			for (const w of POOL.hotOrder) add(w, pickOf(w.chapters, rng));
	}

	// Page lists, only when page images can be fetched (fixture stand-ins or a real
	// sample). Each may make the server ask MangaDex or a source site once; the server
	// memoizes them, and only these chapters are read later.
	let setupClient = 0;
	const anon = () => ({ token: null, xff: xffFor(0xe00000 + setupClient++) });
	const pageLists = { asked: chosen.length, ok: 0, empty: 0, failed: 0, problems: [] };
	await pmap(chosen, GLOBAL.setupConcurrency, async ({ ch }) => {
		const numeric = /^\d+$/.test(ch.id);
		const r = await call(anon(), numeric ? 'PAGES' : 'CANONICAL_PAGES', { chapterId: ch.id });
		const list =
			r.kind === null ? (numeric ? r.json?.data?.pages : r.json?.data?.canonicalPages) : null;
		if (!Array.isArray(list)) {
			pageLists.failed++;
			if (pageLists.problems.length < 8)
				pageLists.problems.push(`${ch.id}: ${r.kind} ${r.messages[0] ?? ''}`.trim());
			return;
		}
		if (!list.length) pageLists.empty++;
		else pageLists.ok++;
		ch.pageSources = [...list]
			.sort((a, b) => (a.index ?? 0) - (b.index ?? 0))
			.map((p) => p.sourceUrl);
	});

	// Covers: the master, and the card size for a resizable store cover.
	const coverItems = [];
	const coverCap = Math.floor(GLOBAL.imageUrls / 2);
	for (const w of [...POOL.hotOrder, ...POOL.series.filter((s) => !s.chapters.length)]) {
		for (const raw of w.covers) {
			if (coverItems.length >= coverCap) break;
			const r = resolveOne(raw);
			if (!r) continue;
			coverItems.push(imageItem(r, 'image:cover', w.id));
			if (r.route === 'covers' && coverItems.length < coverCap) {
				const card = new URL(r.url);
				card.searchParams.set('w', '192');
				coverItems.push(
					imageItem({ ...r, url: card.href, route: 'covers:w192' }, 'image:cover-card', w.id),
				);
			}
		}
	}

	// Pages: a fixture stand-in for each, else the real address under the sample cap.
	let left = GLOBAL.imageUrls - coverItems.length;
	const pageItems = [];
	let pageIndex = 0;
	FIXTURES?.setStandIns(Math.max(0, left));
	const realPages = { skipped: 0 };
	for (const { work, ch } of chosen) {
		if (!ch.pageSources) continue;
		const items = [];
		for (const raw of ch.pageSources) {
			if (left <= 0) break;
			if (FIXTURES) {
				const fx = FIXTURES.standIn(pageIndex++);
				if (!fx) break;
				items.push({ ...fx, label: 'image:page', work: work.id, standInFor: raw });
				left--;
				continue;
			}
			const r = resolveOne(raw);
			if (!r) {
				realPages.skipped++;
				continue;
			}
			items.push(imageItem(r, 'image:page', work.id));
			left--;
		}
		ch.pageImgs = items;
		pageItems.push(...items);
	}

	FIXTURES?.setStandIns(pageIndex);

	// One warm pass: every address once, unmeasured, so warm steps measure warm bytes.
	// Real-source addresses count against --real-source-sample here too.
	const all = [...coverItems, ...pageItems];
	const unique = [...new Map(all.map((i) => [i.url, i])).values()];
	const warm = {
		urls: unique.length,
		ok: 0,
		failed: 0,
		notFetchedRealCap: 0,
		bytes: 0,
		byKind: {},
		byRoute: {},
		byClass: {},
		ms: [],
	};
	const started = Date.now();
	await pmap(unique, Math.max(2, GLOBAL.setupConcurrency * 2), async (img) => {
		if (img.cls === 'real' && !takeReal()) {
			warm.notFetchedRealCap++;
			return;
		}
		await pace();
		const r = await send({
			image: true,
			url: img.u,
			route: img.route,
			label: img.label,
			xff: xffFor(0xd00000 + (setupClient++ % 4096)),
			timeoutMs: SETUP_TIMEOUT_MS,
		});
		const route = (warm.byRoute[img.route] ??= { urls: 0, ok: 0, bytes: 0 });
		const cls = (warm.byClass[img.cls] ??= { urls: 0, ok: 0, bytes: 0 });
		route.urls++;
		cls.urls++;
		route.bytes += r.bytes;
		cls.bytes += r.bytes;
		warm.bytes += r.bytes;
		if (r.kind === null) {
			route.ok++;
			cls.ok++;
			warm.ok++;
			warm.ms.push(r.ms);
			img.warm = true;
		} else {
			warm.failed++;
			tally(warm.byKind, r.kind);
		}
	});
	const okUrls = new Set(unique.filter((i) => i.warm).map((i) => i.url));
	const isWarm = (i) => okUrls.has(i.url);
	const warmPass = {
		...warm,
		ms: dist(warm.ms),
		seconds: +((Date.now() - started) / 1000).toFixed(2),
		at: new Date().toISOString(),
	};
	for (const { ch } of chosen) if (ch.pageImgs) ch.pageImgs = ch.pageImgs.filter(isWarm);
	const list = unique.filter(isWarm);
	const byClass = {};
	for (const i of list) tally(byClass, i.cls);
	const info = {
		policy:
			'store covers and fixture addresses only; real-source addresses (engine pages, /img, nested ' +
			'sources) only under --real-source-sample, counted against it everywhere',
		chaptersChosen: chosen.map(({ work, ch }) => ({
			work: work.id,
			chapter: ch.id,
			pages: ch.pageImgs?.length ?? 0,
		})),
		pageLists,
		coverUrls: coverItems.length,
		pageUrls: pageItems.length,
		pageStandIns: FIXTURES ? pageIndex : 0,
		realPagesSkipped: realPages.skipped,
		skipped,
		rewrittenToTarget: rewritten,
		viaProxyByHost: foreign,
		warmPass,
		warmUrls: list.length,
		warmByClass: byClass,
		burstWorks: burst.map((w) => w.id),
		note:
			'A store cover not yet in the cover store is made once from MangaDex on its first request ' +
			'(the warm pass). Page lists are the other bounded real-source calls of a run.',
	};
	console.log(
		`images    ${list.length}/${unique.length} warm (${JSON.stringify(byClass)}; ${pageItems.length} page addresses ` +
			`from ${pageLists.ok}/${pageLists.asked} page lists), ${(warm.bytes / 1048576).toFixed(1)} MiB in the warm pass`,
	);
	for (const p of pageLists.problems) console.log(`          ! pages ${p}`);
	return { list, burst, chosen, info };
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/** One letter off: swap two neighbours, drop one, double one, or replace one. The
 *  first letter is kept, as typos mostly are. */
function typo(term, r) {
	const chars = [...term];
	const spots = [];
	for (let i = 1; i < chars.length; i++) if (/\p{L}/u.test(chars[i])) spots.push(i);
	if (spots.length < 3) return term;
	const i = pickOf(spots, r);
	const how = randInt(4, r);
	if (how === 0 && i + 1 < chars.length && chars[i + 1] !== ' ')
		[chars[i], chars[i + 1]] = [chars[i + 1], chars[i]];
	else if (how === 1) chars.splice(i, 1);
	else if (how === 2) chars.splice(i, 0, chars[i]);
	else chars[i] = String.fromCharCode(97 + randInt(26, r));
	const out = chars.join('');
	return out === term ? `${term.slice(0, -1)}` : out;
}

const searchTerm = (vu) => {
	const t = pickOf(POOL.terms, vu.rand);
	return vu.rand() < args.typoShare ? typo(t, vu.rand) : t;
};
// Browse: most people stay on the first page.
const browsePage = (r) => (r() < 0.5 ? 1 : 2 + randInt(4, r));

const R = {
	discovery: (vu) => gql(vu, 'DISCOVERY', 'discovery', 'read'),
	updates: (vu) => gql(vu, 'UPDATES_FEED', 'updatesFeed', 'read', { page: 1 }),
	latestAdded: (vu) => gql(vu, 'SEARCH', 'search(latestAdded)', 'read', VARS.latestAdded()),
	browse: (vu) => gql(vu, 'SEARCH', 'search(browse)', 'read', VARS.browse(browsePage(vu.rand))),
	search: (vu) => gql(vu, 'SEARCH', 'search(text)', 'read', VARS.search(searchTerm(vu))),
	series: (vu, id) => {
		const work = id ?? pickOf(CTX.series, vu.rand).id;
		return gql(vu, 'SERIES_PAGE', 'seriesPage', 'read', VARS.series(work), { work });
	},
	reviews: (vu, id) =>
		gql(vu, 'REVIEWS', 'reviews', 'read', { seriesId: id, page: 1 }, { work: id }),
	myReview: (vu, id) => gql(vu, 'MY_REVIEW', 'myReview', 'read', { seriesId: id }, { work: id }),
	comments: (vu, type, id, work) =>
		gql(vu, 'COMMENTS', 'comments', 'read', { targetType: type, targetId: id, page: 1 }, { work }),
	// Numeric ids are a source's own chapters, the rest are MangaDex's (source.ts:3059).
	pages: (vu, id, work) =>
		/^\d+$/.test(id)
			? gql(vu, 'PAGES', 'pages', 'read', { chapterId: id }, { work })
			: gql(vu, 'CANONICAL_PAGES', 'canonicalPages', 'read', { chapterId: id }, { work }),
	recordView: (vu, id) =>
		gql(vu, 'RECORD_VIEW', 'recordView', 'write', { seriesId: id }, { work: id }),
	progress: (vu, chapterId, lastPageRead, read, work, label) =>
		gql(
			vu,
			'SET_PROGRESS',
			label ?? (read ? 'setProgress(read)' : 'setProgress'),
			'write',
			{ chapterId, lastPageRead, read },
			{ work },
		),
	mark: (vu, id, marked) =>
		gql(vu, 'MARK', marked ? 'mark' : 'unmark', 'write', { seriesId: id, marked }, { work: id }),
	postComment: (vu, type, id, work) =>
		gql(
			vu,
			'POST_COMMENT',
			'postComment',
			'write',
			{
				input: {
					targetType: type,
					targetId: id,
					parentId: null,
					body: `capacity probe ${vu.name} #${++vu.seq}`,
					hasSpoiler: false,
					mediaId: null,
				},
			},
			{ want: true, work },
		),
	deleteComment: (vu, id) => gql(vu, 'DELETE_COMMENT', 'deleteComment', 'write', { commentId: id }),
	image: (vu, img) => ({
		image: true,
		kind: 'image',
		label: img.label,
		route: img.route,
		url: img.u,
		xff: vu.xff,
		work: img.work,
	}),
};

function weighted(table) {
	const entries = Object.entries(table).filter(([, w]) => w > 0);
	const total = entries.reduce((s, [, w]) => s + w, 0);
	return (r) => {
		let x = r() * total;
		for (const [k, w] of entries) if ((x -= w) <= 0) return k;
		return entries[entries.length - 1][0];
	};
}

// The server allows 20 comments a minute per account (graphql/mod.rs comment_limiter).
// Stay under it per virtual user, or the probe would measure the limiter.
const COMMENT_GAP_MS = 3500;

function toggleMark(vu, run, workId) {
	const marked = !vu.marked.has(workId);
	if (marked) vu.marked.add(workId);
	else vu.marked.delete(workId);
	return run.perform(R.mark(vu, workId, marked));
}

/**
 * Comment on a chapter of `work`, or on the work itself when `ch` is null.
 *
 * The server only takes a chapter thread for a chapter it has stored (spine, or the
 * engine cache), and a series page can list source chapters that are in neither. Such
 * a work is remembered after its first "No such chapter" and gets series comments
 * from then on, so the refusal costs one request per work, not a steady error rate.
 */
async function postComment(vu, run, work, ch) {
	vu.pace.lastCommentAt = Date.now();
	const onChapter = ch && !work.noChapterThread;
	const r = await run.perform(
		onChapter
			? R.postComment(vu, 'chapter', ch.id, work.id)
			: R.postComment(vu, 'series', work.id, work.id),
	);
	const made = r.json?.data?.postComment?.id;
	if (made) vu.comments.push(made);
	else if (onChapter && r.messages.some((m) => /no such chapter/i.test(m)))
		work.noChapterThread = true;
}

function deleteComment(vu, run) {
	const id = vu.comments.shift();
	return id ? run.perform(R.deleteComment(vu, id)) : null;
}

/**
 * One progress write.
 *   sequential  each virtual user keeps ONE established chapter and writes page n+1 of
 *               it, then the last page with read: true, then moves to the next chapter
 *               of the same work. `seq:new` labels the first write of a chapter in this
 *               run (the row may still exist from an earlier run).
 *   random      a random chapter of a random work at a random page; 3 in 10 are the
 *               end-of-chapter write, which also runs the catch-up cascade. This is what
 *               runs before 2026-10-14 did.
 */
function progressWrite(vu, run) {
	const r = vu.rand;
	if (args.progressPattern === 'sequential') {
		let c = vu.cursor;
		if (!c) {
			const work = pickOf(CTX.readable, r);
			c = vu.cursor = { work, idx: randInt(work.chapters.length, r), page: 0, fresh: true };
		}
		const ch = c.work.chapters[c.idx];
		const last = (ch.pages > 0 ? ch.pages : args.chapterPages) - 1;
		const done = c.page >= last;
		const label = done
			? 'setProgress(seq:read)'
			: c.fresh
				? 'setProgress(seq:new)'
				: 'setProgress(seq)';
		const req = R.progress(vu, ch.id, Math.min(c.page, last), done, c.work.id, label);
		c.fresh = false;
		if (done) {
			c.idx = (c.idx + 1) % c.work.chapters.length;
			c.page = 0;
			c.fresh = true;
		} else c.page++;
		return run.perform(req);
	}
	const work = pickOf(CTX.readable, r);
	const ch = pickOf(work.chapters, r);
	const pages = ch.pages > 0 ? ch.pages : args.chapterPages;
	return r() < 0.3
		? run.perform(
				R.progress(vu, ch.id, Math.max(0, pages - 1), true, work.id, 'setProgress(random:read)'),
			)
		: run.perform(R.progress(vu, ch.id, randInt(pages, r), false, work.id, 'setProgress(random)'));
}

const STOP = Symbol('stop');

const ACTIONS = {
	read: (vu, run) => run.perform(R[CTX.pickRead(vu.rand)](vu)),
	write: (vu, run) => {
		let k = CTX.pickWrite(vu.rand);
		if (k === 'comment') {
			if (vu.comments.length) return deleteComment(vu, run);
			if (Date.now() - vu.pace.lastCommentAt < COMMENT_GAP_MS) k = 'progress';
		}
		if (k === 'comment') {
			const work = pickOf(CTX.readable, vu.rand);
			return postComment(vu, run, work, vu.rand() < 0.7 ? pickOf(work.chapters, vu.rand) : null);
		}
		if (k === 'mark') return toggleMark(vu, run, pickOf(CTX.series, vu.rand).id);
		return progressWrite(vu, run);
	},
	'image-warm': (vu, run) => {
		// Once the real-source sample is used up, only store and fixture addresses remain.
		const list = realLeft() ? CTX.images : CTX.imagesNoReal;
		if (!list.length) {
			CTX.realExhausted = true;
			return STOP;
		}
		return performImage(run, vu, pickOf(list, vu.rand));
	},
	'image-cold': (vu, run) => {
		const img = CTX.cold.coldNext();
		if (!img) {
			CTX.coldExhausted = true;
			return STOP;
		}
		return run.perform(R.image(vu, img));
	},
};

// ---------------------------------------------------------------------------
// Load shapes
// ---------------------------------------------------------------------------

async function closedLoop(run, vus, action) {
	await Promise.all(
		vus.map(async (vu) => {
			while (!run.over()) if ((await action(vu, run)) === STOP) return;
		}),
	);
}

async function openLoop(run, rps, vus, action) {
	// Fire on a fixed schedule whatever the server does, so a stall shows up as latency
	// and errors instead of as a politely lower send rate.
	const inflight = new Set();
	const gapMs = 1000 / rps;
	const start = Date.now();
	let sent = 0;
	while (!run.over()) {
		const wait = start + sent * gapMs - Date.now();
		if (wait > 1) await sleep(wait);
		// Behind schedule: still let responses in between sends.
		else if (sent % 16 === 15) await new Promise(setImmediate);
		if (run.over()) break;
		const res = action(vus[sent % vus.length], run);
		if (res === STOP) break;
		const p = Promise.resolve(res).finally(() => inflight.delete(p));
		inflight.add(p);
		sent++;
	}
	await Promise.allSettled([...inflight]);
}

const pagesOf = (ch) =>
	ch.pageImgs?.length
		? ch.pageImgs.length
		: ch.pages > 0
			? Math.min(ch.pages, 80)
			: args.chapterPages;

/**
 * Read `share` of a chapter over `readingMs`. Without images that is one sleep. With
 * images (mixed, release-burst) the reader loads `--prefetch-pages` at once when the
 * chapter opens, then one more each time a page is turned. False when the step ended.
 */
async function readPages(vu, run, ch, readingMs, share = 1) {
	const imgs = CTX.withImages ? ch.pageImgs : null;
	if (!imgs?.length) return run.nap(readingMs * share);
	const n = imgs.length;
	const per = readingMs / n;
	const ahead = Math.max(1, Math.round(args.prefetchPages));
	let next = 0;
	const loadTo = (k) => {
		const batch = [];
		while (next < Math.min(n, k)) batch.push(performImage(run, vu, imgs[next++]));
		return Promise.all(batch);
	};
	await loadTo(ahead);
	const turns = Math.max(1, Math.round(n * share));
	for (let p = 1; p < turns; p++) {
		if (!(await run.nap(per))) return false;
		await loadTo(p + ahead);
	}
	return run.nap(per);
}

/** One signed-in reader. See SESSION MODEL at the top for the file:line of each step. */
async function sessionUser(run, vu) {
	const r = vu.rand;
	const think = () => run.nap(args.think * 1000 * jitter(0.5, r));
	const chapterMs = (ch) => pagesOf(ch) * args.pageSeconds * 1000;
	// Start each user somewhere inside its cycle, in proportion to the time a cycle
	// spends there. Otherwise every user opens home at second 0 and a short window
	// sees mostly page opens and almost no reading.
	const browseMs = args.think * 1000 * (3 + args.searchShare);
	const readMs =
		args.chapters * (args.chapterPages * args.pageSeconds * 1000 + args.nextSeconds * 1000);
	let resumeAt = r() < readMs / (readMs + browseMs) ? r() : null;
	if (resumeAt === null && !(await run.nap(r() * args.think * 1000))) return;

	while (!run.over()) {
		const work = pickOf(CTX.readable, r);
		if (resumeAt === null) {
			// Home.
			await Promise.all([
				run.perform(R.discovery(vu)),
				run.perform(R.latestAdded(vu)),
				run.perform(R.updates(vu)),
			]);
			if (!(await think())) return;
			if (r() < args.searchShare) {
				await run.perform(R.search(vu));
				if (!(await think())) return;
			}
			// Series page: the bundle, then the reviews and the reader's own review.
			await run.perform(R.series(vu, work.id));
			await run.perform(R.reviews(vu, work.id));
			await run.perform(R.myReview(vu, work.id));
			if (!(await think())) return;
			if (r() < args.markShare) await toggleMark(vu, run, work.id);
		}
		const first = randInt(work.chapters.length, r);
		for (let k = 0; k < args.chapters; k++) {
			const ch = work.chapters[(first + k) % work.chapters.length];
			const last = pagesOf(ch) - 1;
			let reading = chapterMs(ch) * jitter(0.3, r);
			let resumed = false;
			if (resumeAt !== null) {
				// Already open when the step began: only the rest of the chapter is left,
				// and its images are already loaded.
				reading *= 1 - resumeAt;
				resumeAt = null;
				resumed = true;
			} else {
				// Chapter open.
				const open = [run.perform(R.series(vu, work.id))];
				if (args.withPages || CTX.withImages) open.push(run.perform(R.pages(vu, ch.id, work.id)));
				await Promise.all(open);
				await Promise.all([
					run.perform(R.recordView(vu, work.id)),
					run.perform(R.comments(vu, 'chapter', ch.id, work.id)),
				]);
			}
			if (r() < args.quitShare) {
				// Left half-way: beforeNavigate + onDestroy, both with read: false.
				const share = r();
				if (!(await (resumed ? run.nap(reading * share) : readPages(vu, run, ch, reading, share))))
					return;
				const at = randInt(last + 1, r);
				await run.perform(R.progress(vu, ch.id, at, false, work.id));
				await run.perform(R.progress(vu, ch.id, at, false, work.id));
				break;
			}
			if (!(await (resumed ? run.nap(reading) : readPages(vu, run, ch, reading)))) return;
			// Reached the last page.
			await run.perform(R.progress(vu, ch.id, last, true, work.id));
			if (r() < args.commentShare && Date.now() - vu.pace.lastCommentAt > COMMENT_GAP_MS) {
				await postComment(vu, run, work, ch);
				if (!(await run.nap(2000))) return;
				await deleteComment(vu, run);
			}
			if (!(await run.nap(args.nextSeconds * 1000 * jitter(0.5, r)))) return;
			// Next chapter (beforeNavigate) ...
			await run.perform(R.progress(vu, ch.id, last, true, work.id));
			// ... or out of the reader after the last one (onDestroy as well).
			if (k === args.chapters - 1) await run.perform(R.progress(vu, ch.id, last, true, work.id));
		}
		if (!(await think())) return;
	}
}

/**
 * release-burst: every user waits for the measured window to open, then they all open
 * the same newest chapter (of `--burst-works` works, spread over the users) at that
 * instant, and read it with its images. `--bursts` > 1 repeats the opening every
 * `--burst-gap` seconds from the window start.
 */
async function burstUser(run, vu) {
	await run.whenOpen();
	for (let b = 0; b < args.bursts; b++) {
		const at = run.from + b * args.burstGap * 1000 - Date.now();
		if (at > 0 && !(await run.nap(at))) return;
		if (run.over()) return;
		const { work, ch } = CTX.burst[vu.i % CTX.burst.length];
		await run.perform(R.series(vu, work.id));
		await run.perform(R.pages(vu, ch.id, work.id));
		await Promise.all([
			run.perform(R.recordView(vu, work.id)),
			run.perform(R.comments(vu, 'chapter', ch.id, work.id)),
		]);
		const reading = pagesOf(ch) * args.pageSeconds * 1000 * jitter(0.3, vu.rand);
		if (!(await readPages(vu, run, ch, reading))) return;
		await run.perform(R.progress(vu, ch.id, pagesOf(ch) - 1, true, work.id));
	}
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

function makeVus(count, accounts, stream) {
	return Array.from({ length: count }, (_, i) => {
		const account = accounts.length ? accounts[i % accounts.length] : null;
		return {
			i,
			name: account?.username ?? `anon${i + 1}`,
			// Signed-in users keep their account's address; anonymous ones get their own.
			xff: account && i < accounts.length ? account.xff : xffFor(0x100000 + i),
			token: account?.token ?? null,
			rand: makeRng(GLOBAL.seed, `${stream}:vu${i}`),
			marked: new Set(),
			comments: [],
			cursor: null,
			// Shared by every virtual user of one account: the comment limit is per account.
			pace: account ? (account.pace ??= { lastCommentAt: 0 }) : { lastCommentAt: 0 },
			seq: 0,
		};
	});
}

const loadLabel = (s) => (s.users ? `${s.users}u` : s.vus ? `${s.vus}vu` : `${s.rate}rps`);

/**
 * One load point. The load starts at once; the measured window opens after the
 * warm-up AND, with --host-cmd, once the host command printed READY (its baseline is
 * taken), so a burst at the window start is inside the host window. Metrics are
 * scraped at the window open, during it, and AT its close (not after the drain).
 */
async function loadPoint(spec, accounts, stepName, pointIndex) {
	const sc = SCENARIOS[args.scenario];
	const count = spec.users ?? spec.vus ?? (sc.accounts ? accounts.length || 64 : 256);
	const vus = makeVus(count, sc.accounts ? accounts : [], `${stepName}:${pointIndex}`);
	const run = new Run();
	const lag = monitorEventLoopDelay({ resolution: 10 });
	const metrics = metricsRecorder();
	const label = `${stepName}-${loadLabel(spec)}`;
	let host = null;
	let hostReady = null;
	let loadDone = false;
	let boundaryAt = null;
	let inflightAtBoundary = null;
	let metricsP = null;
	let boundaryTimer = null;
	const markBoundary = () => {
		if (boundaryAt !== null) return;
		boundaryAt = Date.now();
		run.boundary = boundaryAt;
		inflightAtBoundary = run.inflight;
		lag.disable();
		metricsP = metrics ? metrics.end() : null;
		run.wake();
	};
	const opener = (async () => {
		const warmEnd = Date.now() + args.warmup * 1000;
		while (Date.now() < warmEnd && !stopRequested() && !loadDone)
			await sleep(Math.min(100, warmEnd - Date.now()));
		if (GLOBAL.hostCmd && !stopRequested() && !loadDone) {
			host = hostCommand(label, args.duration);
			hostReady = await host.ready;
		}
		const seconds = stopRequested() || loadDone ? 0 : args.duration;
		run.open(Date.now(), seconds);
		lag.enable();
		metrics?.begin();
		boundaryTimer = setTimeout(markBoundary, seconds * 1000);
	})();

	if (args.scenario === 'release-burst') await Promise.all(vus.map((vu) => burstUser(run, vu)));
	else if (spec.users) await Promise.all(vus.map((vu) => sessionUser(run, vu)));
	else {
		// Cold addresses are used once each: none is spent before the window opens.
		if (args.scenario === 'image-cold') await Promise.race([run.whenOpen(), opener]);
		if (spec.rate) await openLoop(run, spec.rate, vus, ACTIONS[args.scenario]);
		else await closedLoop(run, vus, ACTIONS[args.scenario]);
	}
	loadDone = true;
	await opener;
	clearTimeout(boundaryTimer);
	markBoundary();
	run.close();
	const wall = Math.max(0.001, (boundaryAt - run.from) / 1000);
	// The histogram samples a 10 ms timer, so 10 ms of every value is the timer itself.
	const lagMs = lag.count ? +Math.max(0, lag.percentile(99) / 1e6 - 10).toFixed(1) : 0;
	const metricsOut = metricsP ? await metricsP : null;

	// Leave no test comments behind. Not measured, and after the last scrape.
	const leftovers = vus.filter((vu) => vu.comments.length);
	await pmap(leftovers, 4, async (vu) => {
		for (const id of vu.comments.splice(0)) await send(R.deleteComment(vu, id));
	});
	const hostOut = host ? await host.done : null;
	const summary = summarize(run.stats, wall, lagMs);
	return {
		...spec,
		label,
		startedAt: new Date(run.from).toISOString(),
		endedAt: new Date(boundaryAt).toISOString(),
		...summary,
		afterBoundary: {
			inflightAtBoundary,
			completedAfterBoundary: run.lateCompletions,
			notSentAfterDeadline: run.notSent,
		},
		...(args.scenario === 'image-cold' ? { coldExhausted: !!CTX.coldExhausted } : {}),
		...(CTX.withImages || args.scenario === 'image-warm'
			? {
					realSource: {
						cap: REAL.cap,
						usedSoFar: REAL.used,
						refusedAfterCapSoFar: REAL.refusedAfterCap,
					},
				}
			: {}),
		metrics: metricsOut,
		host: hostOut ? { ...hostOut, ready: hostReady } : null,
		costInputs: costInputs(summary, metricsOut, hostOut),
	};
}

/** The raw numbers a cost estimate needs, from what this point measured. */
function costInputs(s, m, h) {
	const hj = h?.json ?? null;
	return {
		measuredSeconds: s.seconds,
		requests: { read: s.read.requests, write: s.write.requests, image: s.image.requests },
		responseBytes: { read: s.read.bytes, write: s.write.bytes, image: s.image.bytes },
		serverByteCounters: m?.derived?.bytes ?? null,
		hostBusyCores: hj?.host?.busy_cores ?? null,
		hostCpuCoreSeconds: Number.isFinite(hj?.host?.busy_cores)
			? +(hj.host.busy_cores * s.seconds).toFixed(2)
			: null,
		walBytes: hj?.wal?.bytes ?? null,
		archivedSegments: hj?.archiver?.archived ?? null,
		databaseBytesDelta: hj?.database?.bytes_delta ?? null,
	};
}

const ms = (v) =>
	v === null || v === undefined ? '-' : v < 100 ? v.toFixed(1) : String(Math.round(v));
const COLS = [
	['load', 9],
	['req/s', 9],
	['reads/s', 9],
	['writes/s', 9],
	['r.p50', 8],
	['r.p95', 8],
	['r.p99', 8],
	['w.p50', 8],
	['w.p95', 8],
	['w.p99', 8],
	['err%', 7],
	['lag', 6],
];

function printHeader() {
	console.log(COLS.map(([h, w], i) => (i === 0 ? h.padEnd(w) : h.padStart(w))).join(''));
	console.log('-'.repeat(COLS.reduce((s, [, w]) => s + w, 0)));
}

function printRow(r) {
	const cells = [
		loadLabel(r),
		r.rps,
		r.readsPerSec,
		r.writesPerSec,
		ms(r.read.p50),
		ms(r.read.p95),
		ms(r.read.p99),
		ms(r.write.p50),
		ms(r.write.p95),
		ms(r.write.p99),
		(r.errorShare * 100).toFixed(2),
		ms(r.generatorLagP99Ms),
	];
	console.log(
		cells
			.map((c, i) => (i === 0 ? String(c).padEnd(COLS[i][1]) : String(c).padStart(COLS[i][1])))
			.join(''),
	);
	if (r.image.requests) {
		const mibs = (r.image.bytes / 1048576 / r.seconds).toFixed(2);
		console.log(
			`          images ${r.imagesPerSec}/s  ${mibs} MiB/s  p50 ${ms(r.image.p50)}  p95 ${ms(r.image.p95)}` +
				`  p99 ${ms(r.image.p99)}  err ${r.image.errors}`,
		);
	}
	if (r.errors) {
		const kinds = Object.entries(r.errorsByKind).map(([k, n]) => `${k}=${n}`);
		console.log(`          errors ${r.errors}/${r.requests}: ${kinds.join(' ')}`);
		for (const m of r.errorMessages.slice(0, 5))
			console.log(`            ${m.count}x ${m.message}`);
	}
	if (r.users) {
		const perMin = (v) => ((v / r.users) * 60).toFixed(2);
		console.log(
			`          per user per minute: ${perMin(r.readsPerSec)} reads, ${perMin(r.writesPerSec)} writes` +
				(r.image.requests ? `, ${perMin(r.imagesPerSec)} images` : ''),
		);
	}
	if (r.metrics) {
		const d = r.metrics.derived;
		const parts = [`metrics ${r.metrics.good}/${r.metrics.scrapes} scrapes`];
		if (d?.refusingLanes.length) parts.push(`refused by ${d.refusingLanes.join(', ')}`);
		for (const h of d?.hitRatios.slice(0, 4) ?? [])
			if (h.ratio !== null) parts.push(`${h.key} ${(h.ratio * 100).toFixed(1)}%`);
		console.log(`          ${parts.join('; ')}`);
	}
	if (r.host) console.log(`          host ${r.host.ok ? 'ok' : `failed: ${r.host.error}`}`);
	if (args.ops) {
		for (const [name, o] of Object.entries(r.ops)) {
			console.log(
				`          ${name.padEnd(24)} ${o.kind.padEnd(5)} ${String(o.perSec).padStart(8)}/s` +
					`  p50 ${ms(o.p50).padStart(6)}  p95 ${ms(o.p95).padStart(6)}  p99 ${ms(o.p99).padStart(6)}` +
					`  err ${o.errors}`,
			);
		}
	}
}

function stopReason(r) {
	const worst = Math.max(r.read.p95 ?? 0, r.write.p95 ?? 0);
	if (worst > args.maxP95) {
		const which = (r.read.p95 ?? 0) >= (r.write.p95 ?? 0) ? 'reads' : 'writes';
		return `p95 of ${which} ${ms(worst)} ms passed --max-p95 ${args.maxP95} ms`;
	}
	if ((r.image.p95 ?? 0) > args.maxImageP95)
		return `p95 of images ${ms(r.image.p95)} ms passed --max-image-p95 ${args.maxImageP95} ms`;
	if (r.errorShare > args.maxErrors) {
		let why = `error share ${(r.errorShare * 100).toFixed(2)}% passed ${(args.maxErrors * 100).toFixed(2)}%`;
		const refusing = r.metrics?.derived?.refusingLanes ?? [];
		if (refusing.length) why += `. Refusing lane(s) per /metrics: ${refusing.join(', ')}`;
		else if ((r.errorsByKind.http_503 || 0) > r.errors / 2)
			why +=
				'. Mostly 503 "Server busy": an admission lane (request_budget.rs) or a database pool ' +
				'refused; give --metrics-url to see which lane';
		return why;
	}
	return null;
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

function git(dir, gitArgs, max = 64 << 20) {
	return execFileSync('git', ['-C', dir, ...gitArgs], {
		encoding: 'utf8',
		stdio: ['ignore', 'pipe', 'ignore'],
		timeout: 20_000,
		maxBuffer: max,
	});
}

function gitInfo(dir) {
	const out = { head: null, dirtyFiles: null, untrackedFiles: null, diffSha256: null, error: null };
	try {
		out.head = git(dir, ['rev-parse', 'HEAD']).trim();
		const status = git(dir, ['status', '--porcelain']).split('\n').filter(Boolean);
		out.dirtyFiles = status.length;
		out.untrackedFiles = status.filter((l) => l.startsWith('??')).length;
		// The uncommitted change itself, as a hash: two runs with the same HEAD and the
		// same hash ran the same tracked source.
		out.diffSha256 = sha256(git(dir, ['diff', 'HEAD'], 512 << 20));
	} catch (e) {
		out.error = String(e.message ?? e).split('\n')[0];
	}
	return out;
}

const SECRET_KEY = /pass|secret|token|key|credential|private|auth|cookie|session|dsn/i;

/** The operator's capture of the server environment, with secrets taken out. */
export function readSettings(path) {
	const raw = readFileSync(path, 'utf8');
	let entries = [];
	let format = 'env';
	const t = raw.trim();
	if (t.startsWith('{') || t.startsWith('[')) {
		const j = JSON.parse(t);
		if (Array.isArray(j) && j[0]?.Config?.Env) {
			format = 'docker-inspect';
			entries = j[0].Config.Env.map((l) => [
				l.slice(0, l.indexOf('=')),
				l.slice(l.indexOf('=') + 1),
			]);
		} else if (Array.isArray(j)) {
			format = 'json-list';
			entries = j.map((l) => [String(l).split('=')[0], String(l).split('=').slice(1).join('=')]);
		} else {
			format = 'json';
			entries = Object.entries(j).map(([k, v]) => [
				k,
				typeof v === 'string' ? v : JSON.stringify(v),
			]);
		}
	} else {
		for (const line of raw.split('\n')) {
			const l = line.trim().replace(/^export\s+/, '');
			if (!l || l.startsWith('#') || !l.includes('=')) continue;
			const k = l.slice(0, l.indexOf('=')).trim();
			let v = l.slice(l.indexOf('=') + 1).trim();
			if (/^(['"]).*\1$/.test(v)) v = v.slice(1, -1);
			entries.push([k, v]);
		}
	}
	const values = {};
	const redactedKeys = [];
	for (const [k, v] of entries) {
		if (SECRET_KEY.test(k)) {
			values[k] = '<redacted>';
			redactedKeys.push(k);
		} else {
			// A password inside a URL (postgres://user:pw@host) never leaves this machine.
			const clean = v.replace(/(\/\/[^:/@\s]+:)[^@\s]+@/g, '$1<redacted>@');
			if (clean !== v) redactedKeys.push(k);
			values[k] = clean;
		}
	}
	// Hash the sorted entries, not the file: a recreated container lists the same
	// environment in another order, and that must not make two runs look different.
	const canonical = entries
		.map(([k, v]) => `${k}=${v}`)
		.sort()
		.join('\n');
	return {
		file: resolvePath(path),
		sha256: sha256(canonical),
		format,
		entries: entries.length,
		values,
		redactedKeys,
	};
}

/** Options safe to write: no password, command text and URLs with secrets replaced. */
function recordable(o) {
	const { password: _never, ...rest } = o;
	if (rest.hostCmd) rest.hostCmd = redactCommand(rest.hostCmd);
	for (const k of ['target', 'metricsUrl', 'imgBase', 'fixtureImages'])
		if (rest[k]) rest[k] = redactUrl(rest[k]);
	return rest;
}

function buildManifest(runId, suite, dnsLooked) {
	const toolPath = fileURLToPath(import.meta.url);
	const cpus = os.cpus();
	// Run-wide options only; every step records its own complete options.
	const options = recordable(Object.fromEntries([...GLOBAL_KEYS].map((k) => [k, GLOBAL[k]])));
	let settings = null;
	if (GLOBAL.settingsFile) {
		try {
			settings = readSettings(GLOBAL.settingsFile);
		} catch (e) {
			settings = { file: GLOBAL.settingsFile, error: e.message };
		}
	}
	return {
		runId,
		tool: { file: toolPath, sha256: sha256(readFileSync(toolPath)) },
		git: gitInfo(dirname(toolPath)),
		node: process.version,
		generator: {
			hostname: os.hostname(),
			cpus: cpus.length,
			cpuModel: cpus[0]?.model ?? null,
			platform: process.platform,
			arch: process.arch,
			memoryBytes: os.totalmem(),
			loadAvgAtStart: os.loadavg().map((v) => +v.toFixed(2)),
		},
		target: redactUrl(GLOBAL.target),
		metricsUrl: GLOBAL.metricsUrl ? redactUrl(GLOBAL.metricsUrl) : null,
		imageProxy: IMG_BASE?.origin ?? null,
		hostCommand: GLOBAL.hostCmd ? redactCommand(GLOBAL.hostCmd) : null,
		fixtureOrigins: FIXTURES?.origins ?? [],
		realSourceSample: { cap: GLOBAL.realSourceSample, max: REAL_SOURCE_MAX },
		imageDigest: GLOBAL.imageDigest,
		settings,
		seed: GLOBAL.seed,
		dns: dnsLooked,
		suite,
		options,
		startedAt: new Date().toISOString(),
		finishedAt: null,
		notes: [
			'Requests are what this generator sent and the server answered. A request count is not a ' +
				'number of people: no daily-active-user figure and no concurrency figure is derived from it.',
			'users in session, mixed and release-burst steps are simulated readers following the ' +
				'SESSION MODEL in capacity.mjs; how many real people one equals is an assumption.',
			'Image load fetches only store covers and --fixture-images addresses. Real-source images ' +
				'(engine pages, /img, nested sources) only under --real-source-sample, counted in `realSource`.',
			'Counter deltas cover the measured window (scrape at window open and close), not the warm-up.',
		],
	};
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

function loadSuite(path) {
	let raw;
	try {
		raw = readFileSync(path, 'utf8');
	} catch (e) {
		throw new UsageError(`--suite ${path}: ${e.message}`);
	}
	let s;
	try {
		s = JSON.parse(raw);
	} catch (e) {
		throw new UsageError(`--suite ${path}: not JSON (${e.message})`);
	}
	if (!Array.isArray(s.steps) || !s.steps.length)
		throw new UsageError(`--suite ${path}: "steps" must be a non-empty list`);
	return { raw, s };
}

/** Every step's effective options, in order. */
function planSteps(cli, suite) {
	const base0 = defaults();
	if (!suite) {
		const o = { ...base0, ...cli.explicit };
		return { global: o, steps: [{ name: o.label ?? o.scenario, requires: [], opts: o }] };
	}
	for (const k of Object.keys(cli.explicit))
		if (STEP_ONLY_KEYS.has(k))
			throw new UsageError(
				`${KEY_TO_FLAG[k]} belongs inside the suite's steps, not next to --suite`,
			);
	const sd = suiteOptions(suite.s.defaults, 'suite defaults: ', true);
	const global = { ...base0, ...sd, ...cli.explicit };
	const names = new Set();
	const steps = suite.s.steps.map((st, i) => {
		const where = `suite step ${i + 1}: `;
		if (!st.scenario) throw new UsageError(`${where}needs a "scenario"`);
		const own = suiteOptions(st, where, false);
		const opts = { ...base0, ...sd, ...own, ...cli.explicit };
		for (const k of GLOBAL_KEYS) opts[k] = global[k];
		const name = String(st.name ?? own.label ?? `${i + 1}-${st.scenario}`).replace(
			/[^A-Za-z0-9_.-]/g,
			'_',
		);
		if (names.has(name)) throw new UsageError(`${where}name '${name}' is used twice`);
		names.add(name);
		const requires = Array.isArray(st.requires) ? st.requires.map(String) : [];
		return { name, requires, note: st.note ?? null, opts };
	});
	return { global, steps };
}

function requirementMissing(step) {
	for (const r of step.requires) {
		if (r === 'fixture-images' && !GLOBAL.fixtureImages) return 'requires --fixture-images';
		if (r === 'metrics-url' && !GLOBAL.metricsUrl) return 'requires --metrics-url';
		if (r === 'host-cmd' && !GLOBAL.hostCmd) return 'requires --host-cmd';
	}
	if (SCENARIOS[step.opts.scenario].fixture && !GLOBAL.fixtureImages)
		return 'image-cold requires --fixture-images: sustained cold load must hit fixtures, not real sources';
	if (
		['mixed', 'release-burst'].includes(step.opts.scenario) &&
		!GLOBAL.fixtureImages &&
		!(GLOBAL.realSourceSample > 0)
	)
		return `${step.opts.scenario} page images need --fixture-images (or a bounded --real-source-sample)`;
	return null;
}

/** The works, images and pickers one step uses. Null plus a reason when it cannot run. */
function stepContext(o) {
	const sc = SCENARIOS[o.scenario];
	const mix = mixFor(o);
	const ctx = {
		mix,
		pickRead: o.scenario === 'read' ? weighted(mix) : null,
		pickWrite: o.scenario === 'write' ? weighted(mix) : null,
		withImages: sc.images,
		series: POOL?.series ?? [],
		readable: POOL?.readable ?? [],
		images: [],
		burst: [],
		cold: null,
		hot: null,
	};
	if (!sc.pool) return { ctx };
	if (o.hotWorks) {
		const hot = POOL.hotOrder.slice(0, Math.round(o.hotWorks));
		ctx.series = hot;
		ctx.readable = hot;
		ctx.hot = {
			requested: Math.round(o.hotWorks),
			used: hot.length,
			ids: hot.map((w) => w.id),
			distribution: 'uniform over the hot works',
			shortfall:
				hot.length < o.hotWorks ? `only ${hot.length} works with chapters in the pool` : null,
		};
	}
	if (sc.images) {
		const ids = new Set(ctx.readable.map((w) => w.id));
		ctx.images = IMAGES.list.filter((i) => !o.hotWorks || ids.has(i.work));
		ctx.imagesNoReal = ctx.images.filter((i) => i.cls !== 'real');
		if (o.hotWorks && !ctx.images.length) return { ctx, skip: 'no warm image of the hot works' };
		if (!IMAGES.list.length) return { ctx, skip: 'no warm image address' };
	}
	if (o.scenario === 'mixed') {
		ctx.readable = ctx.readable
			.map((w) => ({ ...w, chapters: w.chapters.filter((c) => c.pageImgs?.length) }))
			.filter((w) => w.chapters.length);
		if (!ctx.readable.length)
			return { ctx, skip: 'no chapter with a warm page list (raise --image-chapters)' };
	}
	if (o.scenario === 'release-burst') {
		ctx.burst = IMAGES.burst
			.slice(0, Math.max(1, Math.round(o.burstWorks)))
			.map((w) => ({ work: w, ch: w.newest }))
			.filter((b) => b.ch?.pageImgs?.length);
		if (!ctx.burst.length) return { ctx, skip: 'no newest chapter with a warm page list' };
	}
	if (sc.accounts && !ctx.readable.length)
		return { ctx, skip: 'no chapter ids in the chosen works; raise --pool / --sample' };
	return { ctx };
}

async function main(argv) {
	let cli;
	let plan;
	let suite = null;
	try {
		cli = parseCli(argv);
		if (cli.help) {
			usage();
			return 0;
		}
		// The production addresses before anything else; a bad entry stops the run.
		try {
			loadProductionConfig();
		} catch (e) {
			if (!(e instanceof ProductionConfigError)) throw e;
			console.error(`refusing to run: production address configuration: ${e.message}`);
			process.exit(3);
		}
		if (cli.explicit.suite) suite = loadSuite(cli.explicit.suite);
		plan = planSteps(cli, suite);
		GLOBAL = plan.global;
		// Production first, before any file, socket or name lookup.
		for (const st of [{ opts: GLOBAL }, ...plan.steps]) {
			const why = productionRefusal(st.opts);
			if (why) refuseProduction(why);
		}
		if (GLOBAL.list) {
			console.log('scenarios:', SCENARIO_NAMES);
			console.log('read     anonymous reads, one per turn:', JSON.stringify(BASE_MIX.read));
			console.log('write    signed-in writes, one per turn:', JSON.stringify(BASE_MIX.write));
			console.log('session  one signed-in reader per user: see SESSION MODEL in the source');
			console.log('image-*, mixed, release-burst: see IMAGE MODEL in the source');
			return 0;
		}
		for (const st of plan.steps) validateStep(st.opts);
		for (const [flag, u] of [
			['--target', GLOBAL.target],
			['--metrics-url', GLOBAL.metricsUrl],
			['--img-base', GLOBAL.imgBase],
		]) {
			if (!u) continue;
			try {
				new URL(u);
			} catch {
				throw new UsageError(`${flag}: not a URL: ${u}`);
			}
		}
		// Fixtures are read (a local file or a template) so every origin is checked like
		// the target before anything is sent.
		if (GLOBAL.fixtureImages) {
			FIXTURES = loadFixtures(
				GLOBAL.fixtureImages,
				RUN_ID,
				new URL(GLOBAL.target),
				GLOBAL.fixtureStart,
			);
			const why = productionRefusal(GLOBAL, FIXTURES.origins);
			if (why) refuseProduction(why);
		}
	} catch (e) {
		if (e instanceof UsageError) die(e.message, 2);
		throw e;
	}
	if (GLOBAL.queries) Q = { ...Q, ...JSON.parse(readFileSync(GLOBAL.queries, 'utf8')) };

	const { why, looked } = await productionByDns([
		GLOBAL.target,
		GLOBAL.metricsUrl,
		GLOBAL.imgBase,
		...(FIXTURES?.origins ?? []),
	]);
	if (why) refuseProduction(why);

	// Single scenario: image-cold without fixtures does not start at all.
	if (!suite) {
		const missing = requirementMissing(plan.steps[0]);
		if (missing) die(missing, 2);
	}

	GLOBAL.seed ??= randomBytes(6).toString('hex');
	base = new URL(GLOBAL.target);
	GQL_PATH = `${base.pathname.replace(/\/$/, '')}/graphql`;
	IMG_BASE = new URL(GLOBAL.imgBase ?? GLOBAL.target);
	OWN_HOSTS = new Set(
		String(GLOBAL.ownHosts ?? '')
			.split(',')
			.map((h) => h.trim().toLowerCase())
			.filter(Boolean),
	);
	args = GLOBAL;
	REAL.cap = GLOBAL.realSourceSample;
	const runId = RUN_ID;
	const suiteInfo = suite
		? {
				file: resolvePath(cli.explicit.suite),
				name: suite.s.name ?? null,
				sha256: sha256(suite.raw),
				steps: plan.steps.length,
			}
		: null;
	const manifest = buildManifest(runId, suiteInfo, looked);
	const jsonPath = GLOBAL.json ?? (suite ? `capacity-${runId}.json` : null);

	console.log(`target    ${redactUrl(GLOBAL.target)}`);
	console.log(
		`run       ${runId}, seed ${GLOBAL.seed}, git ${manifest.git.head?.slice(0, 12) ?? '-'}` +
			`${manifest.git.dirtyFiles ? ` (+${manifest.git.dirtyFiles} dirty)` : ''}`,
	);
	if (suite) console.log(`suite     ${suiteInfo.file} (${plan.steps.length} steps)`);
	if (GLOBAL.metricsUrl)
		console.log(`metrics   ${redactUrl(GLOBAL.metricsUrl)} every ${GLOBAL.metricsInterval}s`);

	const omitted = [];
	const runnable = [];
	for (const st of plan.steps) {
		const missing = requirementMissing(st);
		if (missing) omitted.push({ step: st.name, scenario: st.opts.scenario, reason: missing });
		else runnable.push(st);
	}

	// Accounts: once, for the largest step that needs them.
	let accountsWanted = 0;
	let largestStep = 0;
	for (const st of runnable) {
		if (!SCENARIOS[st.opts.scenario].accounts) continue;
		const loads = loadsFor(st.opts);
		const largest = Math.max(...loads.map((s) => s.users ?? s.vus ?? 0));
		largestStep = Math.max(largestStep, largest);
		accountsWanted = Math.max(accountsWanted, Math.round(GLOBAL.accounts ?? (largest || 64)));
	}
	const setup = accountsWanted
		? await setupAccounts(accountsWanted, largestStep)
		: { accounts: [], counts: {}, problem: null };

	if (runnable.some((st) => SCENARIOS[st.opts.scenario].pool)) POOL = await buildPool();
	const imageSteps = runnable.filter((st) => SCENARIOS[st.opts.scenario].images);
	if (imageSteps.length) {
		const maxBurst = Math.max(
			0,
			...imageSteps
				.filter((s) => s.opts.scenario === 'release-burst')
				.map((s) => Math.round(s.opts.burstWorks)),
		);
		const needPages = !!FIXTURES || REAL.cap > 0;
		IMAGES = await harvestImages(maxBurst, needPages);
	}
	const cold = FIXTURES;

	const out = {
		schema: 'komika-capacity/2',
		manifest,
		accounts: {
			wanted: accountsWanted,
			ready: setup.accounts.length,
			...setup.counts,
			problem: setup.problem,
		},
		pool: POOL?.info ?? null,
		images: IMAGES?.info ?? null,
		fixtures: null,
		realSource: null,
		omitted,
		steps: [],
	};
	const write = () => {
		if (!jsonPath) return;
		manifest.finishedAt = new Date().toISOString();
		out.fixtures = FIXTURES?.report() ?? null;
		out.realSource = {
			cap: REAL.cap,
			used: REAL.used,
			refusedAfterCap: REAL.refusedAfterCap,
			capHitAt: REAL.capHitAt,
		};
		const tmp = `${jsonPath}.tmp`;
		writeFileSync(tmp, JSON.stringify(out, null, 2));
		renameSync(tmp, jsonPath);
	};

	const done = [];
	if (!GLOBAL.setupOnly) {
		for (const [n, st] of plan.steps.entries()) {
			const o = st.opts;
			const skipped = omitted.find((x) => x.step === st.name);
			const record = {
				name: st.name,
				scenario: o.scenario,
				note: st.note ?? null,
				options: recordable(o),
				progressPattern: o.scenario === 'write' ? o.progressPattern : null,
				hotWorks: null,
				burst: null,
				cacheState: null,
				skipped: skipped?.reason ?? null,
				stopped: null,
				loads: [],
			};
			out.steps.push(record);
			if (skipped) {
				console.log(`\n== ${st.name} (${o.scenario}) skipped: ${skipped.reason}`);
				continue;
			}
			args = o;
			const { ctx, skip } = stepContext(o);
			ctx.cold = cold;
			CTX = ctx;
			record.hotWorks = ctx.hot;
			record.burst = ctx.burst.length
				? ctx.burst.map((b) => ({ work: b.work.id, chapter: b.ch.id, pages: b.ch.pageImgs.length }))
				: null;
			record.cacheState = {
				seriesPagesPrewarmed: POOL?.info.seriesPagesWarm ?? 0,
				imageWarmPass: IMAGES
					? {
							urls: IMAGES.info.warmPass.urls,
							ok: IMAGES.info.warmPass.ok,
							at: IMAGES.info.warmPass.at,
						}
					: null,
				earlierSteps: [...done],
				fixturesBefore: cold ? cold.report() : null,
				realSourceUsedBefore: REAL.used,
			};
			const loads = loadsFor(o);
			console.log(
				`\n== ${st.name} (${o.scenario}) step ${n + 1}/${plan.steps.length}: ` +
					`${loads.map(loadLabel).join(',')}; ${o.warmup}s warm-up + ${o.duration}s measured` +
					(o.hotWorks ? `; ${ctx.hot.used} hot works` : '') +
					(o.scenario === 'write' ? `; ${o.progressPattern} progress` : ''),
			);
			if (skip) {
				record.skipped = skip;
				omitted.push({ step: st.name, scenario: o.scenario, reason: skip });
				console.log(`          skipped: ${skip}`);
				continue;
			}
			printHeader();
			for (const [i, spec] of loads.entries()) {
				const r = await loadPoint(spec, setup.accounts, st.name, i);
				record.loads.push(r);
				printRow(r);
				write();
				if (r.generatorLagP99Ms > 50)
					console.log(
						`          ! generator event-loop lag p99 ${r.generatorLagP99Ms} ms: this step is limited ` +
							'by the load generator, not the server. Use a faster client or several processes.',
					);
				if (stopRequested()) {
					record.stopped = interrupted ? 'interrupted' : 'stop file';
					break;
				}
				if (o.scenario === 'image-warm' && CTX.realExhausted) {
					record.stopped = 'real-source sample used up and no store or fixture address left';
					break;
				}
				if (o.scenario === 'image-cold' && r.coldExhausted) {
					record.stopped = 'fixture addresses used up';
					break;
				}
				const why2 = stopReason(r);
				if (why2) {
					record.stopped = why2;
					console.log(`\n! ${why2}: stopping this ramp.`);
					break;
				}
			}
			done.push(st.name);
			const passed = record.loads.filter((r) => !stopReason(r) && r.requests > 0);
			if (!passed.length) console.log('result    no load stayed inside the limits');
			else {
				const best =
					SCENARIOS[o.scenario].shape === 'users'
						? passed[passed.length - 1]
						: passed.reduce((a, b) => (b.okPerSec > a.okPerSec ? b : a));
				console.log(
					`result    ${loadLabel(best)}: ${best.okPerSec} ok req/s (reads p95 ${ms(best.read.p95)} ms, ` +
						`writes p95 ${ms(best.write.p95)} ms, images p95 ${ms(best.image.p95)} ms)` +
						`${record.stopped ? '' : '; the ramp never hit a limit'}`,
				);
			}
			if (stopRequested()) break;
		}
	}
	write();
	if (jsonPath) console.log(`wrote     ${jsonPath}`);
	for (const a of agents.values()) a.destroy();
	return 0;
}

function isMain() {
	if (!process.argv[1]) return false;
	try {
		return import.meta.url === pathToFileURL(realpathSync(process.argv[1])).href;
	} catch {
		return false;
	}
}

if (isMain()) {
	process.on('SIGINT', () => {
		if (interrupted) process.exit(130);
		interrupted = true;
		console.error('\ninterrupted: finishing the step (Ctrl-C again to quit now)');
	});
	main(process.argv.slice(2)).then(
		(code) => process.exit(code),
		(e) => {
			console.error(e);
			process.exit(1);
		},
	);
}
