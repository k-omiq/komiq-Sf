---
title: Cover issues
description: Diagnose failed cover downloads on your server and repair or replace broken cover images.
---

Your server caches series covers to deliver fast image loading and reduce reliance on third-party sources. Recorded crawler failures appear on the Cover issues page in the admin app.

This guide explains how cover processing works, what causes covers to land in the issues queue, and how to repair or replace broken images.

## How komiq processes covers

To keep the reader responsive, your server stores optimized covers in the separate covers database's blob table (`work_cover_blob`). Covers are served locally at `/covers/{work_id}.webp`.

When enabled, the background cover crawler fetches and optimizes covers for eligible uncached works. The pipeline applies these processing steps:

- **Source size limit**: Raw images passed to the processor cannot exceed 24 MiB. The decoder also limits dimensions and memory allocation.
- **Completeness checks**: Your server checks image byte streams before decoding to reject truncated or corrupted downloads.
- **Aspect-preserving downscaling**: The image is resized to fit longest-edge boundaries (512, 448, 384, 320, or 256 pixels). Images smaller than the target size are never upscaled.
- **WebP compression**: The image is re-encoded as lossy WebP using a quality ladder from 82 down to 50. Your server accepts the first size and quality combination at or below the 150 KiB target. If none fits, it returns the smallest encoding produced.

```flow
Your server fetches source cover -> validates source size at or below 24 MiB
Ensures bytes are complete -> decodes and downscales longest edge
Encodes lossy WebP with a 150 KiB target -> stores blob in database
```

The crawler records fetch, processing, and storage failures in `work_cover_issue` and surfaces the title on the Cover issues page. Other paths, such as lazy cover loading, do not record every failure in this queue.

## The Cover issues page

The Cover issues page (`/bugs`) lists works with recorded cover issues.

### Table columns

Each entry in the list displays diagnostic information and repair actions:

- **Cover**: Displays a thumbnail preview or a broken image indicator.
- **Work**: The primary series title and canonical work identifier.
- **Reason**: A badge indicating the error category, alongside the failure message from your server.
- **Tries**: The number of failed attempts recorded for this title.
- **Last attempt**: The date and time when your server last attempted processing.
- **Fix**: Action buttons for retrying the operation or uploading a local replacement file.

### Failure reasons

The console gives these processing and storage errors readable labels:

- **Image too large** (`too_large`): The image exceeds a source size limit.
- **Unsupported or damaged file** (`unsupported`): The error message indicates an unsupported or corrupt image, or zero dimensions.
- **Image is empty** (`empty`): The image processor received empty input.
- **Could not convert** (`encode`): Other processing errors, including WebP encoding failures.
- **Could not save** (`store`): Your server encountered a database error while attempting to commit the WebP blob to storage.

Other reasons appear as raw codes, including `truncated` and `fetch`.

## How to repair a cover

You have two methods to resolve an item on the Cover issues page.

### Retry the download

If a failure was caused by a temporary network glitch, server timeout, or rate limit, use the retry button:

1. Click **Retry** on the affected row.
2. Your server tries MangaDex first when enabled and available, then linked Suwayomi sources until one supplies a processable image.
3. Your server runs the image through the validation, resizing, and WebP compression pipeline.
4. If processing succeeds, the cover is saved to the database, the cache version updates, and the item is automatically removed from the queue.
5. A deterministic processing failure updates the attempt count and error details. If no image can be fetched, the existing issue remains unchanged. Truncated downloads are not recorded by manual retry.

### Upload a replacement image

When an upstream source is permanently unreachable, returns a 404 error, is blocked by anti-bot verification, or provides low-resolution artwork, you can upload an image file from your device:

1. Click **Upload** on the affected row.
2. Select an image file from your local file picker. Supported formats include standard web image types such as JPEG, PNG, and WebP.
3. Your server receives the file, verifies that it is within size limits, processes it as lossy WebP with the 150 KiB target and smallest-encoding fallback, and stores it in the database.
4. The issue is marked resolved and disappears from the Cover issues list immediately.

A background cover download already in progress cannot replace your uploaded cover.

## Bulk cover storage

To generate and store covers for eligible uncached works, use the maintenance tools in [Settings](/admin/settings).

Navigate to **Settings** > **Maintenance** and find **Store covers on this server**. Running this job crawls eligible uncached works. Recorded issues whose retry window has not expired are skipped, and MangaDex covers require MangaDex to be enabled.
