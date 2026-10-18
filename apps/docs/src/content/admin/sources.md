---
title: Extensions and source health
description: Manage your server’s extensions, browse sources for new titles, choose field sources, and monitor source health.
---

The Sources section in the admin app connects your server to comic sources. It contains six pages: Extensions, Browse & add, Search all, Health, Metadata, and Scanlators. Learn each page’s controls.

## Why NSFW settings gate source tools

When your admin account has NSFW (adult) content turned off, your server blocks browsing and ingestion of sources flagged NSFW, and installation or updates of extensions flagged NSFW. A warning notice appears at the top of the Sources section, linking to the Content section in [Settings](/admin/settings).

External sources and extensions may host adult content alongside general titles. To prevent unwanted exposure, these operations check the source or extension's NSFW flag against your account preference.

A series you add from a source is marked adult when its own genres are adult or when its source is flagged NSFW. The server learns the source flag when you install the extension. If it does not know the source yet, it asks the source engine before it adds the series. If it cannot get an answer, it refuses the add. It does not store the series as safe. Adult series stay hidden from signed-out readers and from readers who have adult content turned off.

Catalog searches and updates in the admin console still display NSFW records to an administrator. This allows you to inspect and correct records that were flagged incorrectly. Source browsing and extension installation instead check source or extension flags and your account preference.

## Extensions

The Extensions page lists every extension known to your server’s Suwayomi engine. Suwayomi is the source engine, and extensions connect it to external sources. When no extension repository has been added, the page displays a link to add one in [Settings](/admin/settings).

### Header and filter controls

- `Refresh`: Fetches the latest repository indexes. This updates package availability, versions, and update badges across the table.
- `Show`: Segmented control filtering extensions by status: `All`, `Installed`, or `Available`. Each option displays its count.
- `Language`: Dropdown menu filtering extensions by language code or all languages.
- `Name`: Search field filtering by extension name or package identifier.
- `Clear filters`: Resets active filters when no extensions match.

### Extension list items

Each row shows an extension and its actions:

- Details: Shows the icon, name, package identifier, an `Installed` badge, and an `NSFW` badge if the extension contains adult content.
- Language and version: Shows the language code and version string. An `Update available` badge appears when a newer release exists in the repository.
- Background updates: A switch on installed extensions. Subscribing instructs your server to periodically scan the extension's sources for new series and chapters. Enabling it also triggers an immediate update pass.
- Actions:
  - `Install`: Downloads and installs an extension from the repository.
  - `Browse`: Opens Browse & add scoped to this extension's sources.
  - `Update`: Upgrades an installed extension to the latest version.
  - `Uninstall`: Prompts for confirmation and removes the extension and its sources from your server.

## Browse & add

Browse & add lets you explore sources, preview chapters, and add titles to your server’s catalogue.

### Scoping and source selection

Navigating to this page from an extension scopes the view to that extension's sources. The header shows the source count and an `Add all from this extension` button to start jobs for its installed English sources. Sources flagged NSFW are skipped if your account has NSFW content off. Click `Show all sources` to clear the scope.

Controls include:

- `Source`: Selects an installed source and language variant.
- `Listing`: Switch between `Popular`, `Latest`, and `Search` modes. `Search` opens an input to query the source by title.

### Background ingestion

- `Add all from this source`: Walks the source's Popular and Latest listings in the background, up to 1,000 pages per listing. Only English sources are accepted. A confirmation modal warns that this can add thousands of titles.
- Ingest panel: Displays active and completed jobs, polling progress every 2 seconds with automatic backoff. It reports pages read, series found, succeeded, added as new, merged, sent to [Duplicates](/admin/duplicates), already in catalogue, and failed.
- You can stop a running job with `Stop` or `Stop all`. Titles added before stopping remain in your catalogue. Click `Dismiss` to hide finished jobs.

### Browsing and previewing titles

The grid shows each title returned by the source:

- Each card shows a cover, title, and `In catalogue` badge if already in the Suwayomi library.
- Clicking a card opens the reader preview. Chapter lists use your server's database cache and fetch through Suwayomi when needed. Previewing can fill that cache, but does not add the title to the catalogue or alter reading history.
- The selection button (`+` or checkmark) selects up to 100 titles.

### Adding selected series

Selecting titles displays a sticky bottom bar showing the count, a `Clear` button, and `Add selected to catalogue`. Submitting sends the batch to the deduplication pipeline.

When complete, `Result of the last add` shows the outcome for each series: merged into an existing work, added as a new work, sent to [Duplicates](/admin/duplicates) for review, already present, or failed.

## Search all

Search all looks for one title on every installed source at once. The reader app does not have this search. It only searches the catalogue.

- Enter a title and select `Search`. Each result is one catalogue work, with the sources that have it.
- Searching writes to the catalogue. Results that match a catalogue work are linked to it. The top results that are not in the catalogue yet are added as new works.
- Sources flagged NSFW are skipped if your account has NSFW content off.
- Each account can run about 20 searches a minute. If you search too often, the page shows when you can search again.
- From the Catalog page, a search shows a `Search all sources for it` link that opens this page with the same title.

## Metadata

Open Metadata to choose which sources may supply series details. Each source has six switches: Title, Description, Authors, Tags, Cover and Status. Authors covers both authors and artists. The built-in catalogue source appears first, followed by installed sources.

A switch on means allowed. Turn it off to bar that source from the field group. Each change saves at once. Controls stay disabled while the save runs. The result says `Saved. Re-checking N works`.

The server then checks that field group again on every work linked to the source. It does this in the background, so a large source can take several minutes. Series pages change as each work is checked. If the server stops before it finishes, it continues after it starts again.

A failed save changes nothing. The page reloads the saved switches from the server. You can safely retry the same change, and saving a switch again starts its check again.

For title, description, authors and status, the server chooses the first allowed source in its ranking. If every source is barred, it keeps the current value. It also keeps the value when the chosen source has not been read yet. Catalogue tags come from the first allowed catalogue listing that has reported them. An empty report clears those tags. Your title and description overrides and pinned values stay in place.

Turning off Cover for a source removes the covers that source supplied. The server then fetches a cover from the next allowed source. Covers you uploaded stay. So do covers stored before this setting existed, because the server does not know their source. Readers may keep an old cover image in their browser cache for a while.

If any linked source is barred from Authors, automatic matching uses the stored author or artist only when an allowed source also supplied it or you pinned it. When filling in older metadata records, the server skips field groups that a source is barred from.

The [series editor](/admin/catalog) shows the source selected for each field group. Older servers show `This server does not support metadata bars` here and have no switches.

## Health

The Health page monitors your server’s scan reliability and request failures across sources with tracked series or recorded requests, listing the worst sources first.

### Engines

The **Engines** section shows the main engine (`owner`) and configured regional engines. A regional engine fetches pages and covers for routed sources. The main engine keeps the catalogue, scanning and chapter lists.

For each regional engine, check its reachability, last health check and error. Its counters show engine database lookups, source requests, page fetches and bytes, cover fetches, failures and fallbacks to the main engine. These counters reset when the komiq server restarts. Database lookups do not contact the source.

Routed sources show the main and regional extension versions, whether they match and whether the route is active. Matching requires the same extension package and a known, equal version. An inactive route uses the main engine, unless strict mode is on, which returns HTTP 503 instead.

Open **Sources › Browse**, choose a source, then use its **Engine** picker. For routing rules, see [Source engines](/admin/settings#source-engines). For tunnel setup, authentication and provisioning, read [Add a regional engine](/self-host/regional-engine).

### Outage notices

When sources experience an outage, a notice appears at the top. It reports the number of down sources and how many exclusive works exist only on them. Exclusive works cannot be read while their source is down.

### Scan health metrics

The table shows each source’s metrics:

- `Source`: Source name, falling back to its package identifier or source ID when the name is unavailable.
- `Status`: Overall availability state and status badges (`OK`, `Failing`, `Down`, `Not installed`, `Updates paused`, `Updates off`).
- `Series`: Total series tracked from this source.
- `Failing`: Series whose last scan failed, with a confirmed failure count after repeated consecutive failures.
- `Stale data`: Series where the remote site failed to answer and Suwayomi fell back to cached database data.
- `Errors`: Series where requests failed outright.
- `No chapters`: Series whose tracked chapter count is zero or unknown. If this is most of a source's series, the source has probably never worked.
- `Exclusive works`: Works found only on this source.
- `Last check` and `Last failure`: UTC timestamps for the latest scan and error.

### Request failure breakdown

Troubled sources show diagnostic details below their rows:

- Outage details: How long the source has been down, the failure reason, and when checks will resume.
- 24-hour request health: Summarizes failures across chapter opens (`pages`), page images (`image`), chapter lists (`chapters`), and listings (`browse`).
- Failure reasons: Displays failure counts for codes such as `Blocked` (HTTP 403 or Cloudflare challenge), `Rate limited` (HTTP 429), `Solver down` (FlareSolverr failure), `Not found` (HTTP 404), `Timeout`, `Unreachable` (DNS or TLS errors), `Site error` (HTTP 5xx), `Extension broken`, and `Engine`. Each request category also shows a timestamp and any stored message for its most frequent failure reason.

## Scanlators

The Scanlators page lists who translates the chapters on your server. Use it to tell readers which scanlators translate with AI. The `List` control at the top switches between two lists:

- `Groups`: The scanlation groups that chapters credit, with the most series first. MangaDex chapters name their group.
- `Sites`: The sources that translate their own series. Their chapters credit no group, so you tag the whole source.

Both lists share these controls:

- `Name`: Search field for part of a name.
- `Tag`: Segmented control showing `All`, `Tagged`, `AI`, `Partial AI`, or `Not tagged` rows.
- `AI tag`: Sets each row to `None`, `Partial AI`, or `AI`. Use `AI` for machine translation and `Partial AI` when AI is part of the work, such as a machine translation that a person edits.

### Groups

The `Source` dropdown menu shows only groups with series on one source. The series count then covers that source only. A group's count includes the series of its joint releases.

A tag is kept by group name. It applies to every chapter credited to that group, on every source, including chapters added later. A joint credit such as "A & B" takes the strongest tag of its members unless you give it its own tag. The row then shows where its tag comes from.

Your server keeps the group list up to date as chapters arrive, so a new group appears at once.

### Sites

The list shows every source with a series in your catalogue, except MangaDex, with the most series first. Each row shows the source name, its language and its series count.

A tag on a site applies to every chapter read from that source, including chapters added later. A chapter that also credits a tagged group takes the stronger of the two tags. A tagged source stays in the list when its last series is gone, so you can remove the tag.

Tags save at once in both lists. Readers see an AI or Partial AI badge next to the group or source name in the chapter list and in the reader menu.
