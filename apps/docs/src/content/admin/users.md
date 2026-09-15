---
title: Users and reports
description: Manage accounts on your server, assign roles, reset passwords, and triage reader reports.
---

Use the Users and Reports pages in the admin app to manage access to your server and feedback from its readers. You can create accounts, assign admin roles, reset passwords, ban abusive users, and triage reports filed by readers. This guide explains how to manage accounts and resolve incoming reports.

## Account roles

Every user account on your server has one of two roles:

- Member: A standard reader account. Members can read comics, track progress, bookmark titles, and leave comments in reader apps. They cannot access the admin console.
- Admin: Has full access to the admin console, including catalog editing, metadata overrides, extension installation, user management, and server settings.

The user table displays an `Admin` badge or a `Member` badge for each account. Click `Make admin` or `Revoke admin` on a user row to change permissions. The console prompts for confirmation before applying the change. You cannot revoke your own admin access from your own user row.

## Creating an account

Administrators can add accounts directly:

1. Click `Add user` in the page header.
2. Enter a username (at least 3 characters) and an email address (must include `@`).
3. Toggle the `Admin` switch if the person needs access to the admin console.
4. Click `Add user`.

The console creates the account and displays a temporary password dialog. The password contains 12 random characters arranged in three groups separated by hyphens (such as `xxxx-xxxx-xxxx`). The generator excludes visually ambiguous characters to prevent typos.

Click `Copy` to copy the password and send it to the user. This password is displayed only once and is never stored in plain text. When the user signs in for the first time, your server requires them to create their own password.

## Resetting a password

If a user forgets their password or needs credentials revoked:

1. Locate the user in the table and click `Reset password`.
2. A confirmation dialog explains that this signs the user out of every active session on all devices.
3. Confirm the action.

The console generates a fresh temporary password with 12 random characters and two separating hyphens. Your server saves its hash and revokes active sessions. A dialog displays the password with a `Copy` button. Provide this temporary password to the user. Your server will require them to choose a new password upon their next sign-in. You cannot reset your own password using this button.

## Suspending and restoring accounts

To suspend an abusive member account, click `Ban` on their row. A confirmation modal warns that the user will be blocked from signing in until unbanned. Your server also revokes their active sessions. You cannot ban yourself or another admin.

Banned accounts show a red `Banned` badge and a strike-through across their username. To restore sign-in access, click `Unban`.

## The Reports queue

Readers connected to your server can report issues through the reader app’s Support menu. Incoming submissions appear in the Reports queue, newest first.

### Filtering reports

The toolbar allows filtering by status and issue type:

- Status: Filter between `Open`, `Resolved`, `Rejected`, and `All`. The header shows the total open backlog alongside the count for your current filtered view.
- Kind: Filter by the report category:
  - `Wrongly merged`: Two distinct series were merged into one work.
  - `Needs merging`: Two separate listings represent the same comic.
  - `Missing series`: A requested series is not available.
  - `Comment abuse`: Inappropriate comments or reader harassment.
  - `Other`: General feedback and reports.

### Report table columns

- `Kind`: Displays the category badge. Badges use color tones to indicate urgency, with red for comment abuse and yellow for merge issues.
- `Subject`: Shows the series title, linked subject identifier, or reported username (`@username`). Subject identifiers link directly to the series editor in the admin console.
- `Reporter`: Displays the submitting username or `Anonymous`.
- `Filed`: Submission timestamp.
- `Triage`: Action buttons to view details and update report status.

## Triaging reports and moderating comments

Click `Read` on any report row to expand the detail panel.

### Report details and links

The panel displays the full message entered by the reporter.

If the reporter included a web link, the console parses the URL protocol. Only links starting with `http:` or `https:` are made clickable. Any other format is rendered as plain text to prevent script injection in the admin console.

### Moderating abusive comments

For reports that include a comment identifier, the panel shows a stored excerpt of the reported comment, the comment identifier, and the author. This excerpt is recorded when the report is submitted, ensuring evidence is preserved even if the live comment changes. `Comment abuse` reports can also name a user without including a comment.

Click `Delete this comment` to delete the comment and its entire reply subtree from the database. This action is irreversible. The report keeps its stored snapshot for record-keeping.

### Internal admin notes

The `Add a note` input lets you record administrative actions. Readers never see this note:

- Notes are saved when clicking `Mark resolved`, `Reject`, or `Reopen`.
- Typing text replaces the stored note. Leaving the field blank preserves the existing note.
- Notes cannot be cleared once saved.

### Resolving reports

- `Mark resolved`: Closes the report as completed.
- `Reject`: Closes the report without changes.
- `Reopen`: Reopens a previously closed report.

When handling duplicate or split reports, perform the necessary changes on the [Catalog](/admin/catalog) page. [Duplicates](/admin/duplicates) lists pairs flagged by komiq itself, so a reader-reported pair may not appear there.
