-- samples_list.insert
INSERT INTO {ns}.samples_list (group, year, bucket, sha256, sha1, md5, id, name, description, submitter, origin, uploaded) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- samples_list.delete_multiple_groups
DELETE FROM {ns}.samples_list WHERE group in ? AND year = ? AND bucket = ? AND uploaded = ? AND id = ?
-- samples.get
SELECT sha256, sha1, md5, id, name, description, group, submitter, origin, uploaded FROM {ns}.samples WHERE sha256 = ? AND group in ?
-- samples.get_many
SELECT sha256, sha1, md5, id, name, description, group, submitter, origin, uploaded FROM {ns}.samples WHERE sha256 in ? AND group in ?
-- samples.auth
SELECT sha256 FROM {ns}.samples WHERE sha256 in ? AND group in ? GROUP BY sha256
-- samples.get_basic_submission_info
SELECT group, submitter, id FROM {ns}.samples WHERE sha256 = ?
-- s3_ids.insert
INSERT INTO {ns}.s3_ids (type, id, sha256) VALUES (?, ?, ?)
-- s3_ids.id_exists
SELECT id FROM {ns}.s3_ids WHERE type = ? AND id = ? LIMIT 1
-- s3_ids.delete
DELETE FROM {ns}.s3_ids WHERE type = ? and id = ?
-- s3_sha256s.object_exists
SELECT sha256 FROM {ns}.s3_sha256s WHERE type = ? AND sha256 = ? LIMIT 1
-- s3_sha256s.get
SELECT id FROM {ns}.s3_sha256s WHERE type = ? AND sha256 = ? LIMIT 1
-- comments.insert
INSERT INTO {ns}.comments (group, sha256, uploaded, id, author, comment, files) VALUES (?, ?, ?, ?, ?, ?, ?)
-- comments.get
SELECT group, sha256, uploaded, id, author, comment, files FROM {ns}.comments WHERE group IN ? AND sha256 = ?
-- comments.delete
DELETE FROM {ns}.comments WHERE group = ? AND sha256 = ? AND uploaded = ? AND id = ?
-- comments_by_id.exists
SELECT id FROM {ns}.comments_by_id WHERE id IN ? GROUP BY id
-- repos_list.insert
INSERT INTO {ns}.repos_list (group, year, bucket, uploaded, id, url, provider, user, name, creator, scheme, default_checkout) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- repos_list.update_default_checkout
UPDATE {ns}.repos_list SET default_checkout = ? WHERE group = ? AND year = ? AND bucket = ? AND uploaded = ? AND id = ?
-- repos_list.update_earliest
UPDATE {ns}.repos_list SET earliest = ? WHERE group = ? AND year = ? AND bucket = ? AND uploaded = ? AND id = ?
-- repos.get
SELECT group, provider, user, name, url, id, creator, uploaded, scheme, default_checkout, earliest FROM {ns}.repos WHERE url = ? AND group in ?
-- repos.get_many
SELECT group, provider, user, name, url, id, creator, uploaded, scheme, default_checkout, earliest FROM {ns}.repos WHERE url in ? AND group in ?
-- repos.auth
SELECT url FROM {ns}.repos WHERE url in ? and group in ? group by url per partition limit 1
-- repo_data.insert_data
INSERT INTO {ns}.repo_data (repo, hash) VALUES (?, ?)
-- repo_data.get_data
SELECT hash FROM {ns}.repo_data WHERE repo = ?
-- repo_data.data_exists
SELECT hash FROM {ns}.repo_data WHERE repo = ? AND hash = ? LIMIT 1
-- repo_data.delete_data
DELETE FROM {ns}.repo_data WHERE hash = ? AND repo = ?
-- commitishes.insert
INSERT INTO {ns}.commitishes (kind, group, repo, key, timestamp, data, repo_data) VALUES (?, ?, ?, ?, ?, ?, ?)
-- commitishes.get_data
SELECT data FROM {ns}.commitishes WHERE kind = ? AND group IN ? AND repo = ? AND key = ? LIMIT 1
-- commitishes.get_repo_data
SELECT kind, repo_data FROM {ns}.commitishes WHERE kind IN ? AND group IN ? AND repo = ? AND key = ?
-- commitish_list.insert_list
INSERT INTO {ns}.commitish_list (kind, group, year, bucket, repo, timestamp, key, repo_data) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
-- results.insert
INSERT INTO {ns}.results (id, uploaded, tool, tool_version, cmd, result, files, entities, display_type) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
-- results.get
SELECT id, tool, tool_version, cmd, uploaded, result, files, entities, display_type, children FROM {ns}.results WHERE id in ?
-- results.get_uploaded_by_id
SELECT id, uploaded FROM {ns}.results WHERE id in ?
-- results.delete
DELETE FROM {ns}.results WHERE id = ?
-- results.update_children
UPDATE {ns}.results SET children[?] = ? WHERE id = ? AND uploaded = ?
-- results_stream.insert_stream
INSERT INTO {ns}.results_stream (kind, group, year, bucket, key, tool, tool_version, display_type, uploaded, cmd, id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- results_stream.delete_stream
DELETE FROM {ns}.results_stream WHERE kind = ? AND group = ? AND year = ? AND bucket = ? AND uploaded = ? and id = ?
-- results_auth.get_id
SELECT id FROM {ns}.results_auth WHERE key = ? AND kind = ? AND group in ? AND tool = ?
-- results_auth.get_with_key
SELECT id, tool, display_type, cmd, group, uploaded FROM {ns}.results_auth WHERE kind = ? AND group in ? AND key = ?
-- results_auth.get_with_key_and_tool
SELECT id, tool, display_type, cmd, group, uploaded FROM {ns}.results_auth WHERE kind = ? AND group in ? AND key = ? AND tool in ?
-- results_ids.count
SELECT id FROM {ns}.results_ids WHERE kind = ? AND key = ? AND id = ?
-- tags.insert
INSERT INTO {ns}.tags (type, group, item, year, bucket, key, value, uploaded, key_lower, value_lower) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- tags.delete
DELETE FROM {ns}.tags WHERE type = ? AND group = ? AND year = ? AND bucket = ? AND key = ? AND value = ? AND uploaded = ? AND item = ?
-- tags_by_item.get
SELECT group, item, key, value FROM {ns}.tags_by_item WHERE type = ? AND group IN ? AND item = ?
-- tags_by_item.get_rows
SELECT group, year, bucket, key, value, uploaded, item FROM {ns}.tags_by_item WHERE type = ? AND group IN ? AND item = ?
-- entities.insert
INSERT INTO {ns}.entities (kind, group, year, bucket, created, id, name, submitter, kind_data, description, image) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- entities.delete
DELETE FROM {ns}.entities WHERE kind = ? AND group in ? AND year = ? AND bucket = ? AND created = ? AND id = ?
-- entities_by_id.get
SELECT id, group, kind, created, name, submitter, kind_data, description, image FROM {ns}.entities_by_id WHERE id = ? AND group in ?
-- entities_by_id.get_many
SELECT id, group, kind, created, name, submitter, kind_data, description, image FROM {ns}.entities_by_id WHERE id in ? AND group in ?
-- entities_by_id.get_names_kinds_by_ids
SELECT id, name, kind FROM {ns}.entities_by_id WHERE id in ?
-- associations.insert
INSERT INTO {ns}.associations (group, year, bucket, created, direction, kind, source, target, submitter, extra_source, extra_target, submissions_source, submissions_other) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- associations.delete
DELETE FROM {ns}.associations WHERE group in ? AND year = ? AND bucket = ? AND source = ? AND created = ? AND target = ? AND direction = ?
-- network_policies.insert
INSERT INTO {ns}.network_policies (group, name, id, k8s_name, created, ingress, egress, forced_policy, default_policy) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
-- network_policies.delete
DELETE FROM {ns}.network_policies WHERE group in ? AND name = ?
-- network_policies.delete_all_in_group
DELETE FROM {ns}.network_policies WHERE group = ?
-- network_policies_by_name.get
SELECT group, name, id, k8s_name, created, ingress, egress, forced_policy, default_policy FROM {ns}.network_policies_by_name WHERE name = ? AND group in ?
-- network_policies_by_name.get_many
SELECT group, name, id, k8s_name, created, ingress, egress, forced_policy, default_policy FROM {ns}.network_policies_by_name WHERE name in ? AND group in ?
-- network_policies_by_name.exists
SELECT name, group FROM {ns}.network_policies_by_name WHERE name = ? AND group = ? LIMIT 1
-- network_policies_default.get_default
SELECT group, name, id FROM {ns}.network_policies_default WHERE group = ?
-- nodes.insert
INSERT INTO {ns}.nodes (cluster, node, health, resources) VALUES (?, ?, ?, ?)
-- nodes.get
SELECT cluster, node, health, resources, heart_beat FROM {ns}.nodes WHERE cluster = ? AND node = ?
-- nodes.get_many
SELECT cluster, node, health, resources, heart_beat FROM {ns}.nodes WHERE cluster in ? AND node in ?
-- nodes.update
UPDATE {ns}.nodes SET health = ?, resources = ? WHERE cluster = ? AND node = ?
-- nodes.update_heart_beat
UPDATE {ns}.nodes SET health = ?, resources = ?, heart_beat = ?WHERE cluster = ? AND node = ?
-- notifications.insert
INSERT INTO {ns}.notifications (kind, key, created, id, msg, level, ban_id) VALUES (?, ?, ?, ?, ?, ?, ?)
-- notifications.insert_no_expire
INSERT INTO {ns}.notifications (kind, key, created, id, msg, level, ban_id) VALUES (?, ?, ?, ?, ?, ?, ?) USING TTL 0
-- notifications.get
SELECT key, created, id, msg, level, ban_id FROM {ns}.notifications WHERE kind = ? AND key = ?
-- notifications.delete
DELETE FROM {ns}.notifications WHERE kind = ? AND key = ? AND created = ? AND id = ?
-- notifications.delete_all
DELETE FROM {ns}.notifications WHERE kind = ? AND key = ?
-- logs.insert
INSERT INTO {ns}.logs (reaction, stage, bucket, position, line) VALUES (?, ?, ?, ?, ?)
-- logs.get
SELECT position, line FROM {ns}.logs WHERE reaction = ? AND stage = ? AND bucket in ? AND position >= ? PER PARTITION LIMIT ?
-- samples_list.list_ties
SELECT group, sha256, id, uploaded FROM {ns}.samples_list WHERE group = ? AND year = ? AND bucket = ? AND uploaded = ? AND id <= ? LIMIT ?
-- samples_list.list_pull
SELECT group, sha256, id, uploaded FROM {ns}.samples_list WHERE group = ? AND year = ? AND bucket in ? AND uploaded < ? AND uploaded > ? PER PARTITION LIMIT ?
-- repos_list.list_ties
SELECT group, url, id, uploaded FROM {ns}.repos_list WHERE group = ? AND year = ? AND bucket = ? AND uploaded = ? AND id <= ? LIMIT ?
-- repos_list.list_pull
SELECT group, url, id, uploaded FROM {ns}.repos_list WHERE group = ? AND year = ? AND bucket in ? AND uploaded < ? AND uploaded > ? PER PARTITION LIMIT ?
-- committed_repo_data.get_repo_data_count
SELECT repo_data FROM {ns}.committed_repo_data WHERE year = ? AND bucket in ? AND repo_data = ? PER PARTITION LIMIT 1
-- commitish_list.list_ties
SELECT kind, group, key, timestamp FROM {ns}.commitish_list WHERE kind in ? AND group = ? AND year = ? AND bucket = ? AND repo = ? AND timestamp = ? AND key <= ? PER PARTITION LIMIT ?
-- commitish_list.list_pull
SELECT kind, group, key, timestamp FROM {ns}.commitish_list WHERE kind in ? AND group = ? AND year = ? AND bucket in ? AND repo = ? AND timestamp < ? AND timestamp > ? PER PARTITION LIMIT ?
-- entities.list_ties
SELECT kind, group, created, id, name FROM {ns}.entities WHERE kind in ? AND group = ? AND year = ? AND bucket = ? AND created = ? AND id <= ? ORDER BY created DESC, id DESC PER PARTITION LIMIT ?
-- entities.list_pull
SELECT kind, group, created, id, name FROM {ns}.entities WHERE kind in ? AND group = ? AND year = ? AND bucket in ? AND created < ? AND created > ? ORDER BY created DESC, id DESC PER PARTITION LIMIT ?
-- associations.list_ties
SELECT group, kind, source, target, submitter, created, direction, extra_source, extra_target, submissions_source, submissions_other FROM {ns}.associations WHERE group = ? AND year = ? AND bucket = ? AND source = ? AND created = ? AND target > ? LIMIT ?
-- associations.list_pull
SELECT group, kind, source, target, submitter, created, direction, extra_source, extra_target, submissions_source, submissions_other FROM {ns}.associations WHERE group = ? AND year = ? AND bucket in ? AND source = ? AND created < ? AND created > ? PER PARTITION LIMIT ?
-- network_policies.list_pull
SELECT group, name, id FROM {ns}.network_policies WHERE group = ? PER PARTITION LIMIT ?
-- network_policies.list_pull_more
SELECT group, name, id FROM {ns}.network_policies WHERE group = ? AND name > ? PER PARTITION LIMIT ?
-- network_policies.list_ties
SELECT group, name, id FROM {ns}.network_policies WHERE group = ? AND name = ? PER PARTITION LIMIT ?
-- nodes.list
SELECT cluster, node FROM {ns}.nodes WHERE cluster = ? LIMIT ?
-- nodes.list_ties
SELECT cluster, node FROM {ns}.nodes WHERE cluster = ? AND node > ? LIMIT ?
-- nodes.list_details
SELECT cluster, node, health, resources, heart_beat FROM {ns}.nodes WHERE cluster = ? LIMIT ?
-- nodes.list_details_ties
SELECT cluster, node, health, resources, heart_beat FROM {ns}.nodes WHERE cluster = ? AND node > ? LIMIT ?
-- tags.list_ties
SELECT group, item, uploaded FROM {ns}.tags WHERE type = ? AND group = ? AND year = ? AND bucket = ? AND key = ? AND value = ? AND uploaded = ? AND item >= ?
-- tags.list_pull
SELECT group, item, uploaded FROM {ns}.tags WHERE type = ? AND group = ? AND year = ? AND bucket in ? AND key = ? AND value = ? AND uploaded < ? AND uploaded > ?
-- tags_case_insensitive.list_ties
SELECT group, item, uploaded FROM {ns}.tags_case_insensitive WHERE type = ? AND group = ? AND year = ? AND bucket = ? AND key_lower = ? AND value_lower = ? AND uploaded = ? AND item >= ?
-- tags_case_insensitive.list_pull
SELECT group, item, uploaded FROM {ns}.tags_case_insensitive WHERE type = ? AND group = ? AND year = ? AND bucket in ? AND key_lower = ? AND value_lower = ? AND uploaded < ? AND uploaded > ?
-- associations.list_ties_target
SELECT group, kind, source, target, submitter, created, direction, extra_source, extra_target, submissions_source, submissions_other FROM {ns}.associations WHERE group = ? AND year = ? AND bucket = ? AND source = ? AND created = ? AND target = ? AND direction >= ?
-- samples_list.census
SELECT group, year, bucket, count(*) FROM {ns}.samples_list WHERE token(group, year, bucket) >= ? AND token(group, year, bucket) <= ? GROUP BY group, year, bucket
-- repos_list.census
SELECT group, year, bucket, count(*) FROM {ns}.repos_list WHERE token(group, year, bucket) >= ? AND token(group, year, bucket) <= ? GROUP BY group, year, bucket
-- commitish_list.census
SELECT kind, group, year, bucket, repo, count(*) FROM {ns}.commitish_list WHERE token(kind, group, year, bucket, repo) >= ? AND token(kind, group, year, bucket, repo) <= ? GROUP BY kind, group, year, bucket, repo
-- tags.census
SELECT type, group, year, bucket, key, value, count(*) FROM {ns}.tags WHERE token(type, group, year, bucket, key, value) >= ? AND token(type, group, year, bucket, key, value) <= ? GROUP BY type, group, year, bucket, key, value
-- tags_case_insensitive.census
SELECT type, group, year, bucket, key_lower, value_lower, count(*) FROM {ns}.tags_case_insensitive WHERE token(type, group, year, bucket, key_lower, value_lower) >= ? AND token(type, group, year, bucket, key_lower, value_lower) <= ? GROUP BY type, group, year, bucket, key_lower, value_lower
-- entities.census
SELECT kind, group, year, bucket, count(*) FROM {ns}.entities WHERE token(kind, group, year, bucket) >= ? AND token(kind, group, year, bucket) <= ? GROUP BY kind, group, year, bucket
-- associations.census
SELECT group, year, bucket, source, count(*) FROM {ns}.associations WHERE token(group, year, bucket, source) >= ? AND token(group, year, bucket, source) <= ? GROUP BY group, year, bucket, source
-- notifications.backup
SELECT kind, key, created, id, msg, level, ban_id, TTL(msg) FROM {ns}.notifications WHERE token(kind, key) >= ? AND token(kind, key) <= ?
-- notifications.restore
INSERT INTO {ns}.notifications (kind, key, created, id, msg, level, ban_id) VALUES (?, ?, ?, ?, ?, ?, ?) USING TTL ?
-- tags.backup
Select type, group, year, bucket, key, value, uploaded, item From {ns}.tags Where token(type, group, year, bucket, key, value) >= ? AND token(type, group, year, bucket, key, value) <= ?
