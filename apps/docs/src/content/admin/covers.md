---
title: Covers
description: Choose which source supplies each series cover, pick covers and titles by hand, free cover storage, and repair broken covers.
---

Your server stores one cover for each series and serves it from its own address. The Covers page in the admin app (`/bugs`) sets where those covers come from, can free the space of covers no series shows, and lists covers that failed.

This guide explains the cover ranking, the cover and title picker in the reader app, the optional cleanup of unpicked covers, and how to repair or replace broken images.

## How komiq processes covers

Your server keeps covers as files under `KOMIKA_COVERS_DIR`, indexed in PostgreSQL (`cover_object`), and serves them at `/covers/{work_id}.webp`. Every cover goes through the same steps:

```flow
Source image -> size check: at most 24 MiB, complete bytes
Decode -> downscale the longest edge to 512 px or less, never up
Encode -> lossy WebP with a 150 KiB target
Save -> file plus its PostgreSQL index row
```

The crawler records fetch, processing, and storage failures in `work_cover_issue` and lists the title under Cover issues. Other paths, such as lazy cover loading, do not record every failure there.

## Which cover a series shows

Three rules decide the cover, strongest first:

1. **A cover an admin chose.** An uploaded file, or a cover picked in the reader app. It stays until an admin changes it.
2. **The cover ranking.** The cover comes from the highest-ranked source that supplies one.
3. **The first source that answers.** A series with no cover yet gets one from the first source that sends a usable image, so it is never left empty.

Source bars still apply first: a source barred for covers under [Sources](/admin/sources), Metadata, never supplies one.

### Cover ranking

The **Cover ranking** section lists MangaDex and every installed source. Drag a row by its handle, or use the up and down arrows, then click **Save ranking**. The save ranks every row down to the last one you moved. The rows below it show **default order**: they stay unranked and keep the default order after the ranked ones, MangaDex first, then the source linked to the series first.

After a save, your server moves covers to their top-ranked source in the background. It checks again once a day, so new series and new sources follow the ranking. It uses a source cover it already stores when it can, and downloads only when it must. **Reset to default** clears the ranking. Covers stay as they are until something else changes them.

### Choose a cover and title in the reader app

On a series page in the reader app, an admin sees a **Cover & title** button. It opens a picker:

- **Cover**: every cover the series' allowed sources supply. MangaDex can offer one cover per volume. Pick one to make it the series cover. Your server downloads it and keeps a copy in PostgreSQL, as it does for an upload. **Automatic** drops your choice, or an uploaded cover, and lets the cover ranking choose again. If no source sends a cover just then, the current cover stays and the picker says so.
- **Title**: what each source calls the series, and its alternative titles. Pick one to show it to readers. **Source title** goes back to the title the sources give.

Cards in lists, search and the Updates feed show the change at once. The title choice is the same title override as in the [series editor](/admin/catalog).

### Delete unpicked covers

Your server also stores the cover of each linked source, for that source's own pages. **Delete unpicked covers** removes the stored source covers that no series shows. It is off by default.

Before you turn it on, the section shows how many covers it would remove and how much space that frees, per source. It never removes:

- MangaDex covers;
- a cover any series shows;
- the cover of the source a series' cover came from;
- a cover an admin picked;
- the only cover of a series that has no stored cover of its own.

The removal runs in the background and resumes after a restart. Turn the switch off to stop it at the next cover. While it is on, your server also stops downloading these covers ahead of time. A removed cover downloads again if a reader opens it from that source. The files themselves go with the hourly cleanup, once nothing names them.

## Cover issues

The Cover issues section lists works with recorded cover issues.

Each row shows the cover, the series, the reason, the number of tries, the last attempt, and the **Retry** and **Upload** buttons. Reasons have readable labels: **Image too large** (`too_large`), **Unsupported or damaged file** (`unsupported`), **Image is empty** (`empty`), **Could not convert** (`encode`) and **Could not save** (`store`). Other reasons, such as `truncated` and `fetch`, appear as raw codes.

## How to repair a cover

### Retry the download

Use **Retry** after a temporary network failure, timeout or rate limit. Your server tries the linked sources in cover ranking order, MangaDex only when it is enabled, until one supplies a processable image. On success it saves the cover and its new version in one database transaction, and the row leaves the list. A deterministic processing failure updates the attempt count and the error. If no image can be fetched, the issue stays as it was.

### Upload a replacement image

Use **Upload** when a source is gone, blocked, or sends poor artwork. Pick a JPEG, PNG or WebP file. Your server processes it like any cover, stores the file, and keeps the processed image in PostgreSQL's `cover_upload` table for backups. The row leaves the list at once. The upload is an admin cover: the cover ranking and background downloads never replace it.

Cover files are not backed up. Missing files refill from their sources, and admin covers return from PostgreSQL. Covers uploaded before cover files existed copy from the old cache, but are not in database backups. Read [Update and back up](/self-host/update-and-backup).

## Bulk cover storage

To generate and store covers for eligible uncached works, use the maintenance tools in [Settings](/admin/settings).

Navigate to **Settings** > **Maintenance** and find **Store covers on this server**. Running this job crawls eligible uncached works. Recorded issues whose retry window has not expired are skipped, and MangaDex covers require MangaDex to be enabled.
