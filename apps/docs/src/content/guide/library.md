---
title: Library and progress
description: Manage your library and reading progress on the connected server, follow updates, and view account notifications.
---

Your library belongs to your account on the server the reader app is connected to. It keeps track of what you read, what you plan to read, and your reading progress on that server. This guide explains how to add series to your library, organise them by shelf, check new releases on the updates page, and manage notifications.

## Add a series to your library

You can add a series from its series detail page:

- Click Add to library in the actions area. The button changes to In library.
- Click the heart icon to add the series to your favourites. Adding a series to your favourites automatically adds it to your library.
- Once the series is in your library, select a shelf from the shelf menu.

To remove a series from your library, click In library again. Removing a series clears its shelf assignment and favourite status from your account.

## Reading status and shelves

Komiq uses four shelves to organise your reading:

- Reading: Series you are currently reading.
- Completed: Series you have finished reading. The menu offers Completed when publication has ended, or when Completed is already your shelf. If a series is still ongoing, catching up on chapters leaves its automatically derived shelf on Reading.
- On Hold: Series you have paused temporarily.
- Plan to Read: Series you intend to read in the future.

### Automatic shelf derivation

If you do not choose a shelf manually, komiq derives your shelf from your reading progress:

- If you have read zero chapters, the series lands on Plan to Read.
- If the publication has ended, has at least one chapter, and you have read every chapter, the series lands on Completed.
- In all other cases, the series lands on Reading.

A shelf you choose manually always takes precedence over automatic derivation.

### Change a shelf

To change a series shelf from its detail page, click the shelf button beside the library button and pick an option.

To change a shelf from your library, click the shelf chip in the lower corner of the series cover card. Select a new shelf from the popover menu.

## Track your progress

The library interface provides several tools to help you track your reading.

### Continue reading rail

When you have library series with some but not all chapters read, the top of your library displays a Continue Reading section. Each card shows the series cover and a progress bar showing the percentage of chapters you have read. Click Resume to open the series page, then use Continue Ch. to open the first unread chapter after the one you have read through. If none remains after that point, it opens the first unread chapter in the list.

### Chapter checkmarks

On each series page, every chapter row displays a read mark indicator. When you are signed in, click the checkmark button next to any chapter to toggle it between read and unread. Your read count updates immediately, adjusting your position on your library shelves.

When you finish a chapter, komiq also marks the chapters before it in the series' chapter list as read, on every source. Special chapters, such as extras, stay as they are. While the server is still building the chapter list of a series, only the earlier chapters on the same source are marked.

If you mark a chapter unread, it stays unread. Finishing a later chapter does not mark it read again. Neither does a late save of a later chapter, for example an offline save from another device that syncs afterwards. The chapter becomes read again only when you read it to the end or click its checkmark again. A late save that finished this same chapter also counts as reading it unless the server refuses it as out of date. Reading or ticking the same chapter on another source of the same series counts too. Marking a chapter unread also clears it on the other sources of that series.

The read count and the last-read time of a series use the same rule, so they match the checkmarks you see.

Continue Reading and checkmarks use the same chapter order and progress rules. See [Read a chapter](/guide/reading) for specials and saves from an older view.

### Filter and sort your library

The library toolbar allows you to filter and sort your titles:

- Shelves: Click the shelf tabs to filter between All, Reading, Completed, On Hold, Plan to Read, and Favorites. Each tab displays the count of matching titles.
- Formats: Use the Format dropdown to filter by comic type: All formats, Manga, Manhwa, or Manhua.
- Search: Type in the Filter by title input field to filter series by title.
- Sort: Use the Sort by dropdown to order series by Latest update, Recently read, Recently added, Title, or Unread chapters. Click the direction button beside the dropdown to toggle between ascending and descending order.

## The updates page

The updates page at `/updates` shows series with newly available chapters across the connected server’s catalogue.

- Trending rail: The top section displays trending series.
- Latest chapters: The main feed includes two viewing options. The New tab lists series with newly available chapters in reverse chronological order with page navigation. The Hot tab lists popular trending series.
- Format filters: Use the format chips to limit updates to Manga, Manhwa, or Manhua.
- Release timestamps: Cards display relative timestamps such as 4h or 2d when available. Move your pointer over the chapter details to see whether the time reflects the upstream chapter release or when komiq first had it available.

## Notifications

The notification bell in the top navigation bar alerts you to activity related to your account on the connected server:

- Notification triggers: Inbound notifications arrive when another reader replies to your comment, or when one of your comments reaches a milestone count of likes.
- Unread count: A badge on the bell displays the number of unread notifications, capped at 99+.
- Reading notifications: Click the bell to open the notifications panel. Opening the panel marks all notifications as read and clears the badge.
- Navigation: Each notification item displays the reply author's avatar or a like icon, an excerpt of your comment when available, and the relative time. Notifications with a link open the series page or the chapter reader.

Using the library and notifications requires an account on the connected server. For details on managing your credentials, see [Accounts](/guide/accounts).
