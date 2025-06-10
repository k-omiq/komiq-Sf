# Dedup review queue — triage, 2026-07-26

## The key correction

`method: "title_exact"` does **not** mean the titles match. This queue is, by construction,
the pile the matcher _already refused_ to auto-merge: `dedup.rs:268` auto-merges an exact
alias hit only when it is unambiguous (`exact_candidates == 1`) and the normalized title is
at least `EXACT_MERGE_MIN_TITLE_CHARS` (5). Anything failing either guard is **downgraded**
into this queue. So "title_exact" here means "exact alias hit the matcher judged unsafe".

Merging on that label alone would have irreversibly folded ~2,200 unrelated works
(`merge_works_ex` physically DELETEs the losing work).

## What the score actually means

From `score_candidate` (`catalog/dedup.rs:328`):

    score = 0.6*title_sim + 0.4*corrob + 0.05*author + 0.03*year

`corrob` is description similarity or cover pHash — and `COVER_PHASH=off` in production, so
pHash is permanently `None`. A score of **exactly 0.60** therefore means: the title matched
perfectly and _nothing else did_. 2,232 of the 2,501 `title_exact` rows sat at 0.60.

## Completed

**91 works merged** (batch 1). Criteria: `title_exact`, score >= 0.90 (i.e. real description
corroboration), not contested by another queue row, no edition-variant marker.
Verification: for all 60 candidates readable via `canonicalSeries`, the source title was
confirmed to be a genuine alias of the candidate work — 100% hit rate.

6 of the 97 returned "No such merge candidate" — each was the second row of a duplicated
pair, already consolidated by the first merge. Benign.

Queue: 3,412 -> 3,303.

## Staged but NOT executed (65)

Tier `SAFE_identical+author`: normalized titles identical **and** authors matching, with no
edition-variant or sequel marker. Blocked by the sandbox permission classifier before
execution. Row list: `/tmp/safe65.json`.

These are mostly genuine multi-duplicate clusters — e.g. `God of Blackfield` appears 3x
under one author (Mu Jang), `Dao of the Bizarre Immortal` 3x, `Eleceed` 2x.

## Remaining 3,303, re-tiered against fresh evidence

Evidence pulled per pair: candidate work via `canonicalSeries` (2,496 works; 1,765 readable —
the rest are Suwayomi-only or NSFW-gated), and the source work resolved by exact-title
`search` (1,712 of 3,303 resolved).

| Tier                         | Rows | Meaning                                                                                                               |
| ---------------------------- | ---- | --------------------------------------------------------------------------------------------------------------------- |
| `WEAK`                       | 1067 | No corroboration either way. Needs a human.                                                                           |
| `AUTHOR_CONFLICT`            | 814  | Authors positively disagree — **likely true non-matches**, good reject candidates.                                    |
| `identical_title_unverified` | 640  | Titles identical but source work not resolvable via search.                                                           |
| `VARIANT_EDITION`            | 350  | `(Official Colored)`, `(Canvas)`, `(Book Version)`, `(Oneshot)`, `(Pre-serialization)`. Per your call: keep separate. |
| `same_author_diff_title`     | 268  | Same author, different titles — often a _different work by the same author_.                                          |
| `SEQUEL_SUSPECT`             | 66   | `Rosario to Vampire Season II` -> `Rosario to Vampire`, `Shin Elf-san` -> `Elf-san`.                                  |
| `SAFE_identical+author`      | 65   | Staged above.                                                                                                         |
| `identical_title_no_author`  | 33   | Titles identical, neither side has an author.                                                                         |

## Findings worth acting on separately

- **The catalogue has deep duplication, not just pairs.** `Teenage Mercenary` and
  `Mercenary Enrollment` are both aliases of one Korean work (입학용병) that _also_ exists as a
  third work, `Iphak Yongbyeong`. The queue models pairs, so consolidating a 3-way cluster
  takes multiple passes.
- **`AUTHOR_CONFLICT` (814) is the highest-value reject batch.** Spot-checked samples are
  genuine non-matches (`Futari no Houkago`/Noripachi -> `Tsuyameku Onna`/Guusuka). Rejecting
  is non-destructive and would cut the queue by a quarter.
- **Generic titles drive the false positives.** `Housekeeper` x3, `Horizon` x3,
  `House Sitting` -> `Hajimete Ecchi`. These are exactly what the ambiguity guard catches.
- `canonicalSeries` returns "No such work" for a work that merely lacks a MangaDex anchor
  (`graphql/mod.rs:2729`) — worth distinguishing from a genuine 404 for admin tooling.

## Artifacts

- `/tmp/final.json` — all 3,303 rows with evidence + tier
- `/tmp/safe65.json` — the 65 staged merges
- `/tmp/merge_results.json` — batch-1 results
