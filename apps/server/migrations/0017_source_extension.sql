-- Per-source extension coordinates for on-device provisioning (native-embedded-Suwayomi §2.1).
--
-- The native client embeds Suwayomi and must install the *exact* extension a
-- `source_series` came from. The operator-side Suwayomi already sees every installed
-- extension (its `extensions`/`sources` GraphQL); this table records, per Suwayomi
-- source id, the coordinates a device needs to install/pin that extension:
-- package id, the repo it came from, the artifact name, and the `version_code` at
-- catalogue time (so a device keeps its extension >= the version we catalogued with).
-- Keyed by `source_id` to join `source_series.source_id` (matching '' unknown / 'mangadex').
-- Populated best-effort by the scanner from the live Suwayomi; a missing row just means
-- one fewer install hint, never a catalogue failure.
CREATE TABLE source_extension (
    source_id    TEXT PRIMARY KEY,   -- Suwayomi source id (matches source_series.source_id)
    pkg_name     TEXT NOT NULL,
    repo_url     TEXT NOT NULL,
    apk_name     TEXT,
    version_code INTEGER,
    lang         TEXT,
    is_nsfw      INTEGER NOT NULL DEFAULT 0,
    updated_at   TEXT NOT NULL
);
