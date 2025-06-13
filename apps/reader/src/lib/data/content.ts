/**
 * Editorial copy for the Support / Report / About pages.
 *
 * Every string here is a factual claim about how this service actually runs, so it
 * lives in one module rather than being scattered across three templates — if a claim
 * stops being true it has to be changed in exactly one place.
 *
 * This file previously held a fabricated FAQ and a support email that did not exist.
 * Nothing invented goes back in: if we can't answer it, we don't list it.
 */

import type { ReportKind } from '@komika/types';

/**
 * Where donations go. Deliberately coarse — the point is that the money covers
 * infrastructure, not that a reader can audit the invoices.
 *
 * One clause each: /support renders these as a plain list, not as cards, so a
 * paragraph here reads as filler rather than as an answer.
 */
export const EXPENSES: { title: string; desc: string }[] = [
	{
		title: 'Image workers',
		desc: 'Fetching each page from its source and handing it to your reader. The biggest line, and it grows with how much everyone reads.',
	},
	{
		title: 'Bandwidth',
		desc: 'Every page and cover crosses the network to reach you. Nothing is pre-uploaded, so it is billed as it happens.',
	},
	{
		title: 'Domains',
		desc: 'The komiq.cc names and their certificates — the reader, the API and the image workers each need one.',
	},
	{
		title: 'Maintenance',
		desc: 'Servers, monitoring, and the unglamorous hours spent fixing whatever broke.',
	},
];

/**
 * Whether a field is shown at all, and if so whether the reader may leave it blank.
 * Absent = the field is not part of this topic's form and its value is never sent.
 */
export type FieldNeed = 'required' | 'optional';

/** One reportable problem: the copy for its card and the fields its form needs. */
export interface ReportTopic {
	kind: ReportKind;
	title: string;
	/** Card blurb — what this option is for. */
	desc: string;
	/** Form heading, once the reader has picked this topic. */
	heading: string;
	/** What we need them to write, so the report is actionable. */
	prompt: string;
	/**
	 * Replaces `prompt` when the reader arrived with the subject already attached (today:
	 * a `?comment=` id from the Report link under a comment). Without it the form asks for
	 * something it is no longer showing a field for.
	 */
	promptAttached?: string;
	/**
	 * Which extra inputs this topic renders, and which of them the reader must fill.
	 * The form derives BOTH the `required` attribute and the "optional" hint from these
	 * values, so a field can never be labelled optional while the server rejects it
	 * blank — the trap this shape exists to prevent.
	 */
	needs: {
		/** The series being reported: a link or a bare id. */
		subject?: FieldNeed;
		/** A second series, on a duplicate report. */
		secondary?: FieldNeed;
		/** A free-text title, for a series we don't have an id for. */
		title?: FieldNeed;
		/** The username being reported. */
		user?: FieldNeed;
	};
}

/**
 * The four things only a reader can catch, plus a catch-all.
 *
 * These mirror the server's `ReportKind` exactly, and each `needs` entry mirrors the
 * per-kind check in `submitReport` (apps/server/src/graphql/mod.rs):
 *  - WRONG_MERGE / NEEDS_MERGE → `subjectId` must be present AND name a series we have;
 *  - MISSING_WORK             → `subjectTitle` must be present;
 *  - COMMENT_ABUSE            → `commentId` OR `reportedUsername` must be present, and
 *                               this form has no comment-id input, so the username is
 *                               the only thing that can satisfy it — hence 'required';
 *  - OTHER                    → nothing beyond `detail`.
 * The server is still the authority; this table only ensures the reader finds out
 * before submitting rather than after.
 */
export const REPORT_TOPICS: ReportTopic[] = [
	{
		kind: 'WRONG_MERGE',
		title: 'Two series merged into one',
		desc: 'One entry whose chapter list is actually two different stories.',
		heading: 'Which entry is mixing up two series?',
		prompt:
			'Paste the link to the entry, and tell us which chapters belong to the other series if you know.',
		needs: { subject: 'required' },
	},
	{
		kind: 'NEEDS_MERGE',
		title: 'The same series listed twice',
		desc: 'Duplicate entries for one series — usually under slightly different titles.',
		heading: 'Which entries are duplicates?',
		prompt: 'Paste the links to both entries. If you only have one, paste it and name the other.',
		needs: { subject: 'required', secondary: 'optional' },
	},
	{
		kind: 'MISSING_WORK',
		title: 'A series is missing',
		desc: "A series that isn't in the catalogue at all.",
		heading: 'What are we missing?',
		prompt:
			'Give the exact title (and the author, or a link to it elsewhere) so we can find the right one.',
		needs: { title: 'required' },
	},
	{
		kind: 'COMMENT_ABUSE',
		title: 'Someone in the comments',
		desc: 'Harassment, slurs, spam, or spoilers posted to hurt.',
		heading: 'What happened?',
		prompt:
			'We need the username to find the comment. Then tell us where it is and what they said.',
		promptAttached:
			"We have the comment itself. Tell us what's wrong with it — enough that a moderator who wasn't there can see it.",
		needs: { user: 'required', subject: 'optional' },
	},
	{
		kind: 'OTHER',
		title: 'Something else',
		desc: 'Wrong cover, wrong metadata, a chapter that will not load, anything we missed.',
		heading: "What's wrong?",
		prompt: 'Describe what you saw and where. A link helps.',
		needs: { subject: 'optional' },
	},
];

/**
 * Look up a topic by its wire kind; falls back to the catch-all.
 *
 * The fallback names OTHER explicitly rather than taking the last element: reordering
 * the list must not quietly change where an unrecognised `?kind=` lands, and OTHER is
 * the only kind the server accepts with no extra fields.
 */
export function topicFor(kind: ReportKind): ReportTopic {
	const other = REPORT_TOPICS.find((t) => t.kind === 'OTHER');
	return REPORT_TOPICS.find((t) => t.kind === kind) ?? other ?? REPORT_TOPICS[0];
}
