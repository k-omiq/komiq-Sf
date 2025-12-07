import assert from 'node:assert/strict';
import { mock, test } from 'node:test';
import {
	activeChips,
	activeFilterCount,
	applicableState,
	applyDraft,
	clearFilters,
	cycleTag,
	debounce,
	draftChanges,
	effectiveDir,
	effectiveSort,
	emptyState,
	filterSig,
	filtersOnlySig,
	legacyUnapplied,
	matchesFederated,
	MAX_LANGUAGES,
	MAX_TAGS,
	normalize,
	parseBrowseUrl,
	setTagState,
	shelveTags,
	sortOptions,
	tagLabel,
	tagSlug,
	tagsFromGenreFacets,
	tagState,
	toFilterInput,
	toggleDir,
	toggleIn,
	toLegacyFilters,
	toSearchParams,
	withField,
	withQuery,
	withSort,
	type BrowseState,
} from './browse-filters.ts';

const parse = (qs: string) => parseBrowseUrl(new URLSearchParams(qs));
const canon = (qs: string) => toSearchParams(parse(qs)).toString();
const noNames = () => undefined;

/** A state with every field set to a non-default value. */
function fullState(): BrowseState {
	return {
		q: 'one piece',
		sort: 'MOST_FOLLOWS',
		dir: 'ASC',
		includedTags: ['action', 'boys-love'],
		excludedTags: ['gore'],
		inclMode: 'OR',
		exclMode: 'AND',
		status: ['ONGOING', 'HIATUS'],
		types: ['MANHWA'],
		demographics: ['SEINEN', 'NONE'],
		contentRatings: ['SAFE', 'SUGGESTIVE'],
		languages: ['ja', 'zh-hk'],
		yearFrom: 2001,
		yearTo: 2020,
		minChapters: 50,
		minRating: 7.5,
		minFollows: 10000,
		hasChapters: true,
		page: 3,
	};
}

// ---- URL ⇄ state round trips --------------------------------------------------------

test('a fully-populated state round-trips through the URL unchanged', () => {
	const s = normalize(fullState());
	assert.deepEqual(parseBrowseUrl(toSearchParams(s)), s);
});

test('the empty state is the bare /browse URL — every default is written as absence', () => {
	assert.equal(toSearchParams(emptyState()).toString(), '');
	assert.deepEqual(parse(''), emptyState());
});

test('canonical URL: one fixed param order and sorted lists, whatever order they came in', () => {
	const a = canon('type=manhua&status=hiatus&tag=romance&q=x&status=ongoing&tag=action&page=2');
	const b = canon('page=2&tag=action&status=ongoing&q=x&tag=romance&type=manhua&status=hiatus');
	assert.equal(a, b);
	assert.equal(a, 'q=x&tag=action&tag=romance&status=ongoing&status=hiatus&type=manhua&page=2');
});

test('toSearchParams ∘ parseBrowseUrl is a fixed point', () => {
	for (const qs of [
		'',
		'q=naruto',
		'sort=title&dir=desc',
		'tag=action&tagmode=or&xtag=gore&xtagmode=and',
		'from=1990&to=2000&minch=10&minrating=8&minfollows=500&chapters=1',
		'genre=Action&type=Manga&status=ongoing&sort=trending&content=safe',
	]) {
		const once = canon(qs);
		assert.equal(toSearchParams(parse(once)).toString(), once, qs);
	}
});

test('defaults are omitted: AND include mode, OR exclude mode, DESC, page 1, chapters off', () => {
	assert.equal(canon('tag=action&tagmode=and'), 'tag=action');
	assert.equal(canon('xtag=gore&xtagmode=or'), 'xtag=gore');
	assert.equal(canon('sort=follows&dir=desc'), 'sort=follows');
	assert.equal(canon('sort=title&dir=asc'), 'sort=title');
	assert.equal(canon('page=1'), '');
	assert.equal(canon('chapters=0'), '');
	// A mode with no tags to qualify is meaningless and dropped.
	assert.equal(canon('tagmode=or&xtagmode=and'), '');
});

test('filterSig ignores the page; sameness of filters is page-independent', () => {
	assert.equal(filterSig(parse('tag=action&page=4')), filterSig(parse('tag=action')));
	assert.notEqual(filterSig(parse('tag=action')), filterSig(parse('tag=romance')));
});

test('the panel draft key ignores query, sort and page', () => {
	assert.equal(
		filtersOnlySig(parse('q=a&sort=title&page=3&tag=x')),
		filtersOnlySig(parse('tag=x')),
	);
	assert.notEqual(filtersOnlySig(parse('tag=x')), filtersOnlySig(parse('tag=x&tagmode=or')));
});

test('applying a draft keeps the current query and sort, takes the draft filters, page 1', () => {
	const applied = parse('q=old&sort=follows&page=4&status=ongoing');
	const draft = parse('q=stale&sort=title&type=manga');
	const out = applyDraft(applied, draft, 'new');
	assert.equal(toSearchParams(out).toString(), 'q=new&sort=follows&type=manga');
});

test('draftChanges counts added, removed and changed clauses', () => {
	const applied = parse('tag=a&tag=b&status=ongoing&from=2000');
	assert.equal(draftChanges(applied, applied), 0);
	assert.equal(draftChanges(applied, parse('tag=a&tag=b&status=ongoing&from=2000&type=manga')), 1);
	assert.equal(draftChanges(applied, parse('tag=a&status=ongoing&from=2000')), 1);
	// A changed year range is one chip removed + one added.
	assert.equal(draftChanges(applied, parse('tag=a&tag=b&status=ongoing&from=2001')), 2);
	assert.equal(draftChanges(applied, parse('tag=a&tag=b&tagmode=or&status=ongoing&from=2000')), 1);
});

// ---- tolerant parsing ---------------------------------------------------------------

test('invalid values are dropped, not rejected', () => {
	const s = parse(
		'sort=bogus&dir=sideways&status=alive&type=novel&demo=kids&content=extreme&lang=Japanese!&from=abc&minch=-5&minrating=x&page=-2&tag=Not%20A%20Slug&xtag=%3Cscript%3E',
	);
	assert.deepEqual(s, emptyState());
});

test('enum values are case-insensitive and deduped', () => {
	const s = parse('status=ONGOING&status=ongoing&type=Manhwa&type=MANHWA&demo=Seinen');
	assert.deepEqual(s.status, ['ONGOING']);
	assert.deepEqual(s.types, ['MANHWA']);
	assert.deepEqual(s.demographics, ['SEINEN']);
});

test('WEBTOON and COMIC fold onto the formats the server folds them to', () => {
	assert.deepEqual(parse('type=webtoon&type=comic').types, ['MANGA', 'MANHWA']);
});

test('numbers are clamped into range', () => {
	const s = parse('from=1066&to=3000&minch=1e9&minrating=11&minfollows=99999999999');
	assert.equal(s.yearFrom, 1900);
	assert.equal(s.yearTo, 2100);
	assert.equal(s.minChapters, 100_000);
	assert.equal(s.minRating, 10);
	assert.equal(s.minFollows, 10_000_000);
});

test('a reversed year range is swapped, integers are floored, ratings keep one decimal', () => {
	const s = parse('from=2020&to=2001&minch=12.9&minrating=7.25');
	assert.equal(s.yearFrom, 2001);
	assert.equal(s.yearTo, 2020);
	assert.equal(s.minChapters, 12);
	assert.equal(s.minRating, 7.3);
});

test('zero lower bounds mean "no minimum" and are not written', () => {
	assert.equal(canon('minch=0&minrating=0&minfollows=0'), '');
});

test('list lengths are bounded', () => {
	const many = Array.from({ length: 50 }, (_, i) => `tag=t${String(i).padStart(2, '0')}`).join('&');
	assert.equal(parse(many).includedTags.length, MAX_TAGS);
	const langs = Array.from(
		{ length: 30 },
		(_, i) => `lang=l${String.fromCharCode(97 + (i % 26))}${i}`,
	);
	// Only well-formed codes survive, and at most MAX_LANGUAGES of them.
	assert.ok(parse(langs.join('&')).languages.length <= MAX_LANGUAGES);
	const okLangs = Array.from({ length: 20 }, (_, i) => `lang=a${String.fromCharCode(97 + i)}`);
	assert.equal(parse(okLangs.join('&')).languages.length, MAX_LANGUAGES);
});

test('a tag in both lists stays included only', () => {
	const s = parse('tag=action&xtag=action&xtag=gore');
	assert.deepEqual(s.includedTags, ['action']);
	assert.deepEqual(s.excludedTags, ['gore']);
});

test('the query is trimmed and capped', () => {
	assert.equal(parse('q=%20%20naruto%20').q, 'naruto');
	assert.equal(parse(`q=${'x'.repeat(500)}`).q.length, 200);
});

// ---- legacy params -------------------------------------------------------------------

test('legacy genre links (home page) map onto tag ids', () => {
	const s = parse(`genre=${encodeURIComponent("Boys' Love")}&genre=Sci-Fi&genre=Action`);
	assert.deepEqual(s.includedTags, ['action', 'boys-love', 'sci-fi']);
	assert.equal(canon('genre=Action'), 'tag=action');
});

test('legacy sort words map onto the new sorts', () => {
	assert.equal(parse('sort=trending').sort, 'MOST_VIEWED');
	assert.equal(parse('sort=newest').sort, null); // LATEST_UPDATE is the no-query default
	assert.equal(parse('q=x&sort=newest').sort, 'LATEST_UPDATE');
	assert.equal(parse('sort=rating').sort, 'HIGHEST_RATED');
	assert.equal(parse('sort=added').sort, 'RECENTLY_ADDED');
	assert.equal(parse('sort=chapters').sort, 'MOST_CHAPTERS');
});

test('legacy capitalised type, single status, content tiers, chapters and minRating', () => {
	const s = parse(
		'type=Manga&type=Manhua&status=completed&content=nsfw_only&chapters=1&minRating=6',
	);
	assert.deepEqual(s.types, ['MANGA', 'MANHUA']);
	assert.deepEqual(s.status, ['COMPLETED']);
	assert.deepEqual(s.contentRatings, ['EROTICA', 'PORNOGRAPHIC']);
	assert.equal(s.hasChapters, true);
	assert.equal(s.minRating, 6);
	assert.deepEqual(parse('content=safe').contentRatings, ['SAFE']);
	assert.deepEqual(parse('content=all').contentRatings, []);
});

test('tagSlug matches the contract examples', () => {
	assert.equal(tagSlug("Boys' Love"), 'boys-love');
	assert.equal(tagSlug('Sci-Fi'), 'sci-fi');
	assert.equal(tagSlug('4-Koma'), '4-koma');
	assert.equal(tagSlug('  Post-Apocalyptic '), 'post-apocalyptic');
	assert.equal(tagSlug('Isekai (異世界)'), 'isekai');
});

test('tagSlug follows the server rule for accents, inner apostrophes and non-ASCII', () => {
	// The SAME fixtures as `tags::tests::slug_matches_the_reader_on_shared_fixtures`: the
	// id is compared against `work_tag.tag_key`, which the server's `slug` writes.
	assert.equal(tagSlug('Pokémon'), 'pok-mon');
	assert.equal(tagSlug("Doesn't Matter"), 'doesn-t-matter');
	assert.equal(tagSlug('Shōnen Ai'), 'sh-nen-ai');
	assert.equal(tagSlug('İdol'), 'dol');
	// No ASCII alphanumerics: the server's hash id, so a legacy link to such a tag keeps
	// filtering on the id it is stored under. Value pinned on BOTH sides.
	assert.equal(tagSlug('恋愛'), 'tag-08457119f4ed');
	assert.equal(tagSlug('  恋愛 '), 'tag-08457119f4ed');
	assert.equal(tagSlug('   '), '');
});

// ---- sort -------------------------------------------------------------------------------

test('default sort is Relevance with a query and Latest update without', () => {
	assert.equal(effectiveSort(parse('q=x')), 'RELEVANCE');
	assert.equal(effectiveSort(parse('')), 'LATEST_UPDATE');
});

test('Relevance is only offered with a query', () => {
	assert.ok(sortOptions('naruto').includes('RELEVANCE'));
	assert.ok(!sortOptions('').includes('RELEVANCE'));
	assert.ok(!sortOptions('   ').includes('RELEVANCE'));
});

test('clearing the query while sorted by Relevance falls back to Latest update', () => {
	const s = withSort(parse('q=naruto&sort=follows'), 'RELEVANCE');
	assert.equal(s.sort, null); // Relevance IS the default with a query
	const cleared = withQuery(s, '');
	assert.equal(effectiveSort(cleared), 'LATEST_UPDATE');
	assert.equal(toSearchParams(cleared).toString(), '');
	// A hand-written ?sort=relevance with no query lands on the fallback too.
	assert.equal(effectiveSort(parse('sort=relevance')), 'LATEST_UPDATE');
	assert.equal(canon('sort=relevance'), '');
});

test('an explicit sort survives typing a query; the implicit default does not', () => {
	assert.equal(effectiveSort(withQuery(parse('sort=follows'), 'x')), 'MOST_FOLLOWS');
	assert.equal(effectiveSort(withQuery(parse(''), 'x')), 'RELEVANCE');
	// Latest update chosen with a query is NOT the default there, so it is kept.
	assert.equal(canon('q=x&sort=latest'), 'q=x&sort=latest');
});

test('Title defaults to ascending; a new sort resets the direction', () => {
	assert.equal(effectiveDir(parse('sort=title')), 'ASC');
	assert.equal(effectiveDir(parse('sort=follows')), 'DESC');
	const zToA = toggleDir(parse('sort=title'));
	assert.equal(canon(toSearchParams(zToA).toString()), 'sort=title&dir=desc');
	assert.equal(effectiveDir(withSort(zToA, 'MOST_FOLLOWS')), 'DESC');
});

test('Relevance has no direction: ?dir=asc is ignored and not sent', () => {
	const s = parse('q=x&dir=asc');
	assert.equal(effectiveDir(s), 'DESC');
	assert.equal(toSearchParams(s).toString(), 'q=x');
	assert.equal(toFilterInput(s).direction, undefined);
});

// ---- tri-state tags -----------------------------------------------------------------------

test('a tag cycles off → include → exclude → off', () => {
	let s = emptyState();
	assert.equal(tagState(s, 'action'), 'off');
	s = cycleTag(s, 'action');
	assert.equal(tagState(s, 'action'), 'include');
	s = cycleTag(s, 'action');
	assert.equal(tagState(s, 'action'), 'exclude');
	assert.deepEqual(s.includedTags, []);
	s = cycleTag(s, 'action');
	assert.equal(tagState(s, 'action'), 'off');
	assert.deepEqual(s, emptyState());
});

test('cycling skips a full list rather than doing nothing', () => {
	const full = Array.from({ length: MAX_TAGS }, (_, i) => `t${String(i).padStart(2, '0')}`);
	const s = normalize({ ...emptyState(), includedTags: full });
	assert.equal(tagState(cycleTag(s, 'extra'), 'extra'), 'exclude');
	assert.equal(setTagState(s, 'extra', 'include'), s);
});

test('tag changes reset the page', () => {
	assert.equal(cycleTag(parse('page=5'), 'action').page, 1);
});

// ---- chips ----------------------------------------------------------------------------------

test('every active filter has a chip, with unique keys', () => {
	const chips = activeChips(fullState(), (id) => (id === 'boys-love' ? "Boys' Love" : undefined));
	const keys = chips.map((c) => c.key);
	assert.equal(new Set(keys).size, keys.length);
	assert.deepEqual(
		chips.map((c) => c.label),
		[
			'Action',
			"Boys' Love",
			'Gore',
			'Ongoing',
			'Hiatus',
			'Manhwa',
			'Seinen',
			'No demographic',
			'Safe',
			'Suggestive',
			'Japanese',
			'Cantonese',
			'2001–2020',
			'50+ chapters',
			'Rated 7.5+',
			'10,000+ follows',
			'Readable now',
		],
	);
	assert.equal(chips.find((c) => c.key === 'tag-:gore')?.tone, 'exclude');
	assert.equal(activeFilterCount(fullState()), chips.length);
});

test('removing a chip removes exactly that filter and resets the page', () => {
	const s = normalize(fullState());
	for (const chip of activeChips(s, noNames)) {
		const next = chip.remove();
		assert.equal(next.page, 1, chip.key);
		const remaining = activeChips(next, noNames).map((c) => c.key);
		assert.ok(!remaining.includes(chip.key), chip.key);
		assert.equal(remaining.length, activeChips(s, noNames).length - 1, chip.key);
		// The query and the sort are not filters and survive.
		assert.equal(next.q, s.q);
		assert.equal(next.sort, s.sort);
	}
});

test('year chip wording', () => {
	const label = (qs: string) => activeChips(parse(qs), noNames).map((c) => c.label);
	assert.deepEqual(label('from=2010'), ['From 2010']);
	assert.deepEqual(label('to=1999'), ['Until 1999']);
	assert.deepEqual(label('from=2005&to=2005'), ['Year 2005']);
});

test('an unknown tag id still gets a readable chip', () => {
	assert.equal(tagLabel('slice-of-life', noNames), 'Slice Of Life');
	assert.equal(
		tagLabel('slice-of-life', () => 'Slice of Life'),
		'Slice of Life',
	);
});

test('clear all keeps the query and sort', () => {
	const s = clearFilters(normalize(fullState()));
	assert.equal(s.q, 'one piece');
	assert.equal(s.sort, 'MOST_FOLLOWS');
	assert.equal(activeChips(s, noNames).length, 0);
	assert.equal(s.page, 1);
});

// ---- applicability -------------------------------------------------------------------------

test('without the NSFW opt-in only Safe/Suggestive apply', () => {
	const s = parse('content=safe&content=erotica&content=pornographic');
	assert.deepEqual(applicableState(s, { nsfw: false, tagSource: 'tags' }).contentRatings, ['SAFE']);
	assert.deepEqual(applicableState(s, { nsfw: true, tagSource: 'tags' }).contentRatings, [
		'SAFE',
		'EROTICA',
		'PORNOGRAPHIC',
	]);
});

test('the legacy genre fallback is include-only and matches any', () => {
	const s = parse('tag=action&tag=drama&xtag=gore');
	const a = applicableState(s, { nsfw: false, tagSource: 'genres' });
	assert.deepEqual(a.excludedTags, []);
	assert.equal(a.inclMode, 'OR');
	assert.deepEqual(a.includedTags, ['action', 'drama']);
});

// ---- wire ---------------------------------------------------------------------------------------

test('toFilterInput sends only set clauses, never empty lists', () => {
	assert.deepEqual(toFilterInput(emptyState()), { sort: 'LATEST_UPDATE', direction: 'DESC' });
	assert.deepEqual(toFilterInput(parse('q=x')), { sort: 'RELEVANCE' });
	assert.deepEqual(toFilterInput(normalize(fullState())), {
		includedTags: ['action', 'boys-love'],
		includedTagsMode: 'OR',
		excludedTags: ['gore'],
		excludedTagsMode: 'AND',
		status: ['ONGOING', 'HIATUS'],
		types: ['MANHWA'],
		demographics: ['SEINEN', 'NONE'],
		contentRatings: ['SAFE', 'SUGGESTIVE'],
		originalLanguages: ['ja', 'zh-hk'],
		yearFrom: 2001,
		yearTo: 2020,
		minChapters: 50,
		minRating: 7.5,
		minFollows: 10000,
		hasChapters: true,
		sort: 'MOST_FOLLOWS',
		direction: 'ASC',
	});
});

test('legacy arguments carry only what they can express', () => {
	const names = (id: string) => ({ action: 'Action', 'boys-love': "Boys' Love" })[id] ?? id;
	assert.deepEqual(toLegacyFilters(normalize(fullState()), names), {
		genres: ['Action', "Boys' Love"],
		types: ['MANHWA'],
		// Two statuses: the legacy argument holds one, so it is omitted (wider, not wrong).
		// MOST_FOLLOWS has no legacy sort; {safe, suggestive} is the SUGGESTIVE ceiling.
		contentRating: 'SUGGESTIVE',
		hasChapters: true,
	});
	assert.deepEqual(
		toLegacyFilters(
			parse('status=ongoing&sort=rating&content=erotica&content=pornographic'),
			names,
		),
		{
			status: 'ONGOING',
			sort: 'RATING',
			contentRating: 'NSFW_ONLY',
		},
	);
	// An ascending order can't be said with the legacy sort at all.
	assert.equal(toLegacyFilters(parse('sort=rating&dir=asc'), names).sort, undefined);
	// Neither can a non-prefix rating set.
	assert.equal(toLegacyFilters(parse('content=suggestive'), names).contentRating, undefined);
	// The legacy genre list is capped at 12.
	const many = Array.from({ length: 20 }, (_, i) => `tag=g${String(i).padStart(2, '0')}`).join('&');
	assert.equal(toLegacyFilters(parse(many), names).genres?.length, 12);
});

test('legacyUnapplied names what an old server ignored', () => {
	assert.deepEqual(legacyUnapplied(emptyState()), []);
	assert.deepEqual(legacyUnapplied(parse('tag=action&tagmode=or&type=manga&sort=rating')), []);
	assert.deepEqual(legacyUnapplied(parse('tag=a&tag=b')), ['Match all tags']);
	assert.deepEqual(legacyUnapplied(parse('q=x&type=manga&sort=follows')), ['Format', 'Sort']);
	assert.deepEqual(legacyUnapplied(parse('xtag=gore&lang=ja&from=2000&minfollows=5')), [
		'Excluded tags',
		'Original language',
		'Year',
		'Min follows',
	]);
});

// ---- picker -------------------------------------------------------------------------------------

const TAGS = [
	{ id: 'romance', name: 'Romance', group: 'GENRE', count: 10 },
	{ id: 'action', name: 'Action', group: 'GENRE', count: 30 },
	{ id: 'isekai', name: 'Isekai', group: 'THEME', count: 5 },
	{ id: 'gore', name: 'Gore', group: 'CONTENT', count: 2 },
	{ id: 'long-strip', name: 'Long Strip', group: 'FORMAT', count: 7 },
] as const;

test('tags shelve by group in fixed order, A–Z within a group', () => {
	const shelves = shelveTags([...TAGS], '', emptyState());
	assert.deepEqual(
		shelves.map((s) => [s.group, s.tags.map((t) => t.id)]),
		[
			['GENRE', ['action', 'romance']],
			['THEME', ['isekai']],
			['FORMAT', ['long-strip']],
			['CONTENT', ['gore']],
		],
	);
});

test('type-to-search narrows the shelves but keeps selected tags visible', () => {
	const s = parse('xtag=gore');
	const shelves = shelveTags([...TAGS], 'ROM', s);
	assert.deepEqual(
		shelves.map((sh) => sh.tags.map((t) => t.id)),
		[['romance'], ['gore']],
	);
	assert.deepEqual(shelveTags([...TAGS], 'zzz', emptyState()), []);
});

test('genre facets become GENRE tags with slug ids, first spelling wins', () => {
	assert.deepEqual(
		tagsFromGenreFacets([
			{ genre: 'Sci-Fi', count: 9 },
			{ genre: 'sci fi', count: 1 },
			{ genre: '', count: 3 },
		]),
		[{ id: 'sci-fi', name: 'Sci-Fi', group: 'GENRE', count: 9 }],
	);
});

// ---- federated --------------------------------------------------------------------------------

test('federated rows honour tags (by name), format and status client-side', () => {
	const row = { genres: ['Action', "Boys' Love"], type: 'Manhwa', status: 'ongoing' };
	assert.ok(matchesFederated(row, parse('tag=action&tag=boys-love'), noNames));
	assert.ok(!matchesFederated(row, parse('tag=action&tag=drama'), noNames));
	assert.ok(matchesFederated(row, parse('tag=action&tag=drama&tagmode=or'), noNames));
	assert.ok(!matchesFederated(row, parse('xtag=boys-love'), noNames));
	assert.ok(matchesFederated(row, parse('xtag=boys-love&xtag=gore&xtagmode=and'), noNames));
	assert.ok(!matchesFederated(row, parse('type=manga'), noNames));
	assert.ok(!matchesFederated(row, parse('status=completed'), noNames));
	assert.ok(matchesFederated(row, parse('type=manhwa&status=ongoing'), noNames));
});

// ---- small helpers ---------------------------------------------------------------------------

test('toggleIn adds and removes, withField sets scalars, both reset the page', () => {
	let s = parse('page=3');
	s = toggleIn(s, 'languages', 'ko');
	assert.deepEqual(s.languages, ['ko']);
	assert.equal(s.page, 1);
	s = toggleIn(s, 'languages', 'ko');
	assert.deepEqual(s.languages, []);
	assert.equal(withField(parse('page=2'), 'minRating', 8).minRating, 8);
	assert.equal(withField(parse('page=2'), 'minRating', 8).page, 1);
});

test('debounce runs once, trailing, with the last arguments; cancel and flush work', () => {
	mock.timers.enable({ apis: ['setTimeout'] });
	try {
		const calls: string[] = [];
		const d = debounce((v: string) => calls.push(v), 300);
		d('a');
		d('ab');
		mock.timers.tick(299);
		assert.deepEqual(calls, []);
		assert.equal(d.pending, true);
		mock.timers.tick(1);
		assert.deepEqual(calls, ['ab']);
		assert.equal(d.pending, false);

		d('x');
		d.cancel();
		mock.timers.tick(1000);
		assert.deepEqual(calls, ['ab']);

		d('y');
		d.flush();
		assert.deepEqual(calls, ['ab', 'y']);
		d.flush(); // nothing pending: no-op
		mock.timers.tick(1000);
		assert.deepEqual(calls, ['ab', 'y']);
	} finally {
		mock.timers.reset();
	}
});
