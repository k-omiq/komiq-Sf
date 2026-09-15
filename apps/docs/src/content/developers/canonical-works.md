---
title: One entry per series
description: Understand how the server inside the admin app groups source series into canonical works and lets you review, merge and split them.
---

Komiq is self-hostable: the admin app runs the server and gives you its management portal, and the reader app connects to a server. The server groups series from different websites into a single entry called a canonical work, keeping duplicate cards out of library and browse screens. This page explains the work data model, matching steps and admin review, merge and split operations.

## Overview

A comic often appears across multiple sites with different titles, romanizations, and upload cadences. Instead of presenting each site listing as a separate manga, the server links every external listing to a canonical work in its catalogue.

```flow
Source series -> Deduplication matcher: evaluates external IDs and title
Matcher -> Candidate scorer: checks author, cover hash, and description
Candidate scorer -> Auto-merge: links source to existing canonical work
Candidate scorer -> Review queue: logs uncertain pair for admin review
Candidate scorer -> New work: mints a fresh canonical work record
```

The server's matching pipeline processes series during ingestion, source synchronization and catalogue maintenance. When evidence is decisive, the system attaches the source automatically. When confidence is uncertain, the pair enters a review queue for human inspection.

## The work data model

The canonical catalogue layer uses several related SQLite tables introduced in `apps/server/migrations/0005_canonical_works.sql` and extended by later migrations:

- `work`: the central record representing one logical comic series. It holds canonical metadata including title, author, artist, year, language, status, content rating, `is_nsfw`, and a 64-bit perceptual cover hash (`cover_phash`).
- `work_alias`: alternative titles populated from MangaDex and admin edits. It stores `normalized_title` (lowercased text with punctuation folded to spaces, diacritics folded, and trailing noise markers removed) and `match_key` (an article-insensitive key that removes "the", "a", and "an" except in the last position, added by `apps/server/migrations/0111_work_alias_match_key.sql`).
- `work_external_id`: external catalogue identifiers such as AniList (`al`), MyAnimeList (`mal`), MangaUpdates (`mu`), Kitsu (`kt`), AnimePlanet (`ap`), and MangaDex (`mangadex`). The primary key spans `(provider, external_id)` so identities remain globally unique.
- `source_series`: represents a concrete series on an upstream source, linked to `work.id` by foreign key. One canonical work can own multiple source mappings from different websites.
- `chapter`: mirrored chapter records linked to `source_series.id`. Because chapters belong to the source mapping rather than the work, re-pointing a source mapping carries all its chapters automatically.
- `work_field_pin`: records admin overrides for fields like author, artist, status, or year. Database triggers ensure upstream sync tasks cannot overwrite pinned values, saving incoming source data into `source_value` instead.

## The matching steps in order

When a new series is ingested, `dedup::resolve_ex` in `apps/server/src/dedup.rs` evaluates candidates against existing works using a five-step precision ladder:

1. **External ID match**: the matcher compares external tracker IDs. If the candidate shares an ID in `work_external_id`, the system immediately returns an `AutoMerge` decision with confidence 1.0. This stops further evaluation.
2. **Exact title match**: the system normalizes the title and queries `work_alias`. If no exact match exists, it checks `match_key` for article-insensitive matches ("The Breaker" matches "Breaker").
3. **Fuzzy title blocking**: if no title matches, the matcher selects the top 3 longest distinct words of at least 3 characters from the candidate's normalized title and aliases (`FUZZY_BLOCK_TOKENS`). It queries `work_alias_token` to retrieve up to 50 candidate works per token.
4. **Scoring and corroboration**: for each candidate, `score_candidate` calculates confidence between 0.0 and 1.0:
   - Title similarity accounts for 60% of the score using shingles, sets of word or character fragments.
   - The larger of description shingle overlap and cover hash similarity accounts for 40%:
     - Description similarity uses Jaccard overlap, the fraction of shared fragments in their union, without a 0.50 cutoff here.
     - Cover similarity compares 64-bit dHash values, hashes of adjacent-pixel brightness differences. The auto-merge gate separately requires similarity of at least 0.90 and a candidate hash with 12 to 52 set bits.
   - Author match adds 0.05 using Unicode edit distance, ignoring case, punctuation and word order and considering transliteration variants ("Dedart (디다트)" matches "Dedart").
   - Publication years differing by at most 1 add 0.03. The final score is capped at 1.0.
   - Novel guard: `novel_mismatch` rejects title candidates when the counts of "novel" differ, even if both titles contain the word.
5. **Decision thresholds**:
   - `AutoMerge`: awarded for exact titles with author corroboration, except ambiguous article-only matches with multiple author hits, or matches scoring at least 0.85 (`HIGH`) with discriminative cover hash corroboration.
   - `Review`: assigned if the score is at least 0.60 (`MID`). The pair enters `merge_candidate`.
   - `New`: assigned if the score is below 0.60. The candidate is registered as a new canonical work.

## Pair assessment rules

The duplicate sweep and review rechecker use a unified rule set in `apps/server/src/catalog/pair.rs` (`pair::assess`). It returns one of three verdicts: `Merge`, `Distinct`, or `Review`.

```flow
Candidate pair -> Novel check: compares novel markers
Candidate pair -> Relationship check: tests MangaDex anchors vs site copies
MangaDex pair -> Sequel, language and credit checks: merges on same title with proof
Site copy pair -> Title specificity check: merges on evidence or long title
Evaluator -> Verdict: returns Merge, Distinct, or Review
```

The assessment enforces specific boundaries:

- **Two MangaDex entries**: separate works by construction. They merge only on the same normalized primary title with matching author or artist credit, or cover evidence when the title is not short. They never merge across sequel markers like "Season 2" (`sequel_mismatch`) or language conflicts. Different primary titles route to `Review` when credit and cover or description corroborate them, and otherwise to `Distinct`.
- **Site copy and canonical work**: exact or primary-to-alternative title matches merge with matching author or artist credit, cover, description overlap >= 0.50, or a shared East Asian script title of at least 4 non-whitespace characters. The same normalized primary title also merges with years differing by at most 1. A specific article-insensitive title (at least 4 words, or 3 words and 16 characters) can merge on title alone. Language conflicts and titles under 5 characters without credit are checked first; shared alternative titles alone stay in review.
- **Edition and number guards**: site-copy pairs with mismatched edition words ("promo", "pre-serialization", "anthology") route to review. Number mismatches route to review in the fuzzy-title branch.

## The review queue

Pairs that cannot be decided automatically are stored in `merge_candidate` with `status = 'pending'`. Administrators inspect and resolve these pairs under [Duplicates](/admin/duplicates):

- **Merge**: confirms the merge, folding the candidate work into the target work and marking the row `confirmed`.
- **Keep separate**: marks the row `rejected`. This human decision prevents automated sweeps from re-proposing or auto-merging the pair.

The background worker in `apps/server/src/maintenance.rs` rechecks waiting reviews 30 seconds after server startup, hourly, and after catalogue ingestion. If updated metadata provides conclusive evidence, the pair merges automatically as `confirmed`. If updated data proves the works are distinct, the review closes as `dismissed`. A dismissal is a machine verdict, so it allows future merges if new evidence arrives later.

## Merge and split operations

### Merge operation

Merging two works executes with `BEGIN IMMEDIATE` in `apps/server/src/catalog/mod.rs` (`merge_works_checked`) to avoid SQLite write lock contention:

1. **Precondition check**: automated callers supply identity snapshots. The server verifies that the checked fields still match and aborts if they changed. Admin-requested merges omit this check.
2. **Blocked check**: when snapshots are supplied, the server confirms the pair was not rejected or recorded in `work_split`.
3. **Data folding**: alternative titles, external IDs, source mappings, chapters, library entries, and reading progress move to the surviving work.
4. **Redirects**: the server writes the retired ID into `work_redirect` pointing to the survivor, collapsing any existing chains into single-hop lookups.
5. **Deletion**: the losing work row is deleted.

### Split operation

When two series were merged by mistake, an administrator detaches source mappings using `split_source_series`:

1. **Source selection**: the administrator selects which `source_series` mappings to detach. The server refuses to detach all sources, as a work with zero sources vanishes from browse and search views.
2. **New work creation**: the server mints a new canonical work (`w_...`) using metadata from the detached source mapping. It avoids copying parent identity fields to prevent immediate re-merges.
3. **Moving chapters**: because chapters belong to `source_series_id`, updating `source_series.work_id` moves all chapters without touching individual chapter rows.
4. **Preserved records**: user reviews, library entries, reading progress, and view counts remain on the original work because they cannot be attributed to specific sources.
5. **Split memory**: the server inserts the pair into `work_split` (`work_a` and `work_b`, ordered lexicographically).

`work_split` entries do not use foreign keys. They permanently survive work deletions so background consolidators never re-merge works an administrator split apart.

## Where to look

- `apps/server/src/catalog/mod.rs`: database transactions for `merge_works_checked`, `split_source_series`, and alias indexes.
- `apps/server/src/catalog/pair.rs`: pairwise assessment rules (`pair::assess`), score calculations, and verdict decisions.
- `apps/server/src/catalog/normalize.rs`: title normalization, article stripping (`match_key`), edition checks, and novel mismatch guards.
- `apps/server/src/catalog/similarity.rs`: text shingling, author name distance, and cover perceptual hash comparison.
- `apps/server/src/dedup.rs`: five-step candidate resolution ladder (`dedup::resolve_ex`).
- `apps/server/src/maintenance.rs`: hourly background recheck of pending review candidates.
- `apps/server/migrations/0005_canonical_works.sql`: core schema for works, aliases, external IDs, source mappings, and merge candidates.
- `apps/server/migrations/0098_work_split.sql`: schema and constraints for the permanent split memory table.
- `apps/server/migrations/0111_work_alias_match_key.sql`: article-insensitive alias keys.
- `apps/server/migrations/0112_work_field_pin.sql`: admin field overrides and trigger logic.
