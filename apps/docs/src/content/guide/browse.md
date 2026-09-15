---
title: Browse and search
description: Explore the connected server’s catalogue, search its sources, filter titles, and sort results.
---

Use the reader app’s home screen and browse page to explore manga, manhwa, and manhua on the server it is connected to. This guide covers the home screen, catalogue and external source searches, filters, and sorting.

## Home screen

The home screen at `/` presents featured series and recent updates to that server’s catalogue.

### Featured series carousel

The home page’s spotlight banner shows featured titles. The banner displays:

- Full-bleed cover artwork.
- A flag badge identifying comic format (manga, manhwa, or manhua).
- The series title and up to four genre tags.
- A synopsis excerpt.
- The author credit and chapter counts.

The carousel rotates to the next slide every 10 seconds. Rotation pauses when you hover over navigation controls, focus with the keyboard, or enable reduced motion in your operating system. You can step through slides using the chevron arrow buttons.

### Content feeds

Below the featured banner, the home page arranges series into scrolling rows:

- **Latest Updates**: series with recently released chapters. A "View all" link leads to `/updates`.
- **Trending**: popular titles based on reader views.
- **Latest Added**: series newly indexed in the catalogue. A "View all" link opens `/browse?sort=added`.
- **Genres**: category pill buttons that open filtered browse views.

On an empty self-hosted server, the home screen directs the signed-in owner to add sources in admin. Other visitors see "Nothing here yet" until the owner adds series.

## Searching the catalogue

Search through the header search overlay or browse page search bar.

### Search overlay

Open the search overlay from any page with the navigation header by selecting the magnifying glass icon. Enter a title and press Enter. Komiq opens `/browse` with your query applied. Close the overlay by pressing Escape or clicking outside the bar.

### Browse search bar

At `/browse`, use the persistent search bar. When you type, komiq pauses briefly before updating results, preventing extra network requests and keeping browser history clean.

Catalogue search checks series titles and alternative names. Click the clear icon in the search bar to empty the query.

## Searching external sources

Catalogue search queries stored titles. With an account on the connected server, sign in and enter a query to see **Search external sources**, regardless of matches.

This triggers a federated search, which combines results from sources installed on that server. It selects one source per extension package, preferring English, and excludes adult sources unless you opted in through your account on that server. Source count and simultaneous requests are limited.

External results are deduplicated and displayed together. Because live queries contact upstream websites, external searches are rate limited. If you search too frequently, komiq shows a notice stating when you can search again.

## Browse page and results

The browse page at `/browse` displays up to 30 catalogue series cards per page. External search results appear in one list without pagination. Each card shows the cover image, title, chapter information, ratings, comic format flag, and status badge. Read [Ratings, comments, reports](/guide/community#two-ratings) for the two ratings a card can show. Scanlation translator credits appear when reported by sources.

Below the grid, a pagination bar navigates between pages.

Series that the administrator of the server blocked do not appear. Series that your own block rules hide do not appear either. Read [Accounts and profile](/guide/accounts#blocked-content) to set rules.

### Shareable filter URLs

Applied filters, query, sort, and page number are saved in the URL. Sharing, bookmarking, or reloading restores those choices, subject to your content settings and server support. Results can change; external search is not saved in the URL. After opening a series, browser Back restores results and scroll position if still cached in memory.

## Filtering series

On desktop computers, filters appear in a side rail. On screens narrower than 960 pixels, selecting **Filters** opens a modal sheet dialog.

### The draft filter model

The filter panel uses a draft model. Adjusting options does not query the server immediately:

- Select **Apply filters** to run your filter selections in a single query.
- Select **Reset** to clear the filters in your draft. Select **Apply filters** to commit the reset.
- On mobile, dismissing the sheet discards unapplied changes.

### Filter categories

Use these filters to narrow results:

- **Tags**: tags are organized by Genre, Theme, Format, and Content. Tags use tri-state controls. A tri-state control has three settings: click once to include (`+`), click twice to exclude (`-` with strikethrough), and click three times to turn off. Under **Tag options**, set **Include** or **Exclude** to **All** (AND, every selected tag) or **Any** (OR, any selected tag). Older servers with only genre filters do not support exclusions.
- **Status**: filter by publication status: Ongoing, Completed, Hiatus, or Cancelled.
- **Format**: filter by comic format: Manga (Japanese), Manhwa (Korean, including webtoons), or Manhua (Chinese).
- **Demographic**: filter by intended audience: Shounen, Shoujo, Seinen, Josei, or None.
- **Content rating**: choose between Safe and Suggestive. When "Show NSFW content" is enabled in your profile on the connected server, Erotica and Pornographic also appear.
- **Original language**: filter by publication language, such as Japanese, Korean, Chinese, or English.
- **Year**: enter a minimum release year, a maximum release year, or both (between 1900 and 2100).
- **Minimums**: set thresholds for minimum chapters, minimum rating (0 to 10), and minimum followers. Rating and follower figures come from MangaDex.
- **Source**: show only series that one source carries. Pick one source at a time. The list shows the sources of the connected server, each with its series count. An older server does not offer this filter.
- **Has readable chapters**: toggle this switch to display only series with a known chapter count greater than zero. It does not test whether a source can currently serve those chapters.

### Active filter chips

Applied filters appear as chips above the results grid. Click the close icon on any chip to remove that filter rule. Select **Clear all** to remove all applied filters.

## Sorting options

Order browse results using the sort menu:

- **Relevance**: ranks title and alternative-name matches, using title match priority, chapter count, and full-text relevance. Only available when a search query is entered; default sort for searches.
- **Latest update**: orders titles by recent chapter releases; default sort when browsing without a query.
- **Recently added**: orders titles by catalogue addition date.
- **Most viewed**: orders titles by reader popularity.
- **Most follows**: orders titles by follower count.
- **MangaDex rating**: orders titles by MangaDex's Bayesian rating, a score adjusted for the number of votes.
- **Reader rating**: orders titles by the average score that readers on this server gave. Titles with no reader rating come last.
- **Most chapters**: orders titles by total chapter count.
- **Title A–Z**: orders titles alphabetically.
- **Year**: orders titles by initial release year.

The direction button toggles between descending and ascending order (such as newest first versus oldest first). Relevance sorting does not support direction toggling.

To learn how to manage series you have found, read [Your library](/guide/library). To adjust your mature content visibility, see [Accounts and profiles](/guide/accounts).
