-- The scanlator index: every group that chapters credit, and the series it is credited
-- on, kept current by a trigger on `chapter`. The Scanlators page reads these two small
-- tables instead of grouping the whole chapter table (scanlators.rs).
--
-- A group's key is its credit trimmed and lowercased, like `scanlators::name_key`, so
-- "Asura Scans" and "asura scans " are one group. MangaDex's "No Group" is not a credit.

-- The key of a credit. Empty for text that is no credit. No SET clause, so the planner
-- can inline it; it calls only pg_catalog functions, which resolve first on any path.
CREATE FUNCTION scanlator_key(credit text) RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT CASE WHEN lower(btrim(credit, E' \t\r\n')) = 'no group' THEN ''
                ELSE lower(btrim(credit, E' \t\r\n')) END
$$;

-- One row per group. `name` is the spelling the group was first seen with (the backfill
-- takes the most common one). A row is never deleted: a group whose series are all gone
-- loses `any_series`, and the list hides it.
--
-- The three flags are what the readers' list filters on, so a page never looks past this
-- table. `scanlator_group_refresh()` sets them; the server runs it at most every ten
-- minutes. A group the trigger adds is shown to viewers with the adult-content opt-in at
-- once, and to the others after the next refresh.
CREATE TABLE "scanlator_group" (
    "key" text NOT NULL,
    "name" text NOT NULL,
    -- False for a joint credit ("a & b") whose members are all groups of their own: the
    -- list shows the members instead.
    "listed" boolean NOT NULL DEFAULT true,
    -- Credited on a series that still exists.
    "any_series" boolean NOT NULL DEFAULT true,
    -- Credited on a series whose work Browse lists as not adult.
    "safe_series" boolean NOT NULL DEFAULT false,
    CONSTRAINT "scanlator_group_pkey" PRIMARY KEY ("key")
);

-- The series a group is credited on, once per pair. A joint credit ("a & b") is recorded
-- under its own key AND under each member's key, so a member's row covers its joint
-- releases too. Removing a series removes its rows. A single chapter that is deleted or
-- re-credited leaves its old pair behind; the pair only ever makes a group visible.
CREATE TABLE "scanlator_series" (
    "key" text NOT NULL,
    "source_series_id" text NOT NULL,
    CONSTRAINT "scanlator_series_pkey" PRIMARY KEY ("key", "source_series_id"),
    CONSTRAINT "scanlator_series_fk_0" FOREIGN KEY ("source_series_id")
        REFERENCES "source_series" ("id") ON DELETE CASCADE
);
CREATE INDEX "scanlator_series_series_idx" ON "scanlator_series" ("source_series_id");

-- Recompute the flags of every group from the index; writes only the rows that change.
-- Reads `scanlator_group`, `scanlator_series`, `source_series` and `browse_catalogue`,
-- never `chapter`. Returns the number of rows changed.
CREATE FUNCTION scanlator_group_refresh() RETURNS bigint
LANGUAGE sql SET search_path = pg_catalog, public AS $$
    WITH fresh AS (
        SELECT g.key,
               NOT (g.key LIKE '% & %' AND NOT EXISTS (
                   SELECT 1 FROM unnest(string_to_array(g.key, ' & ')) AS m
                    WHERE NOT EXISTS (SELECT 1 FROM scanlator_group x
                                       WHERE x.key = btrim(m)))) AS listed,
               EXISTS (SELECT 1 FROM scanlator_series l WHERE l.key = g.key) AS any_series,
               EXISTS (SELECT 1 FROM scanlator_series l
                         JOIN source_series s ON s.id = l.source_series_id
                         JOIN browse_catalogue f ON f.work_id = s.work_id
                        WHERE l.key = g.key AND f.is_nsfw = FALSE) AS safe_series
          FROM scanlator_group g
    ), changed AS (
        UPDATE scanlator_group g
           SET listed = fresh.listed, any_series = fresh.any_series,
               safe_series = fresh.safe_series
          FROM fresh
         WHERE g.key = fresh.key
           AND (g.listed, g.any_series, g.safe_series)
               IS DISTINCT FROM (fresh.listed, fresh.any_series, fresh.safe_series)
        RETURNING 1
    )
    SELECT COUNT(*) FROM changed
$$;

CREATE FUNCTION chapter_scanlator_index() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
DECLARE
    credit text := scanlator_key(NEW.scanlator);
BEGIN
    IF credit = '' THEN
        RETURN NULL;
    END IF;
    INSERT INTO scanlator_group (key, name)
        VALUES (credit, btrim(NEW.scanlator, E' \t\r\n'))
        ON CONFLICT (key) DO NOTHING;
    INSERT INTO scanlator_series (key, source_series_id)
        SELECT DISTINCT member, NEW.source_series_id
          FROM (SELECT credit
                UNION
                SELECT btrim(m) FROM unnest(string_to_array(credit, ' & ')) AS m) AS members(member)
         WHERE member <> ''
        ON CONFLICT DO NOTHING;
    RETURN NULL;
END;
$$;

CREATE TRIGGER chapter_scanlator_index_insert AFTER INSERT ON chapter
FOR EACH ROW WHEN (NEW.scanlator IS NOT NULL)
EXECUTE FUNCTION chapter_scanlator_index();
CREATE TRIGGER chapter_scanlator_index_update AFTER UPDATE OF scanlator, source_series_id ON chapter
FOR EACH ROW WHEN (NEW.scanlator IS NOT NULL
    AND (OLD.scanlator IS DISTINCT FROM NEW.scanlator
         OR OLD.source_series_id IS DISTINCT FROM NEW.source_series_id))
EXECUTE FUNCTION chapter_scanlator_index();

-- Backfill: one pass over the chapters that exist now.
INSERT INTO scanlator_group (key, name)
SELECT DISTINCT ON (key) key, name
  FROM (SELECT scanlator_key(scanlator) AS key, btrim(scanlator, E' \t\r\n') AS name,
               COUNT(*) AS n
          FROM chapter
         WHERE scanlator IS NOT NULL
         GROUP BY 1, 2) AS spellings
 WHERE key <> ''
 ORDER BY key, n DESC, name;

INSERT INTO scanlator_series (key, source_series_id)
SELECT DISTINCT member, c.source_series_id
  FROM (SELECT DISTINCT scanlator_key(scanlator) AS key, source_series_id
          FROM chapter
         WHERE scanlator IS NOT NULL) AS c
 CROSS JOIN LATERAL (SELECT c.key
                     UNION
                     SELECT btrim(m) FROM unnest(string_to_array(c.key, ' & ')) AS m) AS members(member)
 WHERE c.key <> '' AND member <> '';

SELECT scanlator_group_refresh();
