---
title: Read a chapter
description: Read available chapters, adjust display settings, switch sources, and save progress on the connected server.
---

Use the reader app to read chapters available through the server it is connected to. When you sign in to an account on that server, it saves your reading progress to that account. This guide explains how to open chapters, change display modes, configure reading settings, and switch between sources when a series has multiple releases.

## Open a chapter

You can open a chapter from any series page.

- The reading button opens the first unread chapter of a series, or the first chapter if all chapters are read. It reads Start reading, or Continue Ch. followed by the chapter number when you have read chapters or that number is greater than 1.
- Browse the chapter list under the Chapters tab. Each row shows the chapter number, with a title, release date, and group or source when available. Click any row to open that chapter.
- When a chapter has more than one version, its row shows a button with a count, such as +1. Click the button to list every version of that chapter, then click a version to open that exact copy.

Some chapters are hosted directly by external publishers. These chapters display an OFF-SITE badge in the chapter list. When you open an off-site chapter, komiq displays a message explaining that the publisher hosts the pages, along with a link to read the chapter on the publisher's website.

## Reading modes

Komiq provides three reading modes. To switch modes, open the reader menu and click Mode to cycle through the options:

- Long strip: Pages appear in a continuous vertical column. You scroll down through the pages. Comics identified as webtoons enforce long strip mode automatically.
- Single page: Displays one page at a time.
- Double page: Displays two pages side by side. If an image is a wide spread where the width exceeds the height, komiq displays it on its own spread instead of squeezing it beside another page.

When you switch modes, komiq keeps you on your current page. Your preferred mode is saved in your browser storage.

## Reader settings

To open the reader menu, click the Menu button in the top bar or press `M` on your keyboard. With Header auto-hides, tapping the reading area toggles the header. With Header hidden, tapping opens the menu. With Header always shown, tapping opens the menu in paged modes and does nothing in long strip mode. Press `Esc` or click the darkened backdrop to close an overlay menu. Press `M` or click the close icon to unpin a docked menu.

On screens that are at least 1024 pixels wide, you can click the pin icon in the menu header to dock the menu beside the pages. On smaller screens, the menu opens as an overlay drawer.

The menu contains display options:

- Page width: In long strip mode, on screens at least 520 pixels wide, drag the slider to change the column width from 480 pixels to 1400 pixels. The default width is 720 pixels.
- Page gaps: In long strip mode, toggle between uninterrupted pages and spacing between pages.
- Fit: In paged modes, choose Fit to screen, Fit width, Fit height, or Original size.
- Direction: In paged modes, choose Left to right or Right to left. Tap zones and keyboard shortcuts follow this direction.
- Header: Choose Header auto-hides (slides away when you scroll down, reappears when you scroll up), Header always shown, or Header hidden.
- Progress: For series that do not enforce long strip mode, choose Page bar (a segmented bar showing every page), Thin progress line, or Progress hidden. Enforced long strips display a continuous progress line.
- Read on source: When a source link is known, click this link to open the chapter or series on its original source website in a new browser tab.

## Read on a phone

The phone app behaves like an app, not like a web page:

- Zoom: Pinch to zoom in on a chapter page. The rest of the app does not zoom.
- Status bar: On iOS, the clock and battery row hides together with the reader's top bar, and returns with it. It also returns when you leave the chapter.
- Top bar and notch: The reader's top bar and page bar stay clear of the notch and the home indicator, in portrait and on its side.
- Phone on its side: Outside the chapter reader, the bottom tab bar stays, as one compact row.

## Move between pages and chapters

In paged modes, you can turn pages in several ways:

- Screen tap zones: Tap the outer 35 percent of either screen edge. Tapping the left or right side moves forward or backward based on your reading direction. The middle 30 percent follows the reading-area tap behaviour described above.
- Touch gestures: Swipe left or right across the page on touchscreens. Page-turn swipes are disabled while zoomed in or when the page is wider than the screen.
- Keyboard navigation: Press the Left Arrow and Right Arrow keys to step through pages according to your reading direction. Press `PageDown` or the Space bar to move to the next page. Press `PageUp` to return to the previous page.
- Page jumping: Select a page from the Page dropdown in the reader menu, or click any segment in the bottom page bar.

Moving past chapter boundaries opens adjacent chapters:

- Turning forward on the final page of a chapter opens the next chapter.
- Turning backward on the first page opens the previous chapter.
- In long strip mode, scrolling past the last page reaches the end-of-chapter block. Click Next chapter to continue reading, or click All chapters to return to the series page.
- The top header provides Previous chapter and Next chapter shortcut buttons.

To reduce pauses between chapters, komiq prefetches the next chapter once you reach 40 percent of your current chapter, or whenever you move your pointer over a next chapter button.

When you are signed in to the connected server, reading progress is saved to your account automatically when you finish a chapter, click to another chapter, switch browser tabs, or navigate away from the reader. For more details on tracking your reading history, see [Library](/guide/library).

## Pick a source

Some series exist on multiple sources or have uploads from different scanlation groups.

The server's administrator can mark a scanlation group as AI translated. The administrator can also mark a whole source, when the source translates its own series and its chapters name no group. Chapters from such a group or source show an AI or Partial AI badge, in the chapter list and under Group & source. Partial AI means AI is part of the work, for example a machine translation that a person edits.

### Switch a single chapter copy

If multiple versions of the current chapter number exist, open the reader menu and look under Group & source. Click any version row to switch to that exact copy. This updates the current chapter without changing your default source for the rest of the series.

### Change the default source for a series

Chapters open from the default source of the series. The series title, description, authors, status and cover stay the same whichever source you pick. You can change it in two places:

- On the series page, click More info, then click a source under Sources. The default source has an accent outline.
- In the reader menu, choose an option under Default source for this series, shown when more than one source is available. The reader stays on the chapter number you are reading.

The reader app saves your choice in browser storage.

When the default source does not have a chapter, that chapter opens from another source that has it. The chapter after it opens from the default source again.
