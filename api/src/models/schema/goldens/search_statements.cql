-- search.results_ids.enumerate
SELECT DISTINCT key FROM {ns}.results_ids WHERE token(key) >= ? AND token(key) <= ?
-- search.results_ids.init_data
SELECT key, kind, group, id FROM {ns}.results_ids WHERE key IN ?
-- search.results.results
SELECT id, result, files, children FROM {ns}.results WHERE id IN ?
-- search.results_auth.event_data
SELECT group, id FROM {ns}.results_auth WHERE key = ? AND kind = ? AND group IN ?
-- search.tags_by_item.enumerate
SELECT DISTINCT type, item FROM {ns}.tags_by_item WHERE token(type, item) >= ? AND token(type, item) <= ?
-- search.tags_by_item.init
SELECT group, item, key, value FROM {ns}.tags_by_item WHERE type = ? AND item IN ?
-- search.tags_by_item.event
SELECT group, key, value FROM {ns}.tags_by_item WHERE type = ? AND item = ? AND group IN ?
