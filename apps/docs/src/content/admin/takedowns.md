---
title: Takedowns
description: Stop serving a work or one chapter copy, review takedowns and lift them when needed.
---

A takedown stops your server from serving a work or one chapter copy to everyone. Use it when you need to remove content from your server, such as after a rights request. This page explains how to add, review and lift takedowns, and what cached copies can remain.

## Choose what to take down

A **work** is one series across its sources. A **copy** is one upload of one chapter by one source.

- `WORK` takes down the whole work, its chapter pages and its covers.
- `COPY` takes down one copy. Its id is `mc_` followed by 32 lowercase hex characters, the same copy id found in `Page.mediaPath`.

When you add a work takedown, a merged-away work id names its survivor. The server records the takedown against that survivor.

A reason is required. It must contain text and be at most 500 characters. The admin console does not yet have a takedown screen. Use the admin GraphQL API or the command line.

## Use the admin API

GraphQL is the server's API for queries and changes. Send these operations to your server's `/graphql` endpoint while signed in as an administrator. Read [GraphQL API](/developers/graphql) for request details. Replace `w_example` with your work id.

Add a work takedown:

```graphql
mutation {
  takeDownMedia(
    kind: WORK
    subjectId: "w_example"
    reason: "Rights request received by the server owner"
  )
}
```

For one copy, use `kind: COPY` and its `mc_` id as `subjectId`.

Review the recorded takedowns:

```graphql
query {
  mediaTakedowns {
    kind
    subjectId
    reason
    createdBy
    createdAt
    viaWork
  }
}
```

`viaWork` names the work that caused a copy row to be recorded. Use the listed `subjectId` when lifting a takedown:

```graphql
mutation {
  liftMediaTakedown(kind: WORK, subjectId: "w_example")
}
```

Both mutations return `false` when there was nothing to change.

## Use the command line

With Docker, run these commands in the server container, with the same environment as the server. Replace the example ids with yours.

```sh
komika-server takedown add work w_example --reason "Rights request received by the server owner"
komika-server takedown list
komika-server takedown remove work w_example
```

For one copy:

```sh
komika-server takedown add copy mc_0123456789abcdef0123456789abcdef --reason "Remove this chapter copy"
komika-server takedown remove copy mc_0123456789abcdef0123456789abcdef
```

Changes through the admin API apply at once on that server. A command-line change reaches a running server within about 5 seconds.

## What changes on the server

A taken-down work leaves every list and cannot be opened. The server refuses its chapter pages and covers. It deletes stored covers and their resized versions. The cover pass skips the work while the takedown holds.

A work takedown also records a copy takedown for every copy the work has at that moment. Those rows keep the copies denied after later catalogue changes. Old merged-away ids are denied too, because cover links keep their original work id.

A merge moves the losing work's takedown onto the survivor. A split copies the takedown to the new work. Lift it by hand on each work that should become available again. Lifting a work also lifts the copy rows recorded with it. Separately recorded copy takedowns stay. A lifted work's cover comes back with the next cover pass.

Site-wide block rules also close chapter pages. Covers stay open under a block rule. Read [Settings](/admin/settings) for rules based on tags and ratings.

## What can remain after a takedown

A takedown stops new serving from your server. It cannot recall pages or covers already in a reader's browser cache, downloaded or offline chapters on a device, or copies other people saved.

A shared cache, such as a content delivery network (CDN), can keep an exact object for up to one policy lease, 600 seconds. A lease is the server's signed permission to serve content for a limited time. Older chapter page URLs keep their 300-second cache lifetime. Source cover responses also use 300 seconds. Current versioned cover URLs allow 600 seconds in shared caches, while a browser can retain those cover bytes for a year.

Keep your proxy respecting the server's cache headers. Read [Reach it from the internet](/self-host/internet-access) for setup and [Exact media and signing](/developers/media-manifests) for the policy lease and verifier expiry rules.
