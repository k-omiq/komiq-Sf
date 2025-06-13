/**
 * Pure chapter-LABEL and external-chapter helpers, factored out of `source.ts` so the two
 * rules below are unit-testable without a live `backend`/`images` context. Mirrors the
 * `translator-select.ts` / `chapter-owner.ts` split.
 *
 * RULE 1 — a chapter COUNT is never a chapter NUMBER. Owner, verbatim: *"By chapter, I mean
 * the name of the chapter, not the amount of chapters a series has per source."* The two
 * quantities are unrelated: a series with 412 mirrored chapters whose newest release is
 * 10.5 was being labelled "Ch. 412". Every card label now comes from the server's
 * `label`/`latestChapter` — the newest RELEASED chapter — or renders nothing at all.
 *
 * RULE 2 — a chapter hosted off-site has no pages for us to serve. ~35,000 chapters (4% of
 * the mirror: MangaPlus, Comikey, NamiComi, BiliBili) carry an `externalUrl` instead. The
 * reader must send the reader there rather than request pages it will never get and then
 * blame "a licensing gap".
 */

/**
 * A bare chapter number: "45", "10.5", "0". Anchored, because a partial match would accept
 * "45 and 46" — a real upstream label (`Chapter 13,14` is in the catalogue) that is not one
 * number and must not be printed as one.
 */
const BARE_NUMBER = /^\d+(?:\.\d+)?$/;

/**
 * A `Ch.` / `Chapter` prefix the label already carries. Stripped before classification —
 * NOT to normalise upstream text, but because `getUpdates` re-parses an already-formatted
 * chip back into a label (`source.ts` strips `^Ch\.\s*` when it folds the Suwayomi and
 * canonical halves into one feed-row shape). Without this, one more hop through that merge
 * would render "Ch. Ch. 45".
 */
const CH_PREFIX = /^ch(?:apter)?\.?\s*/i;

/**
 * The card/chip text for a server chapter label — the ONE place that decides how a chapter
 * is named on a card.
 *
 * A numeric label gets the "Ch. " prefix; a WORD label is printed verbatim, because the
 * server's `label` column holds either ("45", "10.5", "Oneshot", "Extra" — Phase A2), and
 * prefixing the word half produces "Ch. Oneshot". That is not hypothetical: of 600 live
 * `/updates` rows sampled 2026-07-30, **5 carried a non-numeric label** ("Oneshot" ×4,
 * "Brosquito" ×1 — a chapter title standing in as the label), and all 5 rendered
 * "Ch. <word>".
 *
 * Returns '' for a missing/blank label. A blank chip is the honest render — `cardSub` drops
 * the empty half and its separator — and it is deliberately the only fallback: the caller
 * must NOT pass a chapter COUNT here (Rule 1). Nothing beats "Ch. 412" on a series whose
 * newest chapter is 10.5.
 */
export function chapterChip(label: string | null | undefined): string {
	const trimmed = label?.trim();
	if (!trimmed) return '';
	const bare = trimmed.replace(CH_PREFIX, '');
	// Only a label that is purely a number is re-prefixed. "Chapter Zero" keeps its own
	// words rather than being served back as the bare "Zero".
	return BARE_NUMBER.test(bare) ? `Ch. ${bare}` : trimmed;
}

/**
 * Validate an upstream `externalUrl` into something safe to put in an `href`, or `null`.
 *
 * The value is upstream-controlled MangaDex metadata that we mirror verbatim, so it reaches
 * the DOM as attacker-influenced text. Interpolating it unchecked makes a `javascript:` (or
 * `data:`) chapter URL a stored-XSS vector on every reader that opens that chapter — hence
 * the explicit http/https allow-list rather than a "does it look like a link" test.
 * Relative and unparseable values are rejected too: a redirect that lands on our own origin
 * would just re-enter the reader and loop.
 */
export function externalChapterHref(raw: string | null | undefined): string | null {
	const trimmed = raw?.trim();
	if (!trimmed) return null;
	try {
		const url = new URL(trimmed);
		if (url.protocol !== 'http:' && url.protocol !== 'https:') return null;
		return url.href;
	} catch {
		// Not an absolute URL — nothing safe to send anyone to.
		return null;
	}
}

/**
 * The host to NAME in the redirect prompt ("Read on mangaplus.shueisha.co.jp"), or '' when
 * the URL is unusable. Naming the destination is the point of the prompt: an unexplained
 * jump to a third-party site reads as a hijack, and the four hosts behind these chapters
 * (MangaPlus, Comikey, NamiComi, BiliBili) are exactly the licensed readers the chapter
 * legitimately lives on. `www.` is dropped because it is noise, not identity.
 */
export function externalChapterHost(raw: string | null | undefined): string {
	const href = externalChapterHref(raw);
	if (!href) return '';
	return new URL(href).hostname.replace(/^www\./, '');
}
