-- W3d overlay, applied only after every baseline GraphQL and search read.
-- Chapter 103 (MangaDex) and 710101 (Suwayomi) are existing copies of chapter 10.5.
-- Do not mark the work ready: its other copies have not been linked.
INSERT INTO work_chapter (id, work_id, match_key, sort_key, number, label, created_at) VALUES ('wc_00000000000040008000000000001050', 'w_00000000000040008000000000000001', '1050', 10.5, '10.5', '10.5', '2020-01-08T01:02:03Z');
INSERT INTO work_chapter_link (chapter_id, work_chapter_id, linked_by, linked_at) VALUES ('ch_00000000000040008000000000000103', 'wc_00000000000040008000000000001050', 'matcher', '2020-01-08T01:02:03Z');
INSERT INTO work_chapter_link (chapter_id, work_chapter_id, linked_by, linked_at) VALUES ('ch_00000000000040008000000000710001', 'wc_00000000000040008000000000001050', 'matcher', '2020-01-08T01:02:03Z');
-- Every seed author has a myComments golden, so these comments stay in this overlay.
INSERT INTO comments (id, target_type, target_id, user_id, body, has_spoiler, created_at, parent_id) VALUES ('00000000-0000-4000-8000-000000001111', 'chapter', '710101', '00000000-0000-4000-8000-000000000902', 'Mirror chapter root', TRUE, '2020-01-09T01:02:03Z', NULL);
INSERT INTO comments (id, target_type, target_id, user_id, body, has_spoiler, created_at, parent_id) VALUES ('00000000-0000-4000-8000-000000001112', 'chapter', '710101', '00000000-0000-4000-8000-000000000900', 'Reply from the linked copy', FALSE, '2020-01-10T01:02:03Z', '00000000-0000-4000-8000-000000001110');
INSERT INTO comment_votes (comment_id, user_id, value, created_at) VALUES ('00000000-0000-4000-8000-000000001111', '00000000-0000-4000-8000-000000000901', 1, '2020-01-10T02:02:03Z');
INSERT INTO comment_votes (comment_id, user_id, value, created_at) VALUES ('00000000-0000-4000-8000-000000001111', '00000000-0000-4000-8000-000000000902', -1, '2020-01-10T03:02:03Z');
