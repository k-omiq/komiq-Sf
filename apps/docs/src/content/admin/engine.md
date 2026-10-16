---
title: Source engine state
description: Read the source engine state on the Health page and pick a way out of quarantine.
---

Your server keeps its catalogue in PostgreSQL and fetches extension sources through the Suwayomi engine. The server checks that the engine it talks to is the same engine its data was built from. This page explains the engine state on the Health page and what to do when the server quarantines the engine data.

## Where to find it

Open **Sources**, then **Health**. The **Source engine** section shows the current state, the reason and the time it started. When the engine data is quarantined, the section moves to the top of the page, and every other console page shows a warning with a **Review engine state** link.

The section shows one of these states:

- **Active**: the engine matches the server's data. Nothing to do.
- **Quarantined**: the engine does not match. Chapters, pages and thumbnails from extension sources answer as unavailable until you pick a way out.
- **No source engine has been recorded yet**: the server has not adopted an engine. This is normal before the engine starts for the first time.
- **This server does not report the engine state**: the server is older than this check.

Under the current state, a short list shows earlier generations that were set aside, with their reason and time. A generation is one engine and the numbers it gave to series and chapters.

Some older engines cannot store the server's marker. The section then says that the server checks only stored source identities. The server still works.

## Why the server quarantines

Each engine gives series and chapters its own numbers. A different engine can give the same number to a different chapter. The server stores a marker in the engine and a sample of source identities. At start, on each scan and after a lost engine connection, it checks both.

The server quarantines when:

- stored numbers answer different source identities;
- the engine is empty but the server still has series from it;
- none of the stored source identities can be found in the engine, even if its marker matches;
- the engine marker is missing or different.

This happens after you restore only the database onto a fresh engine, rebuild the engine, or restore the engine from its own backup file. Quarantine keeps progress and comments from landing on the wrong chapter. MangaDex series are not affected.

## Way out 1: re-attach the old engine data

Pick this when you still have the engine's data from before the change.

1. Stop the server.
2. Put back the copy of the engine's data, for example the `engine-data` volume archive from [Update and back up](/self-host/update-and-backup).
3. Start the server.

The server checks again and leaves quarantine by itself. Nothing is lost.

If only the marker was lost and the engine data is the same, run this in the server container:

```sh
komika-server engine reattach
```

It checks every stored source identity. It refuses when any of them differ.

## Way out 2: start a new generation

Pick this when you want to keep the engine you have now, for example a fresh engine after a lost disk.

1. Choose **Start a new generation**.
2. Read the dialog. Confirm with **Start a new generation**.

The button works only while the engine data is quarantined. The console sends the generation it showed you. If the state changed after you opened the page, for example the old engine came back and quarantine ended, the server refuses. Refresh the engine state and check again.

The dialog lists what changes:

- Reading progress on extension-source chapters is set aside. The server keeps it, but readers no longer see it.
- Comment threads on extension-source series and chapters are set aside with their replies.
- Extension-source series and chapters leave the catalogue. Scan or add them again from the new engine. The server puts them back on their works.
- Works, libraries and MangaDex progress stay.
- Library entries, reviews and view counts on extension-source series move to their works. Entries for a series that has no work are set aside, like progress. Per-series scan settings for extension-source series are set aside: set them again on the new series.

Reader apps that saved progress or library changes offline before the change cannot send them. The app drops that progress. It never puts it on a different chapter. A reader app that stays open across the change shows a notice. Reload the app to save progress again.

Library membership, shelves and favourites on extension-source series use the same check as progress. Apps too old to send the generation epoch cannot change them for 30 days after a new generation. Browse and search remove entries that still point to the old engine's series numbers until those series are added again.

You cannot undo this from the console. Take a backup first. The same action is available as a command in the server container. Give it the generation id of the quarantined generation:

```sh
komika-server engine start-new-generation <generation-id>
```

The server log line `engine quarantined` shows the id as `generation=`. The command refuses unless that generation is current and quarantined. To replace the current generation even while it is active, run `komika-server engine start-new-generation --force`. Use `--force` only when you are sure the engine data does not match.

## Which one to pick

- You have the old engine data: re-attach it.
- The old engine data is gone, or you chose a fresh engine on purpose: start a new generation.

If you are not sure, keep the server quarantined. Readers can still read MangaDex series while you decide.

Next: [Update and back up](/self-host/update-and-backup).
