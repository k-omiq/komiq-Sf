-- Admin-overridable comic type (manga/manhwa/manhua/webtoon/comic).
-- Content type is otherwise DERIVED on read from `original_language` (+ genre/script
-- heuristics), never stored. This one nullable column lets an admin pin a correct
-- classification when the derivation is wrong or the source language is unknown.
-- NULL => derive; else MANGA/MANHWA/MANHUA/WEBTOON/COMIC (matches ComicType).
ALTER TABLE work ADD COLUMN content_type_override TEXT;
