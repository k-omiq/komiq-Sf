#!/usr/bin/env node
/**
 * Fold Suwayomi-only duplicate works into their MangaDex-anchored counterparts.
 *
 * THE PROBLEM. A Suwayomi source lists a series under its ENGLISH title while the
 * MangaDex mirror carries the ROMANIZED one — "One-Punch Man" vs "One Punch-Man",
 * "JoJo's Bizarre Adventure Part 6" vs "JoJo no Kimyou na Bouken: Part 6". The ingest
 * matcher does not equate those, so each becomes its OWN canonical work: one anchored and
 * enriched, one Suwayomi-only. Measured on production 2026-07-29, ~1,900 works were
 * Suwayomi-only and ~1/3 of them were duplicates of this exact shape.
 *
 * WHAT THIS MERGES, AND WHY THAT PREDICATE. Only pairs where an alias's NORMALIZED title
 * AND the author string both match, and where the source maps to EXACTLY ONE target.
 * Title alone is not enough and must never be used: the dedup review queue is the
 * matcher's reject pile precisely because `title_exact` at score 0.60 is title-only with
 * zero corroboration. Verified on the same snapshot — "A Gift for You" matches two
 * unrelated works ("Dangsinege Bachineun Seonmul" by cho sangdeok and "Mienai Kimi e no
 * Okurimono" by satori tae), so a title-only pass would have destroyed one of them.
 * Author corroboration cuts 661 candidates to 411, and the single-target rule to 368.
 *
 * DIRECTION IS ALWAYS suwayomi-only -> MangaDex-anchored, never the reverse. The anchored
 * work owns the English chapter mirror, the cached covers and the enriched metadata;
 * `canonicalSeries` also rejects a work with no MangaDex anchor, so merging the other way
 * would produce a survivor the reader cannot open. The Suwayomi source mappings move onto
 * the target, so the merged work KEEPS every readable chapter (19,265 across the set).
 *
 * IRREVERSIBLE: `mergeWorks` deletes the source work. It goes through the GraphQL admin
 * mutation rather than touching SQLite directly so that `merge_works_ex` runs in full —
 * user data re-pointing, alias/external-id transfer, `work_redirect` so old links resolve,
 * and cover-blob reclaim from the separate covers DB.
 *
 * USAGE
 *   node scripts/merge-suwayomi-duplicates.mjs --plan merge_plan.json            # dry run
 *   node scripts/merge-suwayomi-duplicates.mjs --plan merge_plan.json --apply
 *
 *   KOMIKA_API    GraphQL endpoint   (default https://api.komiq.cc/graphql)
 *   KOMIKA_TOKEN  admin bearer token (required for --apply)
 *
 * RUN IT WHEN NO BULK INGEST IS IN FLIGHT. Merges are multi-table writes and an
 * "add all from this source" run is already saturating the write lock; the pool's
 * busy_timeout is 15 s, past which contention becomes SQLITE_BUSY rather than a wait.
 * Regenerate the plan first, too — an ingest in progress is still minting the very works
 * being matched, so a plan built before it finished is already stale.
 */

import { readFileSync, appendFileSync, existsSync, readdirSync } from 'node:fs';

const API = process.env.KOMIKA_API ?? 'https://api.komiq.cc/graphql';
const TOKEN = process.env.KOMIKA_TOKEN ?? '';
const args = process.argv.slice(2);
const APPLY = args.includes('--apply');
const planPath = args[args.indexOf('--plan') + 1] ?? 'merge_plan.json';
// Every applied merge is appended here immediately. This is what makes the run RESUMABLE:
// a crash, a 502 or a Ctrl-C mid-batch must not leave us guessing which of 368 destructive
// writes already landed, and a re-run must not re-attempt them.
const LOG = 'merge-suwayomi-duplicates.done.log';

/** Pace between merges. Not politeness — each merge is a multi-table write, and the
 *  scanner/ingest share this database. Serial + spaced keeps us inside busy_timeout. */
const DELAY_MS = 250;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function gql(query, variables) {
	const res = await fetch(API, {
		method: 'POST',
		headers: {
			'content-type': 'application/json',
			...(TOKEN ? { authorization: `Bearer ${TOKEN}` } : {}),
		},
		body: JSON.stringify({ query, variables }),
	});
	const json = await res.json().catch(() => null);
	if (!res.ok) throw new Error(`HTTP ${res.status}`);
	if (json?.errors?.length) throw new Error(json.errors[0].message);
	return json.data;
}

const MERGE = `mutation M($s: ID!, $t: ID!) {
  mergeWorks(sourceWorkId: $s, targetWorkId: $t) { targetWorkId movedSourceSeries }
}`;

function loadDone() {
	if (!existsSync(LOG)) return new Set();
	return new Set(
		readFileSync(LOG, 'utf8')
			.split('\n')
			.filter(Boolean)
			.map((l) => l.split('\t')[0]),
	);
}

async function main() {
	if (!existsSync(planPath)) {
		console.error(`plan not found: ${planPath}`);
		console.error(
			`(cwd has: ${readdirSync('.')
				.filter((f) => f.endsWith('.json'))
				.join(', ')})`,
		);
		process.exit(1);
	}
	const plan = JSON.parse(readFileSync(planPath, 'utf8'));
	const done = loadDone();
	const todo = plan.filter((p) => !done.has(p.source_work_id));

	console.log(`plan      : ${planPath} (${plan.length} pairs)`);
	console.log(`already   : ${done.size}`);
	console.log(`to merge  : ${todo.length}`);
	console.log(`endpoint  : ${API}`);
	console.log(`mode      : ${APPLY ? 'APPLY (destructive)' : 'DRY RUN'}\n`);

	if (APPLY && !TOKEN) {
		console.error('KOMIKA_TOKEN is required for --apply');
		process.exit(1);
	}

	// Re-assert the invariants here rather than trusting the plan file: it is generated by a
	// separate script against a live database, and a source that is ALSO someone's target
	// would merge into a row this same run is about to delete.
	const sources = new Set(plan.map((p) => p.source_work_id));
	const clash = plan.filter((p) => sources.has(p.target_work_id));
	if (clash.length) {
		console.error(`refusing: ${clash.length} pairs target a work that is itself a source`);
		process.exit(1);
	}

	let ok = 0;
	let failed = 0;
	for (const [i, p] of todo.entries()) {
		const label = `[${i + 1}/${todo.length}] ${p.source_title} -> ${p.target_title}`;
		if (!APPLY) {
			console.log(`DRY  ${label}`);
			continue;
		}
		try {
			const d = await gql(MERGE, { s: p.source_work_id, t: p.target_work_id });
			appendFileSync(LOG, `${p.source_work_id}\t${p.target_work_id}\t${p.source_title}\n`);
			ok++;
			console.log(`OK   ${label}  (+${d.mergeWorks.movedSourceSeries} mappings)`);
		} catch (e) {
			const msg = String(e.message ?? e);
			// A source that no longer exists was already merged — by an earlier run whose log
			// we lost, or by hand in the admin UI. Record it and move on; it is the goal state.
			if (/No such work|not found/i.test(msg)) {
				appendFileSync(LOG, `${p.source_work_id}\t${p.target_work_id}\tALREADY-GONE\n`);
				console.log(`SKIP ${label}  (source already gone)`);
				continue;
			}
			failed++;
			console.error(`FAIL ${label}\n     ${msg}`);
			// Contention means the database is busy, not that this pair is bad. Stop the run
			// rather than hammering 300 more writes into a lock we are already losing.
			if (/busy|locked|timeout/i.test(msg)) {
				console.error('\nDatabase is busy — stopping. Re-run later; progress is logged.');
				break;
			}
		}
		await sleep(DELAY_MS);
	}

	console.log(`\nmerged ${ok}, failed ${failed}, remaining ${todo.length - ok - failed}`);
	if (!APPLY) console.log('dry run only — nothing was written. Re-run with --apply.');
}

main().catch((e) => {
	console.error(e);
	process.exit(1);
});
