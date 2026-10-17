---
title: Catalog and series editor
description: Manage your server’s catalog, adjust scan schedules, edit metadata overrides, and view latest updates.
---

The Catalog section in the admin app lets you inspect your server’s catalog, adjust how often sources are scanned for new chapters, override series metadata, and follow releases.

This guide covers the Catalog table, scan settings drawer, series editor, and Latest updates feed.

## The Catalog page

The Catalog page (`/`) lists browsable works in your server's catalog.

### Search modes and IDs

The search bar operates in two modes:

- **Empty query**: Lists the browsable catalog. The page reports an exact total.
- **Title search**: Queries the canonical full-text search index. Both modes use `w_` IDs for MangaDex works and numeric IDs for Suwayomi-only works, with server-side pagination.

> [!NOTE]
> Series flagged as NSFW always appear in the Catalog table, even when your account has NSFW disabled. This lets you find misclassified content.

### Catalog table columns

Each row shows metadata and status:

- **Series**: Cover thumbnail, title, identifier, and author. Clicking the title opens the editor at `/series/[id]`.
- **Type**: Comic format (Manga, Manhwa, Manhua, Comic, Webtoon).
- **Status**: Release badges: Ongoing, Completed, Hiatus, Cancelled, or Unknown.
- **Chapters**: Available chapter count.
- **Source**: Upstream provenance. Suwayomi rows display extension name, language, and source identifier. Canonical rows show `Catalogue work`.
- **Scan**: Scanning state (`Scanning` or `Paused`), cadence, and overdue check frequency.
- **Actions**: Contextual action buttons based on the row type.

### Row actions

Actions depend on the row type:

- `Edit`: Opens the scan settings drawer for source series.
- `Pause` or `Unpause`: Toggles scheduled chapter scanning. Resuming scanning triggers an immediate re-scan on your server.
- `Add`: Appears for source series not yet linked to the canonical catalog. Submitting it runs the deduplication matcher:
  - `Added`: Added as a new canonical work.
  - `Merged`: Matched an existing canonical work and merged into it.
  - `In review`: Sent to the [Duplicates](/admin/duplicates) review queue for human confirmation.
  - `Already added`: The series was previously linked to a canonical work.
- `Merge…`: Starts a manual work merge. Select one work, click `Keep this one` on the target work, and confirm to fold them together.
- `Edit metadata`: For canonical works (`w_`), opens the series detail editor.

## Scan settings drawer

Click `Edit` on a source series to configure scan schedules and status behavior.

### Status override

By default, the series adopts the status reported by the source. Selecting an explicit status (Ongoing, Completed, Hiatus, Cancelled, or Unknown) overrides the source value.

### Scanner mode

The scanner control has three settings:

- **Auto**: Stops scanning if the series is completed, on hiatus, or cancelled.
- **Keep scanning**: Continues scanning on schedule regardless of series status.
- **Paused**: Halts all scheduled checks until unpaused.

### Timing controls

- **Hours between scans**: Sets an interval override in hours. Leaving this blank lets the scheduler choose a cadence based on format and release recency.
- **Minutes between checks when a chapter is overdue**: Stores an override, defaulting to 30 minutes. The format-based scheduler does not use overdue acceleration.
- **Scan now**: Checks the source immediately for new chapters, even if the series is paused.

Click **Save settings** to persist your changes.

## Series detail editor

Opening a series from the catalog brings you to `/series/[id]`. This page lets you view upstream metadata, pin custom values, manage source rescans, and hide or rename chapters.

> [!NOTE]
> Add uncatalogued series from the Catalog table before editing metadata. The editor opens `w_` works only with a MangaDex source. Details follow your account's NSFW filter.

### Details and external trackers

The page shows the cover, format, status, author, series identifier, work identifier, and chapter count.

Under **Details**, view current read-only values, including metadata overrides:

- Publication year, original language, demographic target, and content rating.
- Taxonomy tags grouped into Genres, Themes, Format, Content, and Curated tags.
- Links to external trackers, including MangaDex, checked against an allowed HTTPS host list.

### Metadata overrides and pinned fields

The **Metadata** section lets you replace source values with your own.

An override replaces what the source sends. Leaving a field blank, or choosing `Auto`, tells your server to use the source value.

- **Title override**: Replaces the title shown to readers.
- **Description override**: Replaces the synopsis in every language.
- **Type**: Selects a comic format or remains on `Auto`.
- **NSFW**: Forces `SFW`, forces `NSFW`, or follows `Auto`.
- **Pinned fields** (Author, Artist, Publication year, Original language, Status, Demographic, Content rating):
  Saving a changed value in these fields pins that value in the database. Pinned fields will not be overwritten during future source syncs. Clearing a field or choosing `Auto` and saving removes the pin, restoring source synchronization.
- **Tags**: Add or remove tags. Editing tags creates a curated set that replaces source tags. Clicking **Reset to source** restores source tags.
- **Alternative titles**: Alternative names save immediately. Exact normalized title matches with at least 90% author similarity may merge, subject to identity checks and prior split decisions.

Click **Save metadata** to commit your changes.

### Field sources

Muted lines under Title, Description, Author, Artist, Tags and Status show the selected field source. `From` names that source. `(guessed)` means the link came from stored data rather than a live observation.

`Kept: every source is barred` means the current value stays because no linked source is allowed. `Kept: source not read yet` means the chosen source has no observation for that group. These lines describe source values beneath your overrides and pins. Older servers show no lines.

Change these permissions in [Sources](/admin/sources), under Metadata.

### Sources and rescanning

The **Sources** section lists visible external sources attached to the work, subject to your account's NSFW filter:

- Each source displays its extension name, language, and unique source key.
- `Scan now`: Checks an individual Suwayomi source for new chapters immediately.
- `Re-scan all sources`: Triggers an immediate chapter scan across linked Suwayomi sources, skipping sources that cannot be resolved.

### Chapter management

The **Chapters** list shows one chapter of the work's own list per row, called an entry. Copies from sources link to that entry. The list includes hidden entries and entries with no copies:

- **Rename**: Assigns a custom title override to the chapter. Saving an empty title restores the source title.
- **Hide** / **Unhide**: Soft-hides a chapter from readers without deleting it. You can unhide it at any time.
- **Special on** / **Special off**: Marks or unmarks an entry as special. Finishing a later chapter does not mark a special read.
- **Move up** / **Move down**: Changes the entry's position in the chapter list.
- **Copies**: Click the copy count to see the linked copies and their source details.
- **Move**: Under a copy, choose another entry in the same work from **Move to…**, or choose **A new entry of its own**. Click **Move** and confirm.
- **Merge**: Choose the entry to keep from **Merge into…**, click **Merge** and confirm. The dropped entry's copies move to the kept entry. A new copy with the dropped entry's chapter number joins the kept entry too, as long as one of the moved copies is still there.

Readers keep what they have read when you move copies or merge entries.

> [!NOTE]
> If the chapter list is still being built, the editor shows a note. Readers see the old list until it is ready.

On an older server, you see the old table grouped by chapter number. It offers only rename and hide controls.

## Latest updates

The Latest updates page (`/updates`) provides a chronological feed of recent chapter releases from MangaDex.

- Displays the series title (linking to `/series/[id]`), NSFW indicator, MangaDex identifier, latest chapter number, chapter title, and release timestamp.
- The feed lists 20 updates per page.
- NSFW titles always appear in this view regardless of your account filter.

```flow
Your server synchronizes with MangaDex -> detects new chapter releases
Chapters stored in database -> feed lists newest releases first
Administrator clicks title -> opens series editor at /series/[id]
```

> [!WARNING]
> New MangaDex releases require your server to synchronize with MangaDex enabled. On your server with `CATALOGUE_SYNC` unset, the crawl follows the console's MangaDex switch. `CATALOGUE_SYNC=off` prevents the crawl. Existing stored updates can still appear while synchronization is off.
