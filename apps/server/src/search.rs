//! PostgreSQL search contract: database folding, per-term work intersection and
//! exact/prefix ranking before word and typo candidates. All client input is bound.

pub(crate) fn valid_query(raw: &str) -> bool {
    if raw.len() > crate::catalog::MAX_FTS_QUERY_CHARS * 4
        || raw.chars().count() > crate::catalog::MAX_FTS_QUERY_CHARS
    {
        return false;
    }
    let terms: std::collections::HashSet<_> = raw
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect();
    !terms.is_empty() && terms.len() <= 12
}

/// `$1` is always the raw query. Both Browse and catalogue search consume `m`:
/// the eligible matches with their tier and rank, plus every column of the
/// caller's eligibility SELECT. That SELECT returns one row for a work, names
/// its `work_id` and reads the work row as `w`; its WHERE is passed apart.
/// A term can match a different title/alias row from every other term. Exact and
/// prefix index probes recheck the complete folded string after the bounded key.
///
/// Eligibility is a primary-key probe: one for each ranked work, and for each
/// term that asks whether it has an eligible word hit, as many as it takes to
/// find the first. No statement joins the catalogue. The terms intersect before the
/// probe; that gives the same works as intersecting eligible hits, because a
/// work is eligible or not for all of its hits at once. A term that falls back
/// to typo hits may still carry word hits of ineligible works, and the probe
/// drops those works. Each `OFFSET 0` keeps its subquery a probe: the planner
/// cannot estimate a term's hits and may otherwise join the whole catalogue.
///
/// The prefix probe is a range on the bounded key in every case. A folded query
/// longer than the key starts with its own lower bound, so `starts_with` alone
/// decides, as it did under an equality on the key.
///
/// A term in typo fallback reads one of two sets of rows. With more than
/// 10,000 eligible works the trigram index finds them in the whole catalogue,
/// and the final probe drops the rows of ineligible works. With fewer, only the
/// rows of the eligible works are read: a narrow filter then costs its own
/// size, where the index would return up to 160,000 rows for a short term and
/// each must be rechecked. Both give an eligible work the same hits. 10,000 is
/// where the second way costs what the first costs for the cheapest term on a
/// catalogue of 120,000 works. Each side hangs from the term by `LATERAL`, so
/// nothing of it runs for a query with no term in fallback.
///
/// A typo candidate row is split on single spaces; an empty piece is never a
/// typo match. `search_typo_close` runs only for a word that can pass it: its
/// edit budget bounds the difference in length, the characters of the word
/// that the term lacks and the characters of the term that the word lacks.
/// The length is tested first, alone, because the other two cost the length of
/// the term for each word. A term of more than 256 characters has no typo
/// probe at all: the function accepts no word for it.
pub(crate) const MATCH_CTES: &str = r#"
eligible AS NOT MATERIALIZED (
 __ELIGIBLE_SELECT__ WHERE (__ELIGIBLE_WHERE__)
 AND COALESCE(w.title_override,w.primary_title,'') <> ''
 AND EXISTS (SELECT 1 FROM source_series ss WHERE ss.work_id=w.id)
),
q AS MATERIALIZED (SELECT public.search_fold($1) AS folded),
parts AS MATERIALIZED (
 SELECT term, min(ord) AS ord, max(ord) AS last_ord, count(*) AS repeats
 FROM q, regexp_split_to_table(q.folded, ' +') WITH ORDINALITY p(term, ord)
 WHERE term <> '' GROUP BY term
),
terms AS MATERIALIZED (
 SELECT p.ord, lexeme,
        p.last_ord = (SELECT max(last_ord) FROM parts) AND p.repeats = 1 AS prefix
 FROM parts p, unnest(tsvector_to_array(to_tsvector('pg_catalog.simple', p.term))) lexeme
),
word_hits AS NOT MATERIALIZED (
 SELECT s.work_id, t.ord
 FROM terms t JOIN public.work_search_alias s
 ON s.doc @@ to_tsquery('pg_catalog.simple', quote_literal(t.lexeme) || CASE WHEN t.prefix THEN ':*' ELSE '' END)
),
typo_works AS MATERIALIZED (SELECT work_id FROM eligible LIMIT 10001),
typo_parts AS MATERIALIZED (
 SELECT p.ord, p.term, public.search_trigram_key(p.term) AS key,
        (SELECT count(*) FROM typo_works) <= 10000 AS narrow
 FROM parts p
 WHERE length(p.term) BETWEEN 3 AND 256
 AND NOT EXISTS (SELECT 1 FROM word_hits h WHERE h.ord=p.ord
                 AND EXISTS (SELECT 1 FROM eligible e WHERE e.work_id=h.work_id OFFSET 0)
                 OFFSET 0)
),
typo_rows AS (
 SELECT p.ord, p.term, p.key, s.work_id, s.trigram_key, s.normalized_title
 FROM typo_parts p
 CROSS JOIN LATERAL (SELECT work_id, trigram_key, normalized_title
                     FROM public.work_search_alias any_row
                     WHERE NOT p.narrow AND any_row.trigram_key OPERATOR(public.%>) p.key
                     OFFSET 0) s
 UNION ALL
 SELECT p.ord, p.term, p.key, s.work_id, s.trigram_key, s.normalized_title
 FROM typo_parts p
 CROSS JOIN LATERAL (SELECT work_id FROM typo_works WHERE p.narrow OFFSET 0) e
 CROSS JOIN LATERAL (SELECT work_id, trigram_key, normalized_title
                     FROM public.work_search_alias own_row
                     WHERE own_row.work_id=e.work_id OFFSET 0) s
 WHERE s.trigram_key OPERATOR(public.%>) p.key
),
fuzzy_hits AS (
 SELECT r.work_id, r.ord, 3::bigint AS tier,
        -max(public.word_similarity(r.key, r.trigram_key))::double precision AS rank
 FROM typo_rows r
 WHERE EXISTS (SELECT 1 FROM string_to_table(r.normalized_title, ' ') word
               WHERE abs(length(word) - length(r.term)) <= 2
               AND greatest(abs(length(word) - length(r.term)),
                            length(translate(word, r.term, '')),
                            length(translate(r.term, word, '')))
                   <= CASE WHEN least(length(word), length(r.term)) <= 5 THEN 1 ELSE 2 END
               AND public.search_typo_close(r.term, word))
 GROUP BY r.work_id, r.ord
),
term_hits AS (
 SELECT work_id, ord, min(tier) AS tier, min(rank) AS rank
 FROM (SELECT work_id, ord, 2::bigint AS tier, -1.0::double precision AS rank FROM word_hits
       UNION ALL SELECT * FROM fuzzy_hits) hits
 GROUP BY work_id, ord
),
intersected_works AS (
 SELECT work_id, max(tier) AS tier, avg(rank)::double precision AS rank
 FROM term_hits GROUP BY work_id
 HAVING count(*) = (SELECT count(*) FROM parts)

),
exact_works AS (
 SELECT s.work_id, 0::bigint AS tier, 0.0::double precision AS rank
 FROM q JOIN public.work_search_alias s
 ON left(s.normalized_title,256) = left(q.folded,256)
 AND s.normalized_title = q.folded
),
prefix_works AS (
 SELECT s.work_id, 1::bigint AS tier, 0.0::double precision AS rank
 FROM q JOIN public.work_search_alias s
 ON left(s.normalized_title,256) >= left(q.folded,256)
 AND left(s.normalized_title,256) < left(q.folded,256) || chr(1114111)
 AND starts_with(s.normalized_title, q.folded)
 WHERE q.folded <> ''
),
matched AS (
 SELECT * FROM exact_works UNION ALL SELECT * FROM prefix_works
 UNION ALL SELECT * FROM intersected_works
),
ranked AS NOT MATERIALIZED (
 SELECT work_id, min(tier) AS tier, min(rank) AS rank FROM matched GROUP BY work_id
),
m AS (
 SELECT r.tier, r.rank, e.* FROM ranked r
 CROSS JOIN LATERAL (SELECT * FROM eligible WHERE work_id=r.work_id OFFSET 0) e
)
"#;

/// `cand`: the `__WINDOW__` best rows of `m`, each with its chapter count. The
/// ranked works arrive in window order and are probed one at a time, so the cut
/// costs the window and not the match set, and `chapter` is read for the rows
/// inside the window only.
const WINDOW_CTE: &str = r#"
cand AS (
 SELECT top.*,
        COALESCE((SELECT count(*) FROM chapter c JOIN source_series ss ON ss.id=c.source_series_id
                  WHERE ss.work_id=top.work_id),0) AS chapters
 FROM (SELECT r.tier, r.rank, e.*
       FROM (SELECT * FROM ranked ORDER BY tier, rank, work_id) r
       CROSS JOIN LATERAL (SELECT * FROM eligible WHERE work_id=r.work_id OFFSET 0) e
       ORDER BY r.tier, r.rank, r.work_id LIMIT __WINDOW__) top
)
"#;

/// The eligibility SELECT and its WHERE are assembled solely from internal SQL
/// fragments and validated filters, with client query/filter values bound by
/// the caller. A term uses eligible word/prefix hits whenever present, and fuzzy
/// hits only when it has none; hidden/shell/filter-excluded works cannot
/// suppress recall.
pub(crate) fn match_ctes(eligible_select: &str, eligible_where: &str) -> String {
    MATCH_CTES
        .replace("__ELIGIBLE_SELECT__", eligible_select)
        .replace("__ELIGIBLE_WHERE__", eligible_where)
}

/// Follows [`match_ctes`] in the same `WITH`. `window` is the placeholder of the
/// bound window size.
pub(crate) fn window_cte(window: &str) -> String {
    WINDOW_CTE.replace("__WINDOW__", window)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::search_works_fts;
    use crate::database::Database;

    async fn pool() -> Database {
        let pool = Database::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations_pg")
            .run(pool.write())
            .await
            .unwrap();
        pool
    }

    /// One searchable work: a title, a source and `chapters` chapters.
    async fn work(pool: &Database, id: &str, title: &str, chapters: i32) {
        sqlx::query("INSERT INTO work(id,primary_title,created_at,updated_at) VALUES($1,$2,'2020-01-01T00:00:00Z','2020-01-01T00:00:00Z')")
            .bind(id).bind(title).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO source_series(id,work_id,source_type,source_key,created_at) VALUES($1 || ':s',$1,'mangadex',$1 || ':k','2020-01-01T00:00:00Z')")
            .bind(id).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO chapter(id,source_series_id,external_id,created_at) SELECT $1 || ':c' || n, $1 || ':s', $1 || ':c' || n, '2020-01-01T00:00:00Z' FROM generate_series(1,$2) n")
            .bind(id).bind(chapters).execute(pool).await.unwrap();
    }

    async fn found(pool: &Database, query: &str) -> Vec<String> {
        assert!(valid_query(query), "the server must accept {query:?}");
        let (total, ids) = search_works_fts(pool, query, true, &Default::default(), 1, 30)
            .await
            .unwrap();
        assert_eq!(total as usize, ids.len());
        ids
    }

    fn nodes<'a>(plan: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
        match plan {
            serde_json::Value::Array(items) => items.iter().for_each(|item| nodes(item, out)),
            serde_json::Value::Object(fields) => {
                if fields.contains_key("Node Type") {
                    out.push(plan);
                }
                fields.values().for_each(|value| nodes(value, out));
            }
            _ => {}
        }
    }

    /// The prefix tier decides the order here: `inner` holds the long word and has
    /// the most chapters, so it leads whenever a title that starts with the query
    /// is not recognised as a prefix match. The ligature folds to two characters,
    /// which is how a query within the 256-character input limit folds past the key.
    #[tokio::test]
    async fn prefix_tier_holds_below_at_and_past_the_bounded_key() {
        let pool = pool().await;
        work(&pool, "long", &"fi".repeat(150), 0).await;
        // The key itself, and the key followed by a different character.
        work(&pool, "edge", &"fi".repeat(128), 0).await;
        work(&pool, "fork", &format!("{}zz", "fi".repeat(128)), 0).await;
        work(&pool, "inner", &format!("zz {}", "fi".repeat(150)), 5).await;
        for (folded, query, expected) in [
            (
                255,
                format!("{}f", "ﬁ".repeat(127)),
                vec!["edge", "fork", "long", "inner"],
            ),
            // `edge` is exact here and leads for that reason.
            (256, "ﬁ".repeat(128), vec!["edge", "fork", "long", "inner"]),
            (257, format!("{}f", "ﬁ".repeat(128)), vec!["long", "inner"]),
            (258, "ﬁ".repeat(129), vec!["long", "inner"]),
            // The whole title: exact first.
            (300, "ﬁ".repeat(150), vec!["long", "inner"]),
        ] {
            let length: i32 = sqlx::query_scalar("SELECT length(public.search_fold($1))")
                .bind(&query)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(length, folded);
            assert_eq!(
                found(&pool, &query).await,
                expected,
                "folded length {folded}"
            );
        }
        pool.close().await;
    }

    /// The range ends at the prefix followed by the last code point, so whatever
    /// follows the prefix in a title stays inside it: a space (the lowest folded
    /// character), a digit, a letter, and letters outside ASCII and outside the
    /// basic plane. `decoy` has the most chapters and only word hits.
    #[tokio::test]
    async fn prefix_tier_admits_every_character_after_the_prefix() {
        let pool = pool().await;
        work(&pool, "decoy", "zeta alpha", 9).await;
        work(&pool, "p0", "alpha", 0).await;
        let after = [
            "alpha beta",
            "alpha0",
            "alphaz",
            "alpha龥",
            "alpha\u{20000}",
        ];
        for (n, title) in after.iter().enumerate() {
            work(&pool, &format!("p{}", n + 1), title, 0).await;
        }
        // Next to the range on either side, and not a match of any kind.
        work(&pool, "below", "alph", 0).await;
        work(&pool, "above", "alpi", 0).await;
        let kept: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM work_search_alias WHERE starts_with(normalized_title, 'alpha') \
             AND length(normalized_title) = 6 AND work_id LIKE 'p%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(kept, 4, "each one-character suffix survives the fold");
        assert_eq!(
            found(&pool, "alpha").await,
            ["p0", "p1", "p2", "p3", "p4", "p5", "decoy"]
        );
        pool.close().await;
    }

    /// U+0345 is a letter to the server's validity check and a combining mark to
    /// the fold, which removes it. An empty folded query matches nothing; without
    /// its guard the prefix range would be the whole table.
    #[tokio::test]
    async fn a_query_that_folds_to_nothing_finds_nothing() {
        let pool = pool().await;
        work(&pool, "a", "alpha", 1).await;
        let folded: String = sqlx::query_scalar("SELECT public.search_fold($1)")
            .bind("\u{345}")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(folded, "");
        assert_eq!(found(&pool, "\u{345}").await, Vec::<String>::new());
        pool.close().await;
    }

    /// With sequential scans disabled the planner uses an index wherever one can
    /// serve. The prefix range must be an index condition on the bounded key; the
    /// former CASE form left a scan of the whole alias table as the only path.
    #[tokio::test]
    async fn prefix_probe_is_an_index_range_on_the_bounded_key() {
        let pool = pool().await;
        work(&pool, "a", "alpha", 1).await;
        let mut reader = pool.read().acquire().await.unwrap();
        reader.close_on_drop();
        sqlx::query("SELECT set_config('enable_seqscan','off',false)")
            .execute(&mut *reader)
            .await
            .unwrap();
        let ctes = match_ctes("SELECT w.id AS work_id FROM work w", "TRUE");
        for query in ["alpha".to_owned(), "ﬁ".repeat(150)] {
            let plan: serde_json::Value = sqlx::query_scalar(&format!(
                "EXPLAIN (FORMAT JSON) WITH {ctes} SELECT COUNT(*) FROM m"
            ))
            .bind(&query)
            .fetch_one(&mut *reader)
            .await
            .unwrap();
            let mut all = Vec::new();
            nodes(&plan, &mut all);
            assert!(
                !all.iter().any(|node| node["Node Type"] == "Seq Scan"
                    && node["Relation Name"] == "work_search_alias"),
                "the alias table is scanned whole: {plan}"
            );
            assert!(
                all.iter().any(|node| {
                    node["Index Name"] == "work_search_alias_exact_idx"
                        && node["Index Cond"]
                            .as_str()
                            .is_some_and(|cond| cond.contains(">=") && cond.contains(" < "))
                }),
                "no range probe on the bounded key: {plan}"
            );
        }
        drop(reader);
        pool.close().await;
    }

    /// Five equal matches and a window of two. The window is the two lowest work
    /// ids, whatever their chapter counts, and the chapter count then orders those
    /// two. Chapters are counted for the window only, and eligibility is probed
    /// until the window is full and no further.
    #[tokio::test]
    async fn window_is_cut_before_chapters_are_counted() {
        let pool = pool().await;
        for (id, chapters) in [("e1", 1), ("e2", 2), ("e3", 30), ("e4", 40), ("e5", 50)] {
            work(&pool, id, &format!("ember {id}"), chapters).await;
        }
        let statement = format!(
            "WITH {}, {} SELECT work_id FROM cand ORDER BY tier, chapters DESC, rank, work_id",
            match_ctes("SELECT w.id AS work_id FROM work w", "TRUE"),
            window_cte("2")
        );
        let ids: Vec<String> = sqlx::query_scalar(&statement)
            .bind("ember")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(ids, ["e2", "e1"]);
        let plan: serde_json::Value =
            sqlx::query_scalar(&format!("EXPLAIN (ANALYZE, FORMAT JSON) {statement}"))
                .bind("ember")
                .fetch_one(&pool)
                .await
                .unwrap();
        let mut all = Vec::new();
        nodes(&plan, &mut all);
        let loops = |relation: &str| {
            all.iter()
                .filter(|node| node["Relation Name"] == relation)
                .map(|node| node["Actual Loops"].as_f64().unwrap())
                .fold(0.0, f64::max)
        };
        assert_eq!(
            loops("chapter"),
            2.0,
            "chapters counted outside the window: {plan}"
        );
        assert_eq!(
            loops("work"),
            2.0,
            "eligibility is not one probe a window row: {plan}"
        );
        pool.close().await;
    }

    /// `solo` has a word hit only in works the filter hides, so it falls back to
    /// typo hits, and `kept` is the one eligible work with a close word. Up to
    /// 10,000 eligible works the fallback reads their rows and the trigram index
    /// is not scanned; past that the index is, and the rows of the eligible
    /// works are not read one by one. The result is the same on both sides.
    #[tokio::test]
    async fn typo_fallback_reads_the_eligible_works_when_they_are_few() {
        let pool = pool().await;
        work(&pool, "hidden1", "Solo", 1).await;
        work(&pool, "hidden2", "Sool", 1).await;
        work(&pool, "kept", "Sool Nights", 1).await;
        // 10,000 more works with a source: 10,000 eligible without the last, 10,001 with it.
        sqlx::query("INSERT INTO work(id,primary_title,created_at,updated_at) SELECT 'bulk' || n, 'filler ' || n, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z' FROM generate_series(1,10000) n")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO source_series(id,work_id,source_type,source_key,created_at) SELECT 'bulk' || n || ':s', 'bulk' || n, 'mangadex', 'bulk' || n || ':k', '2020-01-01T00:00:00Z' FROM generate_series(1,10000) n")
            .execute(&pool).await.unwrap();
        for (filter, few) in [
            ("w.id NOT LIKE 'hidden%' AND w.id <> 'bulk10000'", true),
            ("w.id NOT LIKE 'hidden%'", false),
        ] {
            let statement = format!(
                "WITH {} SELECT work_id || ' ' || tier FROM m ORDER BY work_id",
                match_ctes("SELECT w.id AS work_id FROM work w", filter)
            );
            // Dropped unfinished below, so this session is closed and not pooled.
            let mut search = crate::catalog::SearchConnection::new(&pool).await.unwrap();
            let found: Vec<String> = sqlx::query_scalar(&statement)
                .bind("solo")
                .fetch_all(&mut *search)
                .await
                .unwrap();
            assert_eq!(found, ["kept 3"], "{filter}");
            let plan: serde_json::Value =
                sqlx::query_scalar(&format!("EXPLAIN (ANALYZE, FORMAT JSON) {statement}"))
                    .bind("solo")
                    .fetch_one(&mut *search)
                    .await
                    .unwrap();
            let mut all = Vec::new();
            nodes(&plan, &mut all);
            let loops = |alias: &str| {
                let scans: Vec<f64> = all
                    .iter()
                    .filter(|node| node["Alias"] == alias)
                    .map(|node| node["Actual Loops"].as_f64().unwrap())
                    .collect();
                assert!(!scans.is_empty(), "no scan of {alias}: {plan}");
                scans.into_iter().fold(0.0, f64::max)
            };
            if few {
                assert_eq!(loops("any_row"), 0.0, "catalogue probed: {plan}");
                assert_eq!(loops("own_row"), 10000.0, "{plan}");
            } else {
                assert_eq!(loops("any_row"), 1.0, "{plan}");
                assert_eq!(loops("own_row"), 0.0, "eligible rows read: {plan}");
            }
            // A term with an eligible word hit pays for neither side.
            let plan: serde_json::Value =
                sqlx::query_scalar(&format!("EXPLAIN (ANALYZE, FORMAT JSON) {statement}"))
                    .bind("nights")
                    .fetch_one(&mut *search)
                    .await
                    .unwrap();
            let mut all = Vec::new();
            nodes(&plan, &mut all);
            let scope: Vec<_> = all
                .iter()
                .filter(|node| node["CTE Name"] == "typo_works" || node["Alias"] == "any_row")
                .collect();
            assert!(scope.len() >= 3, "{plan}");
            assert!(
                scope.iter().all(|node| node["Actual Loops"] == 0.0),
                "typo scope read without a typo term: {plan}"
            );
        }
        pool.close().await;
    }

    /// Every pair `search_typo_close` accepts must pass the cheap test in front of
    /// it, or a typo match would be lost. The words are each base word with every
    /// one-edit and many two-edit neighbours, over an alphabet that includes
    /// characters outside ASCII, compared with every base word.
    #[tokio::test]
    async fn typo_candidate_test_never_rejects_a_close_word() {
        let pool = pool().await;
        let start = MATCH_CTES.find("abs(length(word)").unwrap();
        let end = MATCH_CTES.find("AND public.search_typo_close").unwrap();
        let candidate = &MATCH_CTES[start..end];
        let bases = [
            "a",
            "ab",
            "solo",
            "azure",
            "naruto",
            "crimson",
            "leveling",
            "aaaaaa",
            "abcabc",
            "straße",
            "リゼロ",
            "onepiece",
            "qq",
            "12345",
            "x1y2z3",
        ];
        let alphabet = ['a', 'e', 'z', '1', 'ß', 'ゼ'];
        fn edits(word: &str, alphabet: &[char]) -> Vec<String> {
            let chars: Vec<char> = word.chars().collect();
            let mut out = Vec::new();
            for i in 0..=chars.len() {
                for &c in alphabet {
                    let mut inserted = chars.clone();
                    inserted.insert(i, c);
                    out.push(inserted.iter().collect());
                }
                if i < chars.len() {
                    let mut deleted = chars.clone();
                    deleted.remove(i);
                    out.push(deleted.iter().collect());
                    for &c in alphabet {
                        let mut replaced = chars.clone();
                        replaced[i] = c;
                        out.push(replaced.iter().collect());
                    }
                }
                if i + 1 < chars.len() {
                    let mut swapped = chars.clone();
                    swapped.swap(i, i + 1);
                    out.push(swapped.iter().collect());
                }
            }
            out
        }
        let mut words = std::collections::BTreeSet::new();
        for base in bases {
            words.insert(base.to_owned());
            for once in edits(base, &alphabet) {
                // Two edits from every seventh neighbour keep the set small.
                if words.len() % 7 == 0 {
                    words.extend(edits(&once, &alphabet));
                }
                words.insert(once);
            }
        }
        words.remove("");
        let words: Vec<String> = words.into_iter().collect();
        let bases: Vec<String> = bases.iter().map(|base| base.to_string()).collect();
        let (close, lost, rejected): (i64, i64, i64) = sqlx::query_as(&format!(
            "SELECT count(*) FILTER (WHERE close), count(*) FILTER (WHERE close AND NOT candidate), \
                    count(*) FILTER (WHERE NOT candidate) \
             FROM (SELECT public.search_typo_close(r.term, word) AS close, {candidate} AS candidate \
                   FROM unnest($1::text[]) r(term), unnest($2::text[]) word) pairs"
        ))
        .bind(&bases)
        .bind(&words)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            close > 1000,
            "the sample holds too few close pairs: {close}"
        );
        assert_eq!(lost, 0, "a close pair fails the candidate test");
        assert!(
            rejected > close,
            "the candidate test rejects nothing: {rejected}"
        );
        pool.close().await;
    }

    /// Typo recall at the edges of the edit budget: one edit for a word of at most
    /// five characters, two for a longer one; a word inside a longer title; a word
    /// with a character outside ASCII; repeated letters.
    #[tokio::test]
    async fn typo_terms_find_words_within_the_edit_budget_only() {
        let pool = pool().await;
        work(&pool, "crimson", "The Crimson Tide", 1).await;
        work(&pool, "solo", "Solo", 1).await;
        work(&pool, "strasse", "Straße 9", 1).await;
        work(&pool, "letters", "abcdefgh", 1).await;
        work(&pool, "repeat", "aaaaab", 1).await;
        for (query, expected) in [
            ("crimsno", Some("crimson")),  // transposition
            ("crimsoon", Some("crimson")), // insertion
            ("crmson", Some("crimson")),   // deletion
            ("crimzon", Some("crimson")),  // substitution
            ("crmzon", Some("crimson")),   // two edits, both words longer than five
            ("crmzn", None),               // five characters: one edit only
            ("crimsonxyz", None),          // three edits
            ("sool", Some("solo")),
            ("soloo", Some("solo")),
            ("sxlo", Some("solo")),
            ("slxo", None), // two edits on a short word
            ("strase", Some("strasse")),
            ("cdefgh", Some("letters")), // two characters shorter, budget two
            ("defgh", None),             // three shorter
            ("aaaab", Some("repeat")),
        ] {
            let expected: Vec<String> = expected.iter().map(|id| id.to_string()).collect();
            assert_eq!(found(&pool, query).await, expected, "{query}");
        }
        pool.close().await;
    }
}
