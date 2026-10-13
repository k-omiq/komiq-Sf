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

/// The first part of both search statements: eligibility, the folded query,
/// its terms and their word hits. See [`MATCH_CTES`].
const TERM_CTES: &str = r#"
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
"#;

/// Whether a word of `r.normalized_title` is inside the edit budget of `r.term`.
const CLOSE_WORD: &str = r#"EXISTS (SELECT 1 FROM string_to_table(r.normalized_title, ' ') word
               WHERE abs(length(word) - length(r.term)) <= 2
               AND greatest(abs(length(word) - length(r.term)),
                            length(translate(word, r.term, '')),
                            length(translate(r.term, word, '')))
                   <= CASE WHEN least(length(word), length(r.term)) <= 5 THEN 1 ELSE 2 END
               AND public.search_typo_close(r.term, word))"#;

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
/// A term in typo fallback (`typo_terms`) is matched against the rows of a set
/// of works, and the set is the smallest the statement can name. No statement
/// reads the trigram index for more than one term: each such read returns up
/// to 160,000 rows for a short term and rechecks every one, and two of them
/// do not fit the budget.
///
/// `held_works` are the works with a word hit for every term that is not in
/// fallback. A work of the answer is one of them. When there are such terms
/// and few such works, every term in fallback reads the rows of those works
/// and nothing else (`held`). Few is no more than the eligible works, and at
/// most 10,000 beside a term of three characters, whose index read is the
/// largest. Beside longer terms it is 2,500: the cheapest index read that
/// finds anything costs what the rows of that many works cost.
///
/// Otherwise the longest term in fallback goes first, the first of them in the
/// query when several are as long (`longest`). It reads one of two sets. With
/// more than 10,000 eligible works the trigram index finds its rows in the
/// whole catalogue, and the final probe drops the rows of ineligible works.
/// With fewer, only the rows of the eligible works are read (`narrow`): a
/// narrow filter then costs its own size. 10,000 is where that costs what the
/// index costs for the cheapest term on a catalogue of 120,000 works. Every
/// other term in fallback reads the rows of the works the longest term found
/// (`longest_hits`): an eligible work of the answer is one of them, because a
/// term in fallback has no eligible word hit.
///
/// Each way gives an eligible work the same typo hits and the same rank, since
/// each reads all the rows of that work that the catalogue read would return.
/// Each hangs from the term by `LATERAL`, so nothing of it runs for a query
/// with no term in fallback.
///
/// A word within one edit of a term of four characters `abcd` starts with
/// `ab`, or ends with `cd`, or ends with `bd` (`b` and `c` changed places).
/// `shapes` holds those three as patterns on the trigram key, and the catalogue
/// read asks for one of them beside the trigram test. That cuts its rows to a
/// fifth. A close word of three characters may keep the first and the last
/// only, which no short pattern holds, and five or more need two shared
/// trigrams already, so every other length has the pattern `%`.
///
/// A typo candidate row is split on single spaces; an empty piece is never a
/// typo match. `search_typo_close` runs only for a word that can pass it: its
/// edit budget bounds the difference in length, the characters of the word
/// that the term lacks and the characters of the term that the word lacks.
/// The length is tested first, alone, because the other two cost the length of
/// the term for each word. A term of more than 256 characters has no typo
/// probe at all: the function accepts no word for it.
///
/// The statement is [`TERM_CTES`], this text and [`RANKED_CTES`], in that order.
/// [`RETRY_CTES`] stands between the same two parts in the second statement.
const MATCH_CTES: &str = r#"typo_works AS MATERIALIZED (SELECT work_id FROM eligible LIMIT 10001),
typo_terms AS MATERIALIZED (
 SELECT p.ord, p.term
 FROM parts p
 WHERE __TYPO_TERMS__
 AND NOT EXISTS (SELECT 1 FROM word_hits h WHERE h.ord=p.ord
                 AND EXISTS (SELECT 1 FROM eligible e WHERE e.work_id=h.work_id OFFSET 0)
                 OFFSET 0)
),
held_works AS MATERIALIZED (
 SELECT work_id
 FROM (SELECT DISTINCT h.work_id, h.ord FROM word_hits h
       WHERE NOT EXISTS (SELECT 1 FROM typo_terms t WHERE t.ord=h.ord)) held_hits
 GROUP BY work_id
 HAVING count(*) = (SELECT count(*) FROM parts) - (SELECT count(*) FROM typo_terms)
 LIMIT 10001
),
typo_parts AS MATERIALIZED (
 SELECT t.*, CASE WHEN t.held THEN FALSE
                  ELSE (SELECT count(*) FROM typo_works) <= 10000 END AS narrow
 FROM (SELECT ord, term, public.search_trigram_key(term) AS key,
              CASE WHEN length(term) = 4 THEN ARRAY[
                   public.search_trigram_key(left(term,2)) || '%',
                   '% ' || public.search_trigram_key(left(term,2)) || '%',
                   '%' || public.search_trigram_key(right(term,2)),
                   '%' || public.search_trigram_key(right(term,2)) || ' %',
                   '%' || public.search_trigram_key(substr(term,2,1) || right(term,1)),
                   '%' || public.search_trigram_key(substr(term,2,1) || right(term,1)) || ' %']
                   ELSE ARRAY['%'] END AS shapes,
              row_number() OVER (ORDER BY length(term) DESC, ord) = 1 AS longest,
              CASE WHEN (SELECT count(*) FROM parts) > (SELECT count(*) FROM typo_terms)
                   THEN (SELECT count(*) FROM held_works)
                        <= least(CASE WHEN max(length(term)) OVER () = 3 THEN 10000 ELSE 2500 END,
                                 (SELECT count(*) FROM typo_works))
                   ELSE FALSE END AS held
       FROM typo_terms) t
),
longest_rows AS (
 SELECT p.ord, p.term, p.key, s.work_id, s.trigram_key, s.normalized_title
 FROM typo_parts p
 CROSS JOIN LATERAL (SELECT work_id, trigram_key, normalized_title
                     FROM public.work_search_alias any_row
                     WHERE p.longest AND NOT p.held AND NOT p.narrow
                     AND any_row.trigram_key OPERATOR(public.%>) p.key
                     AND any_row.trigram_key LIKE ANY (p.shapes)
                     OFFSET 0) s
 UNION ALL
 SELECT p.ord, p.term, p.key, s.work_id, s.trigram_key, s.normalized_title
 FROM typo_parts p
 CROSS JOIN LATERAL (SELECT work_id FROM typo_works
                     WHERE p.longest AND NOT p.held AND p.narrow OFFSET 0) e
 CROSS JOIN LATERAL (SELECT work_id, trigram_key, normalized_title
                     FROM public.work_search_alias own_row
                     WHERE own_row.work_id=e.work_id OFFSET 0) s
 WHERE s.trigram_key OPERATOR(public.%>) p.key
),
longest_hits AS MATERIALIZED (
 SELECT r.work_id, r.ord, 3::bigint AS tier,
        -max(public.word_similarity(r.key, r.trigram_key))::double precision AS rank
 FROM longest_rows r
 WHERE __CLOSE_WORD__
 GROUP BY r.work_id, r.ord
),
typo_rows AS (
 SELECT p.ord, p.term, p.key, c.work_id, s.trigram_key, s.normalized_title
 FROM typo_parts p
 CROSS JOIN LATERAL (SELECT work_id FROM held_works WHERE p.held
                     UNION ALL
                     SELECT work_id FROM longest_hits WHERE NOT p.held AND NOT p.longest
                     OFFSET 0) c
 CROSS JOIN LATERAL (SELECT trigram_key, normalized_title
                     FROM public.work_search_alias held_row
                     WHERE held_row.work_id=c.work_id OFFSET 0) s
 WHERE s.trigram_key OPERATOR(public.%>) p.key
),
fuzzy_hits AS (
 SELECT * FROM longest_hits
 UNION ALL
 SELECT r.work_id, r.ord, 3::bigint AS tier,
        -max(public.word_similarity(r.key, r.trigram_key))::double precision AS rank
 FROM typo_rows r
 WHERE __CLOSE_WORD__
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
"#;

/// The second statement, for a query that [`MATCH_CTES`] answers with nothing.
/// It stands where `MATCH_CTES` stands and gives the same `ranked` and `m`. The
/// callers run it only after an empty answer, so a query that finds works with
/// the first statement never reaches it. It has two rules.
///
/// A term that is spelt wrong and is a word of other titles. Such a term has
/// word hits, so the first statement gives it no typo matching, and no work has
/// every term. Here `near` holds the works with a word hit for every term but
/// one. Each is read by its own rows, and the term it lacks may match one of
/// its words as a typo, by the test of `fuzzy_hits`. Nothing reads the trigram
/// index. The terms are tried from the fewest such works upward, while the
/// works read stay within 10,000, the line of the typo fallback; a term past
/// the line is not tried. One term only: a work that lacks two terms is no
/// candidate, and a query of one term has none. A term outside 3 to 256
/// characters has no typo matching, as in the first statement. Nor is a word
/// under three characters a match here: beside a term of three it would be any
/// particle of a title, and the answer a list of unrelated works.
///
/// A query written without its spaces. A title matches when, with the spaces
/// of both taken out, it starts with the query, and its first word is shorter
/// than the query. `cut` is each length that first word can have. One range
/// probe on the bounded key finds the titles that start with that many
/// characters of the query, a space and the next character, and the whole
/// string is rechecked without its spaces. An equal title is tier 0, a longer
/// one tier 1.
const RETRY_CTES: &str = r#"near AS MATERIALIZED (
 SELECT work_id, ((SELECT sum(ord) FROM parts) - sum(ord))::bigint AS ord
 FROM (SELECT DISTINCT work_id, ord FROM word_hits
       WHERE (SELECT count(*) FROM parts) > 1) hits
 GROUP BY work_id
 HAVING count(*) = (SELECT count(*) FROM parts) - 1
),
near_parts AS MATERIALIZED (
 SELECT ord, term, public.search_trigram_key(term) AS key
 FROM (SELECT p.ord, p.term, sum(lacking.works) OVER (ORDER BY lacking.works, p.ord) AS tried
       FROM (SELECT ord, count(*) AS works FROM near GROUP BY ord) lacking
       JOIN parts p ON p.ord=lacking.ord
       WHERE length(p.term) BETWEEN 3 AND 256) sized
 WHERE tried <= 10000
),
near_rows AS (
 SELECT p.term, p.key, n.work_id, s.trigram_key, s.normalized_title
 FROM near_parts p JOIN near n ON n.ord=p.ord
 CROSS JOIN LATERAL (SELECT trigram_key, normalized_title
                     FROM public.work_search_alias near_row
                     WHERE near_row.work_id=n.work_id OFFSET 0) s
 WHERE s.trigram_key OPERATOR(public.%>) p.key
),
near_works AS (
 SELECT r.work_id, 3::bigint AS tier,
        ((1 - (SELECT count(*) FROM parts) - max(public.word_similarity(r.key, r.trigram_key)))
         / (SELECT count(*) FROM parts))::double precision AS rank
 FROM near_rows r
 WHERE __CLOSE_WORD__
 GROUP BY r.work_id
),
joined_works AS (
 SELECT s.work_id,
        CASE WHEN length(replace(s.normalized_title,' ','')) = length(j.joined)
             THEN 0 ELSE 1 END::bigint AS tier,
        0.0::double precision AS rank
 FROM (SELECT replace(q.folded,' ','') AS joined FROM q) j
 CROSS JOIN LATERAL generate_series(1, length(j.joined) - 1) cut
 CROSS JOIN LATERAL (SELECT left(j.joined,cut) || ' ' || substr(j.joined,cut + 1,1) AS head) h
 CROSS JOIN LATERAL (SELECT work_id, normalized_title
                     FROM public.work_search_alias joined_row
                     WHERE left(joined_row.normalized_title,256) >= left(h.head,256)
                     AND left(joined_row.normalized_title,256) < left(h.head,256) || chr(1114111)
                     OFFSET 0) s
 WHERE starts_with(replace(s.normalized_title,' ',''), j.joined)
),
matched AS (
 SELECT * FROM joined_works UNION ALL SELECT * FROM near_works
),
"#;

/// The last part of both search statements: one row for a matched work, then
/// the eligible ones.
const RANKED_CTES: &str = r#"ranked AS NOT MATERIALIZED (
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
    ctes(MATCH_CTES, TYPO_TERMS, eligible_select, eligible_where)
}

/// The statement to run when [`match_ctes`] finds nothing, and only then: see
/// [`RETRY_CTES`]. Same arguments, same `m`, so a caller swaps the text and
/// keeps its statements and binds.
pub(crate) fn retry_ctes(eligible_select: &str, eligible_where: &str) -> String {
    #[cfg(test)]
    RETRIES.with(|retries| retries.set(retries.get() + 1));
    let close_word = CLOSE_WORD.replacen("WHERE ", "WHERE length(word) >= 3 AND ", 1);
    let retry = RETRY_CTES.replace("__CLOSE_WORD__", &close_word);
    ctes(&retry, TYPO_TERMS, eligible_select, eligible_where)
}

/// [`match_ctes`] with no term in typo fallback, for a search whose first
/// statement ran out of its budget. A query with no term in fallback gets the
/// same statement and the same cost. A query with one gets what is left
/// without typo matching, and no work has every term then, so the caller goes
/// on to [`retry_ctes`].
pub(crate) fn plain_ctes(eligible_select: &str, eligible_where: &str) -> String {
    ctes(MATCH_CTES, "FALSE", eligible_select, eligible_where)
}

/// Whether PostgreSQL cancelled the statement at the search budget.
pub(crate) fn over_budget(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .as_deref()
        == Some("57014")
}

// The terms that may fall back to typo matching in [`MATCH_CTES`].
const TYPO_TERMS: &str = "length(p.term) BETWEEN 3 AND 256";

fn ctes(matched: &str, typo_terms: &str, eligible_select: &str, eligible_where: &str) -> String {
    [TERM_CTES, matched, RANKED_CTES]
        .concat()
        .replace("__CLOSE_WORD__", CLOSE_WORD)
        .replace("__TYPO_TERMS__", typo_terms)
        .replace("__ELIGIBLE_SELECT__", eligible_select)
        .replace("__ELIGIBLE_WHERE__", eligible_where)
}

// How many retry statements this test thread has built. A test runs on one
// thread, so it reads its own searches and no other test's.
#[cfg(test)]
thread_local! {
    pub(crate) static RETRIES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Test databases only. From this call on the typo test sleeps and then accepts
/// every word. `seconds` is SQL for how long, and may read `waited`, the
/// seconds since this call: a reader cannot count its calls, so a test that
/// wants one statement slow and the next one fast tells them apart by the time.
#[cfg(test)]
pub(crate) async fn slow_typo_test(pool: &crate::database::Database, seconds: &str) {
    for statement in [
        "CREATE TABLE IF NOT EXISTS public.typo_test_clock AS SELECT clock_timestamp() AS since",
        "UPDATE public.typo_test_clock SET since = clock_timestamp()",
        &format!(
            "CREATE OR REPLACE FUNCTION public.search_typo_close(query_word text, candidate_word text) \
             RETURNS boolean LANGUAGE sql VOLATILE AS $$ \
             SELECT pg_sleep({seconds}) IS NOT NULL \
             FROM (SELECT extract(epoch FROM clock_timestamp() - since) AS waited \
                   FROM public.typo_test_clock) clock $$"
        ),
    ] {
        sqlx::query(statement).execute(pool).await.unwrap();
    }
}

/// Follows [`match_ctes`] or [`retry_ctes`] in the same `WITH`. `window` is the
/// placeholder of the bound window size.
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

    /// Every word one edit from `word`: a character of `alphabet` put in or put
    /// in place of another, a character taken out, two neighbours exchanged.
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

    /// Every pair `search_typo_close` accepts must pass the cheap test in front of
    /// it, or a typo match would be lost. The words are each base word with every
    /// one-edit and many two-edit neighbours, over an alphabet that includes
    /// characters outside ASCII, compared with every base word.
    #[tokio::test]
    async fn typo_candidate_test_never_rejects_a_close_word() {
        let pool = pool().await;
        let start = CLOSE_WORD.find("abs(length(word)").unwrap();
        let end = CLOSE_WORD.find("AND public.search_typo_close").unwrap();
        let candidate = &CLOSE_WORD[start..end];
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

    /// A term of four characters reads the trigram index with `shapes` beside the
    /// trigram test. Every word `search_typo_close` accepts for such a term must
    /// match one of the patterns wherever it stands in a title, or a typo match
    /// would be lost. The words are every one-edit neighbour of each term.
    #[tokio::test]
    async fn the_shapes_of_a_four_letter_term_never_reject_a_close_word() {
        let pool = pool().await;
        let start = MATCH_CTES.find("ARRAY[\n").unwrap();
        let end = MATCH_CTES.find("ELSE ARRAY['%']").unwrap();
        let shapes = &MATCH_CTES[start..end];
        let terms = ["solo", "levl", "aaaa", "abab", "1a2b", "ßoゼo", "リゼロ日"];
        let alphabet = ['a', 'o', 'z', '1', 'ß', 'ゼ'];
        let mut words = std::collections::BTreeSet::new();
        for term in terms {
            words.insert(term.to_owned());
            words.extend(edits(term, &alphabet));
        }
        let terms: Vec<String> = terms.iter().map(|term| term.to_string()).collect();
        let words: Vec<String> = words.into_iter().collect();
        let (close, lost, rejected): (i64, i64, i64) = sqlx::query_as(&format!(
            "SELECT count(*) FILTER (WHERE close), count(*) FILTER (WHERE close AND NOT shaped), \
                    count(*) FILTER (WHERE NOT shaped) \
             FROM (SELECT public.search_typo_close(term, word) AS close, \
                          public.search_trigram_key(title) LIKE ANY ({shapes}) AS shaped \
                   FROM unnest($1::text[]) term, unnest($2::text[]) word, \
                        LATERAL (VALUES (word), (word || ' zz'), ('zz ' || word), \
                                        ('zz ' || word || ' zz')) titles(title)) pairs"
        ))
        .bind(&terms)
        .bind(&words)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(close > 800, "the sample holds too few close pairs: {close}");
        assert_eq!(lost, 0, "a close word matches no shape");
        assert!(rejected > close, "the shapes reject nothing: {rejected}");
        pool.close().await;
    }

    /// `found`, and how many retry statements the search built on the way.
    async fn found_counting(pool: &Database, query: &str) -> (Vec<String>, u32) {
        let before = RETRIES.with(std::cell::Cell::get);
        let ids = found(pool, query).await;
        (ids, RETRIES.with(std::cell::Cell::get) - before)
    }

    fn list(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    /// `kaizen`, `bleu`, `good` and the prefix `mna` are words of other titles, so
    /// the first statement gives them no typo matching and finds no work with
    /// every term. The retry reads the works that have the other terms.
    #[tokio::test]
    async fn a_misspelt_term_that_is_a_word_elsewhere_still_finds_the_work() {
        let pool = pool().await;
        work(&pool, "jk", "Jujutsu Kaisen", 5).await;
        work(&pool, "kz", "Kaizen Diary", 1).await;
        work(&pool, "jc", "Jujitsu Club", 1).await;
        work(&pool, "bl", "Blue Lock", 1).await;
        work(&pool, "bc", "Bleu Ciel", 1).await;
        work(&pool, "cm", "Chainsaw Man", 1).await;
        // `na` is one edit from `mna` and too short a word to be a match.
        work(&pool, "cn", "Chainsaw na Yoru", 1).await;
        work(&pool, "mn", "Mnarani Days", 1).await;
        work(&pool, "tg", "Tower of God", 1).await;
        work(&pool, "go", "Good Omens", 1).await;
        work(&pool, "gw", "Go West", 1).await;
        for (word, owner) in [
            ("kaizen", "kz"),
            ("bleu", "bc"),
            ("mna", "mn"),
            ("good", "go"),
        ] {
            assert_eq!(
                found_counting(&pool, word).await,
                (list(&[owner]), 0),
                "{word}"
            );
        }
        for (query, expected) in [
            ("jujutsu kaizen", "jk"),
            ("kaizen jujutsu", "jk"),
            ("bleu lock", "bl"),
            // The last term is a prefix in the first statement and a whole word here.
            ("chainsaw mna", "cm"),
            ("tower of good", "tg"),
        ] {
            assert_eq!(
                found_counting(&pool, query).await,
                (list(&[expected]), 1),
                "{query}"
            );
        }
        for query in [
            // Two terms are wrong: no work has all the others.
            "jujitsu kaizen",
            // `diary` is no typo of a word of `jk`, nor `jujutsu` of one of `kz`.
            "jujutsu diary",
            // Two characters: no typo matching, as in the first statement.
            "og west",
        ] {
            assert_eq!(
                found_counting(&pool, query).await,
                (Vec::new(), 1),
                "{query}"
            );
        }
        pool.close().await;
    }

    /// The query has no space where the title has one. An equal title leads,
    /// then the longer ones by chapters. A typo inside a joined query is not
    /// covered, nor a joined word that does not start the title.
    #[tokio::test]
    async fn a_query_without_its_spaces_finds_the_title() {
        let pool = pool().await;
        work(&pool, "op", "One Piece", 3).await;
        work(&pool, "omake", "One Piece Omake", 9).await;
        work(&pool, "sl", "Solo Leveling", 1).await;
        work(&pool, "bl", "Blue Lock", 1).await;
        work(&pool, "aot", "Attack on Titan", 1).await;
        work(&pool, "dr", "Dr. STONE", 1).await;
        work(&pool, "long", &format!("{} tail", "fi".repeat(150)), 1).await;
        for (query, expected) in [
            ("onepiece", vec!["op", "omake"]),
            ("OnePiece", vec!["op", "omake"]),
            ("sololeveling", vec!["sl"]),
            ("bluelock", vec!["bl"]),
            ("attackontitan", vec!["aot"]),
            ("attack ontitan", vec!["aot"]),
            ("drstone", vec!["dr"]),
            // The start of the title is enough, as for a query with its spaces.
            ("onepie", vec!["omake", "op"]),
            ("onepieceo", vec!["omake"]),
            // The space of the title lies past the bounded key.
            (&format!("{}tail", "ﬁ".repeat(150)), vec!["long"]),
            ("onepeice", vec![]),
            ("pieceone", vec![]),
        ] {
            let (ids, retries) = found_counting(&pool, query).await;
            assert_eq!(ids, list(&expected), "{query}");
            assert_eq!(retries, 1, "{query}");
        }
        pool.close().await;
    }

    /// Each of these queries has an answer in the first statement, and the retry
    /// would add a work to it: `bl` to the first, `op` to the second and third.
    /// The answer is the first statement's alone, items and total, and no retry
    /// statement is built.
    #[tokio::test]
    async fn a_query_with_an_answer_never_runs_the_retry() {
        let pool = pool().await;
        work(&pool, "bl", "Blue Lock", 9).await;
        work(&pool, "bls", "Bleu Lock Stories", 1).await;
        work(&pool, "op", "One Piece", 9).await;
        work(&pool, "o1", "Onepiece", 1).await;
        work(&pool, "o2", "Onepiece Party", 2).await;
        let first = format!(
            "WITH {}, {} SELECT work_id FROM cand ORDER BY tier,chapters DESC,rank,work_id",
            match_ctes("SELECT w.id AS work_id FROM work w", "TRUE"),
            window_cte("500")
        );
        for (query, expected) in [
            ("bleu lock", vec!["bls"]),
            ("onepiece", vec!["o1", "o2"]),
            // Typo fallback of the first statement.
            ("onepeice", vec!["o2", "o1"]),
            ("one piece", vec!["op"]),
        ] {
            let mut search = crate::catalog::SearchConnection::new(&pool).await.unwrap();
            let alone: Vec<String> = sqlx::query_scalar(&first)
                .bind(query)
                .fetch_all(&mut *search)
                .await
                .unwrap();
            drop(search);
            assert_eq!(alone, list(&expected), "{query}");
            assert_eq!(found_counting(&pool, query).await, (alone, 0), "{query}");
        }
        // The retry statement itself does hold the other work. `bls` is there too:
        // its title starts with the two words.
        let retry = format!(
            "WITH {} SELECT work_id FROM m ORDER BY work_id",
            retry_ctes("SELECT w.id AS work_id FROM work w", "TRUE")
        );
        for (query, expected) in [("bleu lock", vec!["bl", "bls"]), ("onepiece", vec!["op"])] {
            let mut search = crate::catalog::SearchConnection::new(&pool).await.unwrap();
            let ids: Vec<String> = sqlx::query_scalar(&retry)
                .bind(query)
                .fetch_all(&mut *search)
                .await
                .unwrap();
            assert_eq!(ids, list(&expected), "{query}");
        }
        pool.close().await;
    }

    /// Five works found by the retry only, two to a page: the same total on
    /// every page, each work once, in the order of the whole answer.
    #[tokio::test]
    async fn a_retry_answer_pages_with_one_total() {
        let pool = pool().await;
        for (id, chapters) in [("e1", 1), ("e2", 5), ("e3", 3), ("e4", 2), ("e5", 4)] {
            work(&pool, id, &format!("Ember Fall {id}"), chapters).await;
        }
        work(&pool, "other", "Embers", 9).await;
        let whole = found(&pool, "emberfall").await;
        assert_eq!(whole, ["e2", "e5", "e3", "e4", "e1"]);
        let mut paged = Vec::new();
        for page in 1..=4 {
            let (total, ids) =
                search_works_fts(&pool, "emberfall", true, &Default::default(), page, 2)
                    .await
                    .unwrap();
            assert_eq!(total, 5, "page {page}");
            assert_eq!(ids.len(), [2, 2, 1, 0][page as usize - 1], "page {page}");
            paged.extend(ids);
        }
        assert_eq!(paged, whole);
        pool.close().await;
    }

    /// The works read for typo matching stay within 10,000. `filler` is in 9,999
    /// works and `kaizen` in one, so 10,000 works lack one term and all are read;
    /// one more work with `filler` and the larger set is left out. The trigram
    /// index is read in neither case.
    #[tokio::test]
    async fn the_retry_reads_at_most_ten_thousand_works_for_typo_matching() {
        let pool = pool().await;
        work(&pool, "target", "Filler Kaisen", 1).await;
        work(&pool, "decoy", "Kaizen", 1).await;
        sqlx::query("INSERT INTO work(id,primary_title,created_at,updated_at) SELECT 'bulk' || n, 'filler ' || n, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z' FROM generate_series(1,9998) n")
            .execute(&pool).await.unwrap();
        let statement = format!(
            "WITH {} SELECT work_id FROM m",
            retry_ctes("SELECT w.id AS work_id FROM work w", "TRUE")
        );
        for (extra, read, expected) in [(false, 10000.0, vec!["target"]), (true, 1.0, vec![])] {
            if extra {
                work(&pool, "extra", "Filler 0", 1).await;
            }
            assert_eq!(
                found(&pool, "filler kaizen").await,
                list(&expected),
                "{read}"
            );
            let mut search = crate::catalog::SearchConnection::new(&pool).await.unwrap();
            let plan: serde_json::Value =
                sqlx::query_scalar(&format!("EXPLAIN (ANALYZE, FORMAT JSON) {statement}"))
                    .bind("filler kaizen")
                    .fetch_one(&mut *search)
                    .await
                    .unwrap();
            let mut all = Vec::new();
            nodes(&plan, &mut all);
            let loops: f64 = all
                .iter()
                .filter(|node| node["Alias"] == "near_row")
                .map(|node| node["Actual Loops"].as_f64().unwrap())
                .fold(0.0, f64::max);
            assert_eq!(loops, read, "{plan}");
            assert!(
                !all.iter()
                    .any(|node| node["Index Name"] == "work_search_alias_trgm_gin"),
                "the retry reads the trigram index: {plan}"
            );
        }
        pool.close().await;
    }

    /// With sequential scans disabled the planner uses an index wherever one can
    /// serve: each cut of a joined query must be a range probe on the bounded key.
    #[tokio::test]
    async fn the_joined_probe_is_an_index_range_on_the_bounded_key() {
        let pool = pool().await;
        work(&pool, "op", "One Piece", 1).await;
        let mut reader = pool.read().acquire().await.unwrap();
        reader.close_on_drop();
        sqlx::query("SELECT set_config('enable_seqscan','off',false)")
            .execute(&mut *reader)
            .await
            .unwrap();
        let plan: serde_json::Value = sqlx::query_scalar(&format!(
            "EXPLAIN (ANALYZE, FORMAT JSON) WITH {} SELECT COUNT(*) FROM m",
            retry_ctes("SELECT w.id AS work_id FROM work w", "TRUE")
        ))
        .bind("onepiece")
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
                    && node["Actual Loops"] == 7.0
                    && node["Index Cond"]
                        .as_str()
                        .is_some_and(|cond| cond.contains(">=") && cond.contains(" < "))
            }),
            "no range probe for each of the seven cuts: {plan}"
        );
        drop(reader);
        pool.close().await;
    }

    /// Two and three terms spelt wrong, none of them a word of any title: every
    /// one falls back to typo hits in the first statement. The last term is as
    /// short as type-ahead sends it. A term spelt right beside a wrong one takes
    /// the same answer by the other way (`held`).
    #[tokio::test]
    async fn misspelt_terms_together_find_the_work() {
        let pool = pool().await;
        work(&pool, "sl", "Solo Leveling", 5).await;
        work(&pool, "lu", "Solo Level Up", 1).await;
        work(&pool, "sn", "Solo Nights", 9).await;
        work(&pool, "lw", "Leveling Worlds", 9).await;
        for (query, expected) in [
            ("slo levleing", "sl"),
            ("levleing slo", "sl"),
            ("slo levelign", "sl"),
            // As it is typed: the last term stops short of the word.
            ("slo levl", "lu"),
            ("slo levle", "lu"),
            ("sloo levle upp", "lu"),
            ("solo levleing", "sl"),
            ("solo levl", "lu"),
        ] {
            assert_eq!(
                found_counting(&pool, query).await,
                (list(&[expected]), 0),
                "{query}"
            );
        }
        // `slo` and `nihgts` are close to words of `sn`, `levleing` to none of them.
        assert_eq!(
            found_counting(&pool, "slo levleing nihgts").await,
            (Vec::new(), 1)
        );
        pool.close().await;
    }

    /// With more than 10,000 eligible works a term in fallback reads the trigram
    /// index, and only one term of a query does: the longest. The other reads the
    /// rows of the works that one found: four for `levleing`, one for `levl`.
    /// Beside a term with word hits no term reads the index while the works with
    /// those hits are few: each term in fallback reads their rows. Three works
    /// have `solo`. 10,000 have `filler`: few beside a term of three characters,
    /// too many beside a longer one.
    #[tokio::test]
    async fn one_typo_term_at_most_reads_the_catalogue() {
        let pool = pool().await;
        work(&pool, "sl", "Solo Leveling", 1).await;
        work(&pool, "lu", "Solo Level Up", 1).await;
        work(&pool, "lw", "Leveling Worlds", 1).await;
        work(&pool, "la", "Leveling Alone", 1).await;
        work(&pool, "fl", "Filler Leveling", 1).await;
        work(&pool, "fs", "Filler Solo", 1).await;
        sqlx::query("INSERT INTO work(id,primary_title,created_at,updated_at) SELECT 'bulk' || n, 'filler ' || n, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z' FROM generate_series(1,9998) n")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO source_series(id,work_id,source_type,source_key,created_at) SELECT 'bulk' || n || ':s', 'bulk' || n, 'mangadex', 'bulk' || n || ':k', '2020-01-01T00:00:00Z' FROM generate_series(1,9998) n")
            .execute(&pool).await.unwrap();
        let statement = format!(
            "WITH {} SELECT work_id FROM m ORDER BY work_id",
            match_ctes("SELECT w.id AS work_id FROM work w", "TRUE")
        );
        for (query, expected, catalogue, works) in [
            ("slo levleing", "sl", 1.0, 4.0),
            ("slo levl", "lu", 1.0, 1.0),
            ("solo levleing", "sl", 0.0, 3.0),
            // Two terms in fallback, three works each.
            ("solo levleing slo", "sl", 0.0, 6.0),
            ("filler slo", "fs", 0.0, 10000.0),
            ("filler levleing", "fl", 1.0, 0.0),
        ] {
            let mut search = crate::catalog::SearchConnection::new(&pool).await.unwrap();
            let found: Vec<String> = sqlx::query_scalar(&statement)
                .bind(query)
                .fetch_all(&mut *search)
                .await
                .unwrap();
            assert_eq!(found, [expected], "{query}");
            let plan: serde_json::Value =
                sqlx::query_scalar(&format!("EXPLAIN (ANALYZE, FORMAT JSON) {statement}"))
                    .bind(query)
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
            assert_eq!(loops("any_row"), catalogue, "{query}: {plan}");
            assert_eq!(loops("own_row"), 0.0, "{query}: {plan}");
            assert_eq!(loops("held_row"), works, "{query}: {plan}");
        }
        pool.close().await;
    }

    /// A first statement that runs out of the budget on typo matching is no error
    /// to the reader. The search answers with what it finds without typo
    /// matching: here the title with a space where the query has none, in place
    /// of the title one letter away, and nothing at all for a query with no
    /// cheaper match. A query with no term in fallback is as fast as before.
    #[tokio::test]
    async fn a_search_over_budget_answers_without_typo_matching() {
        let pool = pool().await;
        work(&pool, "op", "One Piece", 1).await;
        work(&pool, "saga", "Onepiecf Saga", 1).await;
        work(&pool, "sl", "Solo Leveling", 1).await;
        assert_eq!(
            found_counting(&pool, "onepiece").await,
            (list(&["saga"]), 0)
        );
        assert_eq!(
            found_counting(&pool, "slo levleing").await,
            (list(&["sl"]), 0)
        );
        slow_typo_test(&pool, "10").await;
        assert_eq!(found_counting(&pool, "onepiece").await, (list(&["op"]), 1));
        assert_eq!(found_counting(&pool, "slo levleing").await, (Vec::new(), 1));
        assert_eq!(found_counting(&pool, "one piece").await, (list(&["op"]), 0));
        pool.close().await;
    }

    /// The budget error stands when typo matching is not what costs: the same
    /// search without it runs out again, and no retry answers in its place with
    /// fewer works than the query has. Another session holds the alias table
    /// here, so every statement waits past the budget.
    #[tokio::test]
    async fn a_search_over_budget_without_typo_matching_is_still_an_error() {
        let pool = pool().await;
        work(&pool, "op", "One Piece", 1).await;
        let mut holder = pool.write().begin().await.unwrap();
        sqlx::query("LOCK TABLE public.work_search_alias IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *holder)
            .await
            .unwrap();
        let before = RETRIES.with(std::cell::Cell::get);
        let answer = search_works_fts(&pool, "one piece", true, &Default::default(), 1, 30).await;
        assert!(answer.is_err(), "{answer:?}");
        assert_eq!(RETRIES.with(std::cell::Cell::get), before);
        holder.rollback().await.unwrap();
        assert_eq!(found(&pool, "one piece").await, ["op"]);
        pool.close().await;
    }

    /// The retry is an extra. When it runs out of the budget the answer is the
    /// first statement's, which found nothing in time, and no error. Here the
    /// first statement has no term in fallback and never asks the typo test; the
    /// retry asks it for `kaizen` against `kaisen`.
    #[tokio::test]
    async fn a_retry_over_budget_gives_the_empty_answer() {
        let pool = pool().await;
        work(&pool, "jk", "Jujutsu Kaisen", 1).await;
        work(&pool, "kz", "Kaizen Diary", 1).await;
        assert_eq!(
            found_counting(&pool, "jujutsu kaizen").await,
            (list(&["jk"]), 1)
        );
        slow_typo_test(&pool, "10").await;
        let started = std::time::Instant::now();
        assert_eq!(
            found_counting(&pool, "jujutsu kaizen").await,
            (Vec::new(), 1)
        );
        assert!(started.elapsed() >= std::time::Duration::from_secs(2));
        pool.close().await;
    }

    /// `found_counting`, with the statements the request ran and how long it took.
    async fn found_timing(pool: &Database, query: &str) -> (Vec<String>, u32, u32, f64) {
        use crate::catalog::STATEMENTS;
        let before = STATEMENTS.with(std::cell::Cell::get);
        let started = std::time::Instant::now();
        let (ids, retries) = found_counting(pool, query).await;
        let statements = STATEMENTS.with(std::cell::Cell::get) - before;
        (ids, retries, statements, started.elapsed().as_secs_f64())
    }

    /// One request spends at most `SEARCH_ALLOWANCE` on its statements. The count
    /// is cancelled at 2 s, the count without typo matching finds nothing at
    /// once, the retry's count takes 1.2 s and finds `a`, and 0.8 s is left for
    /// the retry's page, which needs 1.2 s too. It is cancelled, and the answer
    /// is the empty one after 4 s. With 2 s for every statement the answer
    /// would be `a` after 4.4 s.
    #[tokio::test]
    async fn one_request_spends_at_most_the_allowance() {
        let pool = pool().await;
        work(&pool, "a", "Solo Jujutsu", 1).await;
        assert_eq!(found_timing(&pool, "slo jujutsu").await.0, ["a"]);
        slow_typo_test(&pool, "CASE WHEN waited < 1 THEN 10 ELSE 1.2 END").await;
        let (ids, retries, statements, seconds) = found_timing(&pool, "slo jujutsu").await;
        assert_eq!((ids, retries, statements), (Vec::new(), 1, 4));
        let allowance = crate::catalog::SEARCH_ALLOWANCE.as_secs_f64();
        assert!(
            (allowance - 0.2..allowance + 0.3).contains(&seconds),
            "{seconds} s"
        );
        pool.close().await;
    }

    /// The count fits the budget (0.9 s) and its page does not. The request
    /// starts again without typo matching, and both its total and its items
    /// come from that second pass: here from the retry, which is fast by then.
    /// This is the longest road a request can take, five statements.
    #[tokio::test]
    async fn a_page_over_budget_is_answered_without_typo_matching() {
        let pool = pool().await;
        work(&pool, "a", "Solo Jujutsu", 1).await;
        slow_typo_test(
            &pool,
            "CASE WHEN waited < 0.8 THEN 0.9 WHEN waited < 2.5 THEN 10 ELSE 0 END",
        )
        .await;
        let (ids, retries, statements, seconds) = found_timing(&pool, "slo jujutsu").await;
        assert_eq!((ids, retries, statements), (list(&["a"]), 1, 5));
        assert!((2.8..4.0).contains(&seconds), "{seconds} s");
        pool.close().await;
    }
}
