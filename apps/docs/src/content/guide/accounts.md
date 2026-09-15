---
title: Accounts and profile
description: Manage your account and profile preferences on the server your reader app connects to.
---

Your account belongs to the server you read on: the global server in a browser, a local server, or a server on the internet. Use it to track progress, save library series, join discussions, and customize preferences there. This guide covers registration, sign-in, temporary passwords, profiles, mature content preferences, and sign-out.

## Create an account

To register a new account, visit `/login` and select the **Create account** tab.

Enter your details:

- **Username**: choose a handle unique on that server, without spaces.
- **Email**: enter your email address for identification.
- **Password**: enter a password of at least 8 characters.

Select **Create account** to finish. The connected server registers your account. The app saves an authentication session token on your device and redirects you to your requested page. A session token is a secure credential stored on your device that verifies your identity without sending your password on every request.

### Closed registration

The owner of the connected server can disable public registration.

When registration is closed, the "Create account" tab is hidden. The sign-in page displays the note "New here? Ask the owner for an account." In this situation, the server owner creates accounts manually in the admin console.

## Sign in

Sign in with an account on the connected server:

1. Open `/login`.
2. Enter your username and password.
3. Select **Sign in**.

The app stores your session token locally and validates it against the server. If you arrived from a specific series or chapter, the app returns you there after sign-in.

In native desktop and mobile apps, the sign-in card displays the address of the current server with a **Change server** link. Switching servers clears the previous server’s session. Sign in with an account on your chosen server. See [Connect to a server](/guide/connect).

## Choose a new password

When an administrator on that server creates an account for you or resets your credentials, they set a temporary password. Accounts created by an administrator require a password update by default. Password resets always require one.

When you sign in with a temporary password, komiq displays a full-page screen titled "Choose a new password." This screen replaces the app until you set your own password.

- If you sign in and stay in the same session, the temporary password is held in memory. You only type your new password and confirm it.
- If you reload the page, enter your temporary password in the **Temporary password** field.
- Type your replacement password in the **New password** field (minimum 8 characters).
- Retype it in the **Confirm new password** field.
- Select **Save password**.

Once saved, the requirement clears and the reader application opens. If you signed into the wrong account, select **Sign out** at the bottom to return to the sign-in form.

## Profile and avatar

Open your profile by opening the account menu in the navigation header and selecting **Profile**, or by navigating to `/profile`.

Signed-out visitors see a prompt to sign in rather than reading statistics.

### Profile overview

The profile page displays:

- Your avatar photo, or a colored initial circle.
- Your display name, handle (`@username`), join date, and any role badges.
- Your biography.
- Four statistics: In library, Chapters read, Reading now, and Library avg.
- Your comment history with links to discussion threads.

### Edit display name and biography

To edit your public profile:

1. Select **Edit profile**.
2. Update your **Display name** (up to 50 characters).
3. Update your **Bio** (up to 500 characters).
4. Select **Save changes**.

If you attempt to navigate away while you have unsaved changes in the editor, komiq prompts you to confirm whether you want to discard your edits.

### Upload an avatar photo

1. On your profile page, click or tap the camera icon on your avatar.
2. Select an image file from your device. Komiq accepts PNG, JPEG, and WebP formats.
3. The image uploads to the connected server and updates your avatar across the header, profile, and comments on that server.

## Content settings and NSFW

The **Settings** screen manages content filtering. Open it from the account menu in the header, or from the **Settings** button on your profile. You can also jump directly to the section using `/settings#content`.

### Mature content setting

Mature content in komiq includes works flagged as "not safe for work", or NSFW. Adult ratings such as Erotica or Pornographic and adult source signals can set this flag. An administrator on the connected server can override it.

By default, mature content is hidden. To change this:

1. Open **Settings**.
2. Locate the **Content** section.
3. Toggle the **Show NSFW content** switch.

When the switch is off:

- Titles flagged as NSFW are excluded from search results, catalogue browsing, and update feeds.
- In the Browse filter panel, the Erotica and Pornographic rating chips are hidden.

When the switch is on:

- Adult-rated titles appear in search, browse, and update lists.
- Erotica and Pornographic chips become selectable in the Browse filter panel.

The setting is saved to your account on the connected server. It applies to all devices where you sign in to that account on that server.

### MangaDex ratings

A series can show two ratings. The yellow star is the rating from readers on the connected server. The orange star is the rating from MangaDex. Read [Ratings, comments, reports](/guide/community#two-ratings) for what each one means.

To hide the MangaDex rating:

1. Open **Settings**.
2. Locate the **Content** section.
3. Turn off the **Show MangaDex ratings** switch.

You then see the yellow star only. This choice is kept on your device, so it works when you are signed out, and you set it again on each device.

An administrator can turn MangaDex ratings off for the whole server. The switch is then off and you cannot change it.

### Blocked content

You can hide series that you do not want to see. A rule names tags, content ratings, or both. You must be signed in.

1. Open **Settings**.
2. Locate **Blocked content** in the **Content** section.
3. Select the tags for the rule. Type in **Search tags** to find a tag. A series must have all the tags you select.
4. Select the ratings under **Content rating**. A series must have one of them. Leave them empty for any rating.
5. Select **Add rule**.

The list shows each rule in words. `Gore + Harem` hides series with both tags. `Any series rated Erotica` hides series with that rating. `Gore, when rated Erotica or Pornographic` hides series with the tag and one of those ratings. A series is hidden when it matches any rule. To remove a rule, select the remove button next to it.

Your rules hide series from Browse, search, the home screen and the update lists. Your library does not change, and a series that you open by its link still opens. The rules are saved to your account on the connected server, and you can keep up to 50.

An administrator can also block content for the whole server. Those series are hidden for everyone and do not open.

### On-device chapter fetching

In apps that include the embedded content engine, an additional setting titled **Fetch chapters on this device** appears under **Reading**. The desktop app includes the engine. An Android app includes it when it was built with it.

When enabled, the app downloads chapters directly from sources. When disabled, the server your reader app is connected to retrieves them. If the server operator locked this setting so apps must fetch their own chapters, the switch remains on and cannot be turned off. Changing this setting reloads the app.

## Sign out

To sign out:

1. Open the navigation header.
2. Click or tap your avatar to open the account menu.
3. Select **Sign out**.

Signing out performs three actions:

- It removes your session token from local browser or application storage.
- It clears your local offline queue so pending writes do not replay under another user.
- It asks the server to revoke the session token.

If the server revocation request fails, the app still clears your local session after that attempt. The header displays a temporary notification that the session may still be valid on the server.

If you sign out while viewing `/profile` or `/library`, komiq redirects you to the home page at `/`.

To learn how to explore manga and use filters, see [Browse and search](/guide/browse). For information on how your data is handled, see the [Privacy policy](/guide/privacy).
