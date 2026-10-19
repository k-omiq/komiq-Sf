---
title: 'Duplicates: merge and split'
description: Review duplicates on your server, merge identical series, and split incorrectly combined titles.
---

When multiple sources provide the same comic, your server uses an automated matcher to combine them under one canonical work. When the matcher is uncertain, it places candidate pairs into the Duplicates review queue for human verification.

This guide explains why duplicates occur, how to review and resolve candidates, how merges behave, and how to split erroneously combined works.

## Why duplicates happen

Your server can aggregate content across MangaDex and multiple external sources through Suwayomi, the source engine.

Different sources often title the same series differently. Common discrepancies include:

- Romanized titles compared with translated English titles.
- Differing transliterations of Japanese, Korean, or Chinese names.
- Subtitles, volume numbers, or release edition tags in the title.
- Variations in author or artist name spelling.

When a source series is added to the catalog, the matcher compares titles, alternative names, and author metadata against existing catalog works.

```flow
New source series added -> matcher evaluates title and author
Confidence above auto threshold -> automatically merged into existing work
No plausible match found -> created as new canonical work
Match uncertain -> candidate pair sent to Duplicates review queue
```

Candidates with high confidence merge automatically. Unmatched titles become new works. Intermediate matches that require human judgment are placed in the Duplicates queue.

### What is never merged

Some pairs look alike but are different works. Your server never merges them and never lists them in the Duplicates queue:

- **Two MangaDex entries.** MangaDex lists different versions of one series as separate entries, often under the same title and author. For example, a mature version and an erotic version, or a colored edition. A manual merge of two MangaDex entries is refused too.
- **An edition and the series.** If one title names an edition and the other title does not, the pair stays separate. Edition words include colored (fan colored, official colored, full color), long strip, webtoon, promo, pre-serialization, one-shot, side story, and anthology.

## The Duplicates review queue

The Duplicates page (`/review`) lists candidate pairs awaiting administrative review, ordered by confidence score.

### Review queue table

The queue table displays the following information for each pair:

- **Source series**: Cover thumbnail, title, and work identifier of the newly detected series.
- **Candidate work**: Cover thumbnail, title, and work identifier of the potential match already in the catalog.
- **Confidence**: Match confidence score as a percentage (such as 75% or 90%).
- **Matched by**: Matching method used by the engine (such as `Title corroborated`).
- **Queued**: Date when the pair entered the review queue.
- **Decision**: Action buttons for resolving the candidate pair.

### Inspecting details

Clicking the cover or title of either work opens an inspection modal.

This dialog displays:

- Cover preview, work identifier, and chapter count.
- Badges for status, publication year, content rating, original language, and NSFW classification.
- Author and artist credits.
- Synopsis description.
- Alternative titles list.
- All currently linked source extensions and languages.
- An **Open in the catalogue editor** link if the work has a MangaDex source mapping.

### Resolving candidate pairs

After comparing the two works, choose one of two actions:

- `Merge`: Confirms that both entries represent the same series. A confirmation dialog outlines the changes before you proceed. Merging folds the source series into the candidate work and removes the source work.
- `Keep separate`: Rejects the match. Both works remain distinct in the catalog, and the pair is removed from the queue.

> [!NOTE]
> Resolving rows causes subsequent items to shift forward in the list. After a decision, clicking Next reloads the current page so no candidates are skipped. An emptied page reloads automatically if candidates remain.

### Bulk duplicate merge

For large libraries with substantial backlogs, you can run bulk deduplication in [Settings](/admin/settings).

Navigate to **Settings** > **Maintenance** and locate **Merge duplicate works**. This maintenance job reviews works sharing titles (ignoring common articles like "the", "a", and "an"). It automatically merges high-evidence matches, closes rejected pairs, and leaves ambiguous candidates in the review queue.

## Merging works

A merge folds one canonical work into another.

### Where merges happen

Merges can occur in four places:

1. **Duplicates queue**: Accepting a candidate pair on `/review`.
2. **Catalog page**: Clicking `Merge…` on a series row, clicking `Keep this one` on the target row, and confirming the dialog.
3. **Alternative titles**: Adding an exact normalized alternative title match in `/series/[id]` with at least 90% author similarity may merge works, subject to additional identity checks and prior split decisions.
4. **Maintenance job**: Running the bulk merge duplicates job in Settings.

### How a merge behaves

Merging is destructive and irreversible:

- The source work record is permanently deleted.
- All source mappings (`source_series`) re-point to the target work.
- Chapters move with their source mappings. Because chapters are associated with source series rather than works, chapter runs are preserved without data loss.
- Library entries, reading progress, reviews (including ratings), and series comments move to the target work. If the same user reviewed both works, the target's review is kept. Library entries combine favorites and retain the target's explicit shelf, falling back to the source shelf.

## Splitting works

Splitting is the administrative counterpart to merging. It detaches source mappings from a work and places them onto a newly created canonical work.

### When to split

Splitting is used when two different series were erroneously combined. This typically happens if two unrelated comics share the exact same title.

> [!WARNING]
> Splitting is not an undo function. Merging permanently deletes the losing work record. Splitting creates a new canonical work rather than restoring past deleted records.

### How to split a work

Splitting is performed directly in the reader application:

1. Connect the reader app to your server, sign in with an administrator account on that server, and open the series page for the affected title.
2. Click **Split** to open the source splitter dialog.
3. Select the checkboxes for the source mappings you want to detach.
4. Click **Review split**. Before you confirm, the dialog shows how many readers' reading progress moves with the selected sources.
5. Click **Split off N**, where N is the number of selected sources.

```flow
Administrator opens series page -> clicks split button
Source splitter dialog opens -> administrator selects sources to detach
Administrator confirms split -> new canonical work created with detached sources
```

### Split rules and behavior

- **Cannot detach all sources**: You must leave at least one source on the original work. A work without source mappings is excluded from browse listings and search indexes.
- **New work identity**: Your server creates a new canonical work with a fresh identifier. The new work takes its primary title from the first detached source.
- **Chapters**: Chapters belong to source series records. All chapters belonging to the detached sources transfer to the new work cleanly.
- **Reading progress**: Readers' reading progress on the detached sources moves with them to the new series. The exception is a chapter that also exists on a source that stays: it keeps its progress on the original series.
- **Other user data**: Reviews, series comments, library entries and view counts stay on the original series. Chapter records stay attached to their sources.
- **MangaDex sources**: If you split off a MangaDex source, both series fetch their metadata and covers from MangaDex again in the background. Each series then keeps only its own MangaDex entry's description, credits, covers, and alternative titles. This needs MangaDex to be on.
