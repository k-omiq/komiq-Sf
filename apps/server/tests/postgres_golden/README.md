# PostgreSQL compatibility goldens

This is a Phase 0 baseline of the current SQLite GraphQL API, recorded by executing
real resolvers in process. It is not a PostgreSQL implementation or a production
format census. Keep these response files unchanged during the port unless a
non-date/non-ID difference is explicitly accepted under the Phase 3 gate.

The test is `graphql::postgres_golden_tests::postgres_golden_sqlite_baseline` in
`src/graphql/postgres_golden_tests.rs`. This binary crate's existing resolver tests
are sibling modules, so the test reuses `sources_tests`' schema, request/auth and
migrated temporary `Database` helpers. All fixture artifacts live here.

From `apps/server`:

```sh
cargo test postgres_golden_sqlite_baseline -- --nocapture
UPDATE_GOLDEN=1 cargo test postgres_golden_sqlite_baseline -- --nocapture
```

The first command only compares. The second is the **only recording mode** and
can now create only missing W3d `chapter_comments__<role>.json` and
`chapter_comments_linked__<role>.json` files for the four existing roles. It uses
exclusive file creation and compares every existing response without rewriting
it, including those eight files once recorded. The Phase 3 prohibition on
re-recording existing baselines remains in force. Review all resulting diffs.
Recording never changes `search_queries.json`'s independent human relevance
annotations. Expected search sets are deliberately not assertions
that SQLite already satisfies: PostgreSQL's future recall gate should use them.
The baseline test does assert today's ordered results, including its misses.

## Files and comparison

- `seed.sql`: fixed portable INSERT/UPDATE statements through `Database`, with
  boolean literals, explicit dates and identifiers. No checked-in database, FTS
  implementation SQL, rowid, SQLite functions, or SQLite conflict syntax.
- `seed_chapter_comments_linked.sql`: a W3d overlay applied after all baseline
  GraphQL and search reads, only for the four linked chapter-comment cases.
- `graphql_cases.json`: 192 named requests with roles and intentional-error flags.
- `graphql/*.json`: one full real `async_graphql::Response` per request, including
  data, error messages, locations, paths and extensions where present.
- `search_queries.json`: 42 exact/prefix/typo/transpose/accent/case/short/CJK/alias/
  broad/no-match queries and independent human expected work-ID sets.
- `search_observed.json`: ordered work IDs actually returned by the current public
  `search` resolver's FTS5 branch, as admin with `includeNsfw: true` so recall is
  independent of reader NSFW and content blocks.

Objects are pretty-printed with recursively sorted keys. Array order, floats,
NULLs, IDs and date strings are preserved. A failure names the file, the first
changed JSON path, expected/actual values and a contextual line diff. Queries
expected to succeed must succeed even in recording mode. Intentional errors are
limited to authorization/NSFW refusal, not failed SQL or schema validation.

## Fixture and deterministic construction

W3d's four `chapter_comments__*` cases query the external MangaDex id
`00000000-0000-4000-8000-000000000103` before the overlay: one existing comment,
one root, no linked copies. The four `chapter_comments_linked__*` cases read that
id and `710101` in one request, aliased as `mangadex` and `suwayomi`. The overlay
links these existing copies of work 1's chapter 10.5, adds a root on the numeric
copy and a reply there to the original comment, and adds votes on the new root.
Both aliases should return identical pages with three comments and two roots,
preserving each comment's own `targetId` and the reply's `parentId`. The numeric
root has one like and one dislike, with `myVote` 1 for `user`, -1 for `nsfw_user`,
and 0 for `anonymous`/`admin`. The work is never marked ready.

All three seed authors have existing `profile__*` / `myComments` snapshots, so
putting these extra comments in the common seed would change old outputs. The
overlay runs only after the old responses and search have been compared. Neither
`seed.sql` nor the unused historical `seed_sqlite.sql` needs to change.

To record and then verify W3d on a machine with an installed PostgreSQL 18 test
runtime, from `apps/server` (replace the binary directory if necessary):

```sh
KOMIKA_TEST_POSTGRES_BIN=/path/to/postgresql-18/bin UPDATE_GOLDEN=1 cargo test --offline postgres_golden_sqlite_baseline -- --nocapture
KOMIKA_TEST_POSTGRES_BIN=/path/to/postgresql-18/bin cargo test --offline postgres_golden_sqlite_baseline -- --nocapture
```

The explicit runtime path prevents the test harness from downloading PostgreSQL.
Review the eight new `graphql/chapter_comments*.json` files by hand. No existing
response should have a diff. The test retains its historical SQLite-baseline name
but now uses the PostgreSQL test harness.

15 works cover MANGA, MANHWA, MANHUA, WEBTOON and COMIC, including one with no
chapters and one NSFW work. Titles include Latin, accents, punctuation, 1/2/3-letter
ASCII, Japanese, Korean, Chinese, shared aliases and shared prefixes. Work 1 has
three sources: a MangaDex spine and two cached Suwayomi translators. It includes
zero, leading-zero, fractional, duplicate-number, oneshot, extra, undated and
external-link chapters, scanlator credits/AI tags, localized descriptions,
author/artist credits, primary/additional cover metadata and a chapter override.

There are three fixed UUID users, including one admin and one NSFW-enabled reader,
with libraries, favorites/shelves, canonical/source progress, reviews (including a
historical duplicate identity for one author), root/reply/chapter comments, votes,
notifications, activity and personal/site content-block rules. The admin does not
have its reader NSFW preference enabled: console browse's escape hatch and reader
detail's refusal are both recorded.

Date shapes are fixed examples of the current writers/storage: SQL
`YYYY-MM-DD HH:MM:SS`, zoneless ISO `YYYY-MM-DDTHH:MM:SS`, RFC 3339 `Z`, explicit UTC
and nonzero offsets, fractions of 3/6/9 digits, numeric milliseconds stored as text
on the Suwayomi cache, integer-millisecond ledger/feed/browse dates, and NULL dates.
These are shape examples, not a claim that all columns or production rows have
been inventoried. Auth uses fixed tokens and far-future fixed expiry dates.

Catalogue upserts, source/chapter cache writers and social mutations create UUIDs
or stamp `Utc::now()`, so rows are inserted directly. Alias normalization and
chapter spine labels reuse the server's pure functions. Integer-millisecond
**date** columns are populated through the server's existing ledger writer with
an explicit fixed clock (`1578618123456`), then the normal feed/browse/search
projection writers rebuild derived data. This avoids fixture SQL depending on
integer encodings when C3 makes these columns native timestamps. The sole
production-code change makes `catalog::ledger::seed_work` crate-visible for this
fixed-clock call; its body and runtime behavior are unchanged.

The usual database/schema helper will move to PostgreSQL along with the existing
server test harness in Phase 3. These fixture SQL statements and API queries contain
no SQLite-specific constructs. Backend-specific migrations, timestamp/UUID/boolean
binding in the shared harness, and projection/ledger encoding remain the server's
responsibility; this task does not try to pre-implement the PostgreSQL backend.

No mock HTTP server is started. MangaDex is disabled. Suwayomi uses an invalid
`offline://golden` URL, and fixed `2999-01-01T00:00:00Z` fetch markers keep metadata
and chapter lists on warm database paths. Successful responses are checked for
errors. Hourly view buckets are empty, so time-dependent Trending/MostViewed and
7d/24h views exercise their deterministic empty-window fallback, while all-time
views are populated. Active rolling-window ranking is intentionally not covered.
Projection builders use the current clock to exclude future releases, but all
release inputs are fixed in 2020 and no clock-generated value enters a response.

## Coverage and exclusions

Reader coverage includes home rails; all five legacy sorts; all ten v2 sorts in
both directions; every legacy/v2 filter field (including both tag modes); type
and NSFW rating tiers; empty pages; filtered text relevance; canonical detail,
chapter lists and the complete series-page bundle/aggregation; cached source
series/chapters; updates/canonical/merged feeds; database source mappings and batch
mappings; library/progress; session/profile activity/comments; reviews/comments/
votes; notifications; tag/genre facets; personal/site blocks and scanlator tags.
Roles are anonymous, normal reader, admin and NSFW-enabled reader where relevant.
Admin coverage includes the `search(includeNsfw: true)` catalogue/list path, raw
metadata and chapter editor detail, work review detail and users.

Successful `sources`, `workSourceRows`, `browseSources`, `scanlatorSites`,
`seriesSourcesBatch`, `sourceBrowse`, `searchAllSources`, `extensionRepos`,
`extensions`, `pages`, `canonicalPages` and `chapterOrigin` are omitted because
those paths consult the upstream engine, source coordinates or live page services.
`sources`' anonymous and normal-user authorization responses are recorded.
Database-backed `workSources`/`workSourcesBatch` cover source provenance fully.
`aggregatedChapters` as a standalone operation is omitted because the same
aggregation is selected in `seriesPage.aggregated` with every field.

`scanStatus`, `sourceRateLimits`, `sourceIngestJobs`, `scanlators`, `mergeQueue`,
`coverIssues`, `reports`, `instance`, `mangadexSetting`, `accessStatus` and
`accessPreflight` are outside this small reader/catalogue baseline: they involve
operational clocks/configuration/environment, unseeded job/moderation/dedup queues,
or deployment state. Their existing unit tests remain the coverage for those
surfaces. Reports, avatar/comment-image BLOB bytes, redirects, ban behavior,
non-empty pagination boundaries and permutations of all filter combinations are
not fixture scenarios.

**All mutations are omitted.** This baseline snapshots reads over fixed state;
auth/social/catalogue mutations often generate IDs/current dates, trigger jobs or
make live upstream calls. Existing mutation tests remain in place. No runtime
behavior, dependency, network allowance or production data is changed here.

## Observations in SQLite's recorded output

- Typos/transposed letters return nothing. `X`, `Go`, and ASCII `su` return nothing
  under the minimum FTS length gate. CJK one-character prefixes match. Accent and
  case variants match; duplicate aliases return both owning works.
- Dates coexist as verbatim space-separated, zoneless, Z and offset/fractional
  strings. Numeric Suwayomi upload-date strings are converted to fractional RFC 3339;
  ledger/feed millisecond dates also format with `+00:00` at the API edge.
- The ledger's current `strftime('%s') * 1000` writer truncates release fractions
  to whole seconds, even though chapter copy dates preserve the fractions. Its
  fixed supplied late-announcement clock retains `.456` in the API output.
- A work with no chapters has `latestChapterAt: ""`, not null. Oneshots/extras have
  numeric `number: 0` alongside their meaningful labels.
- Reviews collapse an author's canonical/source duplicate to the newer source-ID
  row; the returned review still carries that numeric source ID. Comments return
  a full reply thread but their total counts roots (three rows, two roots).
- Canonical chapter selection collapses `1`/`001.00` into one reader chapter;
  aggregation retains both copies. The merged updates feed has one row per work.
- WEBTOON and COMIC fixture values are emitted as MANHWA and MANGA on Browse
  today, matching the current type-folding behavior. The merged ledger feed can
  report an older release than canonicalUpdates when a newer copy of an already
  known chapter exists: the ledger keeps the first source's release.
- `libraryProgress` includes the reader's NSFW library entry even when `library`
  hides that card under the viewer's current NSFW preference. Personal content
  blocks also leave already-owned library cards visible in this fixture.
