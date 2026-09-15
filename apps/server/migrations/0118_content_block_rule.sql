-- Content block rules: series this server, or one user, does not want listed.
--
-- One row is one rule. A rule names tags, content ratings, or both, and it matches a work
-- that carries ALL of its tags and, when it names ratings, has ONE of them:
--
--   tags = 'gore'            ratings = ''                      every work tagged Gore
--   tags = ''                ratings = 'pornographic'          every pornographic work
--   tags = 'gore'            ratings = 'erotica,pornographic'  Gore, but only when adult
--   tags = 'harem,isekai'    ratings = ''                      works that are both
--
-- A work is blocked when ANY rule that applies to the viewer matches it.
--
--   user_id   NULL   a site-wide rule: it applies to every viewer, signed in or not
--             else   that user's own rule: it applies to that user only, on top of the
--                    site-wide rules
--
-- `tags` holds tag ids (`tag.id`, the `work_tag.tag_key` slug) and `ratings` holds the
-- four rating words ('safe' | 'suggestive' | 'erotica' | 'pornographic'), each list sorted
-- and joined with ','. Both vocabularies are `[a-z0-9-]` only, so ',' cannot occur in a
-- member. `block::BlockRule` is the one writer and validates both; a rule with neither is
-- rejected there and by the CHECK.
--
-- A scope's rules are replaced as a whole (`setSiteBlockRules`, `setMyBlockRules`), so a
-- row has no identity a client needs: `id` only keeps the order they were given in.
CREATE TABLE content_block_rule (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    TEXT REFERENCES users(id) ON DELETE CASCADE,
    tags       TEXT NOT NULL DEFAULT '',
    ratings    TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    CHECK (tags != '' OR ratings != '')
);

-- Every read is "the site rules plus this user's": one seek per scope.
CREATE INDEX idx_content_block_rule_user ON content_block_rule(user_id);
