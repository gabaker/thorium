-- schema_migrations.INSERT
INSERT INTO {ns}.schema_migrations (kind, id, fingerprint, apply_key, layouts, revision, version, entry, result, value, status, attempt, lease, started, finished, job, backup, counts, report, updated) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- schema_migrations.SELECT_ALL
SELECT kind, id, fingerprint, apply_key, layouts, revision, version, entry, result, value, status, attempt, lease, started, finished, job, backup, counts, report, updated FROM {ns}.schema_migrations
-- schema_migrations.SCAN
SELECT kind, id, fingerprint, apply_key, layouts, revision, version, entry, result, value, status, attempt, lease, started, finished, job, backup, counts, report, updated FROM {ns}.schema_migrations WHERE token(kind) >= ? AND token(kind) <= ?
-- schema_migrations.CENSUS
SELECT kind, count(*) AS count FROM {ns}.schema_migrations WHERE token(kind) >= ? AND token(kind) <= ? GROUP BY kind
-- schema_migration_issues.INSERT
INSERT INTO {ns}.schema_migration_issues (migration, transform, reason, worker, attempt, checkpoint, lease, class, count, first_seen, last_seen, samples, resolved_count, accepted, updated) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- schema_migration_issues.SELECT_ALL
SELECT migration, transform, reason, worker, attempt, checkpoint, lease, class, count, first_seen, last_seen, samples, resolved_count, accepted, updated FROM {ns}.schema_migration_issues
-- schema_migration_issues.SCAN
SELECT migration, transform, reason, worker, attempt, checkpoint, lease, class, count, first_seen, last_seen, samples, resolved_count, accepted, updated FROM {ns}.schema_migration_issues WHERE token(migration) >= ? AND token(migration) <= ?
-- schema_migration_issues.CENSUS
SELECT migration, count(*) AS count FROM {ns}.schema_migration_issues WHERE token(migration) >= ? AND token(migration) <= ? GROUP BY migration
-- samples_list.INSERT
INSERT INTO {ns}.samples_list (group, year, bucket, uploaded, id, sha256, sha1, md5, name, description, submitter, origin) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- samples_list.SELECT_ALL
SELECT group, year, bucket, uploaded, id, sha256, sha1, md5, name, description, submitter, origin FROM {ns}.samples_list
-- samples_list.SCAN
SELECT group, year, bucket, uploaded, id, sha256, sha1, md5, name, description, submitter, origin FROM {ns}.samples_list WHERE token(group, year, bucket) >= ? AND token(group, year, bucket) <= ?
-- samples_list.CENSUS
SELECT group, year, bucket, count(*) AS count FROM {ns}.samples_list WHERE token(group, year, bucket) >= ? AND token(group, year, bucket) <= ? GROUP BY group, year, bucket
-- s3_ids.INSERT
INSERT INTO {ns}.s3_ids (type, id, sha256) VALUES (?, ?, ?)
-- s3_ids.SELECT_ALL
SELECT type, id, sha256 FROM {ns}.s3_ids
-- s3_ids.SCAN
SELECT type, id, sha256 FROM {ns}.s3_ids WHERE token(type, id) >= ? AND token(type, id) <= ?
-- s3_ids.CENSUS
SELECT type, id, count(*) AS count FROM {ns}.s3_ids WHERE token(type, id) >= ? AND token(type, id) <= ? GROUP BY type, id
-- comments.INSERT
INSERT INTO {ns}.comments (group, sha256, uploaded, id, author, comment, files) VALUES (?, ?, ?, ?, ?, ?, ?)
-- comments.SELECT_ALL
SELECT group, sha256, uploaded, id, author, comment, files FROM {ns}.comments
-- comments.SCAN
SELECT group, sha256, uploaded, id, author, comment, files FROM {ns}.comments WHERE token(group, sha256) >= ? AND token(group, sha256) <= ?
-- comments.CENSUS
SELECT group, sha256, count(*) AS count FROM {ns}.comments WHERE token(group, sha256) >= ? AND token(group, sha256) <= ? GROUP BY group, sha256
-- repos_list.INSERT
INSERT INTO {ns}.repos_list (group, year, bucket, uploaded, id, url, provider, user, name, creator, scheme, default_checkout, earliest) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- repos_list.SELECT_ALL
SELECT group, year, bucket, uploaded, id, url, provider, user, name, creator, scheme, default_checkout, earliest FROM {ns}.repos_list
-- repos_list.SCAN
SELECT group, year, bucket, uploaded, id, url, provider, user, name, creator, scheme, default_checkout, earliest FROM {ns}.repos_list WHERE token(group, year, bucket) >= ? AND token(group, year, bucket) <= ?
-- repos_list.CENSUS
SELECT group, year, bucket, count(*) AS count FROM {ns}.repos_list WHERE token(group, year, bucket) >= ? AND token(group, year, bucket) <= ? GROUP BY group, year, bucket
-- repo_data.INSERT
INSERT INTO {ns}.repo_data (repo, hash) VALUES (?, ?)
-- repo_data.SELECT_ALL
SELECT repo, hash FROM {ns}.repo_data
-- repo_data.SCAN
SELECT repo, hash FROM {ns}.repo_data WHERE token(repo) >= ? AND token(repo) <= ?
-- repo_data.CENSUS
SELECT repo, count(*) AS count FROM {ns}.repo_data WHERE token(repo) >= ? AND token(repo) <= ? GROUP BY repo
-- commitishes.INSERT
INSERT INTO {ns}.commitishes (kind, group, repo, key, timestamp, data, repo_data) VALUES (?, ?, ?, ?, ?, ?, ?)
-- commitishes.SELECT_ALL
SELECT kind, group, repo, key, timestamp, data, repo_data FROM {ns}.commitishes
-- commitishes.SCAN
SELECT kind, group, repo, key, timestamp, data, repo_data FROM {ns}.commitishes WHERE token(kind, group, repo, key) >= ? AND token(kind, group, repo, key) <= ?
-- commitishes.CENSUS
SELECT kind, group, repo, key, count(*) AS count FROM {ns}.commitishes WHERE token(kind, group, repo, key) >= ? AND token(kind, group, repo, key) <= ? GROUP BY kind, group, repo, key
-- commitish_list.INSERT
INSERT INTO {ns}.commitish_list (kind, group, year, bucket, repo, timestamp, key, repo_data) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
-- commitish_list.SELECT_ALL
SELECT kind, group, year, bucket, repo, timestamp, key, repo_data FROM {ns}.commitish_list
-- commitish_list.SCAN
SELECT kind, group, year, bucket, repo, timestamp, key, repo_data FROM {ns}.commitish_list WHERE token(kind, group, year, bucket, repo) >= ? AND token(kind, group, year, bucket, repo) <= ?
-- commitish_list.CENSUS
SELECT kind, group, year, bucket, repo, count(*) AS count FROM {ns}.commitish_list WHERE token(kind, group, year, bucket, repo) >= ? AND token(kind, group, year, bucket, repo) <= ? GROUP BY kind, group, year, bucket, repo
-- results.INSERT
INSERT INTO {ns}.results (id, uploaded, tool, tool_version, cmd, result, files, display_type, children, entities) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- results.SELECT_ALL
SELECT id, uploaded, tool, tool_version, cmd, result, files, display_type, children, entities FROM {ns}.results
-- results.SCAN
SELECT id, uploaded, tool, tool_version, cmd, result, files, display_type, children, entities FROM {ns}.results WHERE token(id) >= ? AND token(id) <= ?
-- results.CENSUS
SELECT id, count(*) AS count FROM {ns}.results WHERE token(id) >= ? AND token(id) <= ? GROUP BY id
-- results_stream.INSERT
INSERT INTO {ns}.results_stream (kind, group, year, bucket, uploaded, id, key, tool, tool_version, display_type, cmd) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- results_stream.SELECT_ALL
SELECT kind, group, year, bucket, uploaded, id, key, tool, tool_version, display_type, cmd FROM {ns}.results_stream
-- results_stream.SCAN
SELECT kind, group, year, bucket, uploaded, id, key, tool, tool_version, display_type, cmd FROM {ns}.results_stream WHERE token(kind, group, year, bucket) >= ? AND token(kind, group, year, bucket) <= ?
-- results_stream.CENSUS
SELECT kind, group, year, bucket, count(*) AS count FROM {ns}.results_stream WHERE token(kind, group, year, bucket) >= ? AND token(kind, group, year, bucket) <= ? GROUP BY kind, group, year, bucket
-- tags.INSERT
INSERT INTO {ns}.tags (type, group, year, bucket, key, value, uploaded, item, key_lower, value_lower) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- tags.SELECT_ALL
SELECT type, group, year, bucket, key, value, uploaded, item, key_lower, value_lower FROM {ns}.tags
-- tags.SCAN
SELECT type, group, year, bucket, key, value, uploaded, item, key_lower, value_lower FROM {ns}.tags WHERE token(type, group, year, bucket, key, value) >= ? AND token(type, group, year, bucket, key, value) <= ?
-- tags.CENSUS
SELECT type, group, year, bucket, key, value, count(*) AS count FROM {ns}.tags WHERE token(type, group, year, bucket, key, value) >= ? AND token(type, group, year, bucket, key, value) <= ? GROUP BY type, group, year, bucket, key, value
-- entities.INSERT
INSERT INTO {ns}.entities (kind, group, year, bucket, created, id, name, submitter, kind_data, description, image) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- entities.SELECT_ALL
SELECT kind, group, year, bucket, created, id, name, submitter, kind_data, description, image FROM {ns}.entities
-- entities.SCAN
SELECT kind, group, year, bucket, created, id, name, submitter, kind_data, description, image FROM {ns}.entities WHERE token(kind, group, year, bucket) >= ? AND token(kind, group, year, bucket) <= ?
-- entities.CENSUS
SELECT kind, group, year, bucket, count(*) AS count FROM {ns}.entities WHERE token(kind, group, year, bucket) >= ? AND token(kind, group, year, bucket) <= ? GROUP BY kind, group, year, bucket
-- associations.INSERT
INSERT INTO {ns}.associations (group, year, bucket, source, created, target, direction, kind, submitter, extra_source, extra_target, submissions_source, submissions_other) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- associations.SELECT_ALL
SELECT group, year, bucket, source, created, target, direction, kind, submitter, extra_source, extra_target, submissions_source, submissions_other FROM {ns}.associations
-- associations.SCAN
SELECT group, year, bucket, source, created, target, direction, kind, submitter, extra_source, extra_target, submissions_source, submissions_other FROM {ns}.associations WHERE token(group, year, bucket, source) >= ? AND token(group, year, bucket, source) <= ?
-- associations.CENSUS
SELECT group, year, bucket, source, count(*) AS count FROM {ns}.associations WHERE token(group, year, bucket, source) >= ? AND token(group, year, bucket, source) <= ? GROUP BY group, year, bucket, source
-- network_policies.INSERT
INSERT INTO {ns}.network_policies (group, name, id, k8s_name, created, ingress, egress, forced_policy, default_policy) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
-- network_policies.SELECT_ALL
SELECT group, name, id, k8s_name, created, ingress, egress, forced_policy, default_policy FROM {ns}.network_policies
-- network_policies.SCAN
SELECT group, name, id, k8s_name, created, ingress, egress, forced_policy, default_policy FROM {ns}.network_policies WHERE token(group) >= ? AND token(group) <= ?
-- network_policies.CENSUS
SELECT group, count(*) AS count FROM {ns}.network_policies WHERE token(group) >= ? AND token(group) <= ? GROUP BY group
-- nodes.INSERT
INSERT INTO {ns}.nodes (cluster, node, health, resources, heart_beat) VALUES (?, ?, ?, ?, ?)
-- nodes.SELECT_ALL
SELECT cluster, node, health, resources, heart_beat FROM {ns}.nodes
-- nodes.SCAN
SELECT cluster, node, health, resources, heart_beat FROM {ns}.nodes WHERE token(cluster) >= ? AND token(cluster) <= ?
-- nodes.CENSUS
SELECT cluster, count(*) AS count FROM {ns}.nodes WHERE token(cluster) >= ? AND token(cluster) <= ? GROUP BY cluster
-- notifications.INSERT
INSERT INTO {ns}.notifications (kind, key, created, id, msg, level, ban_id) VALUES (?, ?, ?, ?, ?, ?, ?)
-- notifications.SELECT_ALL
SELECT kind, key, created, id, msg, level, ban_id FROM {ns}.notifications
-- notifications.SCAN
SELECT kind, key, created, id, msg, level, ban_id FROM {ns}.notifications WHERE token(kind, key) >= ? AND token(kind, key) <= ?
-- notifications.CENSUS
SELECT kind, key, count(*) AS count FROM {ns}.notifications WHERE token(kind, key) >= ? AND token(kind, key) <= ? GROUP BY kind, key
-- logs.INSERT
INSERT INTO {ns}.logs (reaction, stage, bucket, position, line) VALUES (?, ?, ?, ?, ?)
-- logs.SELECT_ALL
SELECT reaction, stage, bucket, position, line FROM {ns}.logs
-- logs.SCAN
SELECT reaction, stage, bucket, position, line FROM {ns}.logs WHERE token(reaction, stage, bucket) >= ? AND token(reaction, stage, bucket) <= ?
-- logs.CENSUS
SELECT reaction, stage, bucket, count(*) AS count FROM {ns}.logs WHERE token(reaction, stage, bucket) >= ? AND token(reaction, stage, bucket) <= ? GROUP BY reaction, stage, bucket
-- samples_list.insert
INSERT INTO {ns}.samples_list (group, year, bucket, uploaded, id, sha256, sha1, md5, name, description, submitter, origin) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- samples_list.delete_multiple_groups
DELETE FROM {ns}.samples_list WHERE group IN ? AND year = ? AND bucket = ? AND uploaded = ? AND id = ?
-- samples_list.list_ties
SELECT group, sha256, id, uploaded FROM {ns}.samples_list WHERE group = ? AND year = ? AND bucket = ? AND uploaded = ? AND id <= ? LIMIT ?
-- samples_list.list_pull
SELECT group, sha256, id, uploaded FROM {ns}.samples_list WHERE group = ? AND year = ? AND bucket IN ? AND uploaded < ? AND uploaded > ? PER PARTITION LIMIT ?
-- samples_list.census
SELECT group, year, bucket, count(*) AS count FROM {ns}.samples_list WHERE token(group, year, bucket) >= ? AND token(group, year, bucket) <= ? GROUP BY group, year, bucket
-- samples.get
SELECT sha256, sha1, md5, id, name, description, group, submitter, origin, uploaded FROM {ns}.samples WHERE sha256 = ? AND group IN ?
-- samples.get_many
SELECT sha256, sha1, md5, id, name, description, group, submitter, origin, uploaded FROM {ns}.samples WHERE sha256 IN ? AND group IN ?
-- samples.auth
SELECT sha256 FROM {ns}.samples WHERE sha256 IN ? AND group IN ? GROUP BY sha256
-- samples.get_basic_submission_info
SELECT group, submitter, id FROM {ns}.samples WHERE sha256 = ?
-- s3_ids.insert
INSERT INTO {ns}.s3_ids (type, id, sha256) VALUES (?, ?, ?)
-- s3_ids.id_exists
SELECT type, id FROM {ns}.s3_ids WHERE type = ? AND id = ? LIMIT 1
-- s3_ids.delete
DELETE FROM {ns}.s3_ids WHERE type = ? AND id = ?
-- s3_sha256s.object_exists
SELECT sha256, type FROM {ns}.s3_sha256s WHERE sha256 = ? AND type = ? LIMIT 1
-- s3_sha256s.get
SELECT id FROM {ns}.s3_sha256s WHERE sha256 = ? AND type = ? LIMIT 1
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
-- repos_list.list_ties
SELECT group, url, id, uploaded FROM {ns}.repos_list WHERE group = ? AND year = ? AND bucket = ? AND uploaded = ? AND id <= ? LIMIT ?
-- repos_list.list_pull
SELECT group, url, id, uploaded FROM {ns}.repos_list WHERE group = ? AND year = ? AND bucket IN ? AND uploaded < ? AND uploaded > ? PER PARTITION LIMIT ?
-- repos_list.census
SELECT group, year, bucket, count(*) AS count FROM {ns}.repos_list WHERE token(group, year, bucket) >= ? AND token(group, year, bucket) <= ? GROUP BY group, year, bucket
-- repos.get
SELECT group, provider, user, name, url, id, creator, uploaded, scheme, default_checkout, earliest FROM {ns}.repos WHERE url = ? AND group IN ?
-- repos.get_many
SELECT group, provider, user, name, url, id, creator, uploaded, scheme, default_checkout, earliest FROM {ns}.repos WHERE url IN ? AND group IN ?
-- repos.auth
SELECT url FROM {ns}.repos WHERE url IN ? AND group IN ? GROUP BY url PER PARTITION LIMIT 1
-- repo_data.insert_data
INSERT INTO {ns}.repo_data (repo, hash) VALUES (?, ?)
-- repo_data.get_data
SELECT hash FROM {ns}.repo_data WHERE repo = ?
-- repo_data.data_exists
SELECT repo, hash FROM {ns}.repo_data WHERE repo = ? AND hash = ? LIMIT 1
-- repo_data.delete_data
DELETE FROM {ns}.repo_data WHERE repo = ? AND hash = ?
-- commitishes.insert
INSERT INTO {ns}.commitishes (kind, group, repo, key, timestamp, data, repo_data) VALUES (?, ?, ?, ?, ?, ?, ?)
-- commitishes.get_data
SELECT data FROM {ns}.commitishes WHERE kind = ? AND group IN ? AND repo = ? AND key = ? LIMIT 1
-- commitishes.get_repo_data
SELECT kind, repo_data FROM {ns}.commitishes WHERE kind IN ? AND group IN ? AND repo = ? AND key = ?
-- commitish_list.insert_list
INSERT INTO {ns}.commitish_list (kind, group, year, bucket, repo, timestamp, key, repo_data) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
-- commitish_list.list_ties
SELECT kind, group, key, timestamp FROM {ns}.commitish_list WHERE kind = ? AND group = ? AND year = ? AND bucket = ? AND repo = ? AND timestamp = ? AND key <= ? LIMIT ?
-- commitish_list.list_pull
SELECT kind, group, key, timestamp FROM {ns}.commitish_list WHERE kind IN ? AND group = ? AND year = ? AND bucket IN ? AND repo = ? AND timestamp < ? AND timestamp > ? PER PARTITION LIMIT ?
-- commitish_list.census
SELECT kind, group, year, bucket, repo, count(*) AS count FROM {ns}.commitish_list WHERE token(kind, group, year, bucket, repo) >= ? AND token(kind, group, year, bucket, repo) <= ? GROUP BY kind, group, year, bucket, repo
-- committed_repo_data.get_repo_data_count
SELECT repo_data FROM {ns}.committed_repo_data WHERE repo_data = ? AND year = ? AND bucket IN ? PER PARTITION LIMIT 1
-- results.insert
INSERT INTO {ns}.results (id, uploaded, tool, tool_version, cmd, result, files, entities, display_type) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
-- results.get
SELECT id, tool, tool_version, cmd, uploaded, result, files, entities, display_type, children FROM {ns}.results WHERE id IN ?
-- results.get_uploaded_by_id
SELECT id, uploaded FROM {ns}.results WHERE id IN ?
-- results.delete
DELETE FROM {ns}.results WHERE id = ?
-- results.update_children
UPDATE {ns}.results SET children[?] = ? WHERE id = ? AND uploaded = ?
-- results_stream.insert_stream
INSERT INTO {ns}.results_stream (kind, group, year, bucket, uploaded, id, key, tool, tool_version, display_type, cmd) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- results_stream.delete_stream
DELETE FROM {ns}.results_stream WHERE kind = ? AND group = ? AND year = ? AND bucket = ? AND uploaded = ? AND id = ?
-- results_auth.get_id
SELECT id FROM {ns}.results_auth WHERE key = ? AND kind = ? AND group IN ? AND tool = ?
-- results_auth.get_with_key
SELECT id, tool, display_type, cmd, group, uploaded FROM {ns}.results_auth WHERE key = ? AND kind = ? AND group IN ?
-- results_auth.get_with_key_and_tool
SELECT id, tool, display_type, cmd, group, uploaded FROM {ns}.results_auth WHERE key = ? AND kind = ? AND group IN ? AND tool IN ?
-- results_ids.count
SELECT id FROM {ns}.results_ids WHERE key = ? AND id = ? AND kind = ?
-- tags.insert
INSERT INTO {ns}.tags (type, group, year, bucket, key, value, uploaded, item, key_lower, value_lower) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- tags.delete
DELETE FROM {ns}.tags WHERE type = ? AND group = ? AND year = ? AND bucket = ? AND key = ? AND value = ? AND uploaded = ? AND item = ?
-- tags.list_ties
SELECT group, item, uploaded FROM {ns}.tags WHERE type = ? AND group = ? AND year = ? AND bucket = ? AND key = ? AND value = ? AND uploaded = ? AND item >= ?
-- tags.list_pull
SELECT group, item, uploaded FROM {ns}.tags WHERE type = ? AND group = ? AND year = ? AND bucket IN ? AND key = ? AND value = ? AND uploaded < ? AND uploaded > ?
-- tags.census
SELECT type, group, year, bucket, key, value, count(*) AS count FROM {ns}.tags WHERE token(type, group, year, bucket, key, value) >= ? AND token(type, group, year, bucket, key, value) <= ? GROUP BY type, group, year, bucket, key, value
-- tags.backup
SELECT type, group, year, bucket, key, value, uploaded, item FROM {ns}.tags WHERE token(type, group, year, bucket, key, value) >= ? AND token(type, group, year, bucket, key, value) <= ?
-- tags_by_item.get
SELECT group, item, key, value FROM {ns}.tags_by_item WHERE type = ? AND item = ? AND group IN ?
-- tags_by_item.get_rows
SELECT group, year, bucket, key, value, uploaded, item FROM {ns}.tags_by_item WHERE type = ? AND item = ? AND group IN ?
-- tags_case_insensitive.list_ties
SELECT group, item, uploaded FROM {ns}.tags_case_insensitive WHERE type = ? AND group = ? AND year = ? AND bucket = ? AND key_lower = ? AND value_lower = ? AND uploaded = ? AND item >= ?
-- tags_case_insensitive.list_pull
SELECT group, item, uploaded FROM {ns}.tags_case_insensitive WHERE type = ? AND group = ? AND year = ? AND bucket IN ? AND key_lower = ? AND value_lower = ? AND uploaded < ? AND uploaded > ?
-- tags_case_insensitive.census
SELECT type, group, year, bucket, key_lower, value_lower, count(*) AS count FROM {ns}.tags_case_insensitive WHERE token(type, group, year, bucket, key_lower, value_lower) >= ? AND token(type, group, year, bucket, key_lower, value_lower) <= ? GROUP BY type, group, year, bucket, key_lower, value_lower
-- entities.insert
INSERT INTO {ns}.entities (kind, group, year, bucket, created, id, name, submitter, kind_data, description, image) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- entities.delete
DELETE FROM {ns}.entities WHERE kind = ? AND group IN ? AND year = ? AND bucket = ? AND created = ? AND id = ?
-- entities.list_ties
SELECT kind, group, created, id, name FROM {ns}.entities WHERE kind = ? AND group = ? AND year = ? AND bucket = ? AND created = ? AND id <= ? ORDER BY created DESC, id DESC LIMIT ?
-- entities.list_pull
SELECT kind, group, created, id, name FROM {ns}.entities WHERE kind IN ? AND group = ? AND year = ? AND bucket IN ? AND created < ? AND created > ? ORDER BY created DESC, id DESC PER PARTITION LIMIT ?
-- entities.census
SELECT kind, group, year, bucket, count(*) AS count FROM {ns}.entities WHERE token(kind, group, year, bucket) >= ? AND token(kind, group, year, bucket) <= ? GROUP BY kind, group, year, bucket
-- entities_by_id.get
SELECT id, group, kind, created, name, submitter, kind_data, description, image FROM {ns}.entities_by_id WHERE id = ? AND group IN ?
-- entities_by_id.get_many
SELECT id, group, kind, created, name, submitter, kind_data, description, image FROM {ns}.entities_by_id WHERE id IN ? AND group IN ?
-- entities_by_id.all_groups
SELECT group FROM {ns}.entities_by_id WHERE id = ?
-- entities_by_id.get_groups_kinds_by_ids
SELECT id, group, kind FROM {ns}.entities_by_id WHERE id IN ?
-- entities_by_id.get_names_kinds_by_ids
SELECT id, name, kind FROM {ns}.entities_by_id WHERE id IN ?
-- associations.insert
INSERT INTO {ns}.associations (group, year, bucket, source, created, target, direction, kind, submitter, extra_source, extra_target, submissions_source, submissions_other) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- associations.delete
DELETE FROM {ns}.associations WHERE group IN ? AND year = ? AND bucket = ? AND source = ? AND created = ? AND target = ? AND direction = ?
-- associations.list_ties
SELECT group, kind, source, target, submitter, created, direction, extra_source, extra_target, submissions_source, submissions_other FROM {ns}.associations WHERE group = ? AND year = ? AND bucket = ? AND source = ? AND created = ? AND target > ? LIMIT ?
-- associations.list_ties_target
SELECT group, kind, source, target, submitter, created, direction, extra_source, extra_target, submissions_source, submissions_other FROM {ns}.associations WHERE group = ? AND year = ? AND bucket = ? AND source = ? AND created = ? AND target = ? AND direction >= ? LIMIT ?
-- associations.list_pull
SELECT group, kind, source, target, submitter, created, direction, extra_source, extra_target, submissions_source, submissions_other FROM {ns}.associations WHERE group = ? AND year = ? AND bucket IN ? AND source = ? AND created < ? AND created > ? PER PARTITION LIMIT ?
-- associations.census
SELECT group, year, bucket, source, count(*) AS count FROM {ns}.associations WHERE token(group, year, bucket, source) >= ? AND token(group, year, bucket, source) <= ? GROUP BY group, year, bucket, source
-- network_policies.insert
INSERT INTO {ns}.network_policies (group, name, id, k8s_name, created, ingress, egress, forced_policy, default_policy) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
-- network_policies.delete
DELETE FROM {ns}.network_policies WHERE group IN ? AND name = ?
-- network_policies.delete_all_in_group
DELETE FROM {ns}.network_policies WHERE group = ?
-- network_policies.list_pull
SELECT group, name, id FROM {ns}.network_policies WHERE group = ? PER PARTITION LIMIT ?
-- network_policies.list_pull_more
SELECT group, name, id FROM {ns}.network_policies WHERE group = ? AND name > ? PER PARTITION LIMIT ?
-- network_policies.list_ties
SELECT group, name, id FROM {ns}.network_policies WHERE group = ? AND name = ? PER PARTITION LIMIT ?
-- network_policies_by_name.get
SELECT group, name, id, k8s_name, created, ingress, egress, forced_policy, default_policy FROM {ns}.network_policies_by_name WHERE name = ? AND group IN ?
-- network_policies_by_name.get_many
SELECT group, name, id, k8s_name, created, ingress, egress, forced_policy, default_policy FROM {ns}.network_policies_by_name WHERE name IN ? AND group IN ?
-- network_policies_by_name.exists
SELECT name, group FROM {ns}.network_policies_by_name WHERE name = ? AND group = ? LIMIT 1
-- network_policies_default.get_default
SELECT group, name, id FROM {ns}.network_policies_default WHERE group = ?
-- nodes.insert
INSERT INTO {ns}.nodes (cluster, node, health, resources) VALUES (?, ?, ?, ?)
-- nodes.get
SELECT cluster, node, health, resources, heart_beat FROM {ns}.nodes WHERE cluster = ? AND node = ?
-- nodes.get_many
SELECT cluster, node, health, resources, heart_beat FROM {ns}.nodes WHERE cluster IN ? AND node IN ?
-- nodes.update
UPDATE {ns}.nodes SET health = ?, resources = ? WHERE cluster = ? AND node = ?
-- nodes.update_heart_beat
UPDATE {ns}.nodes SET health = ?, resources = ?, heart_beat = ? WHERE cluster = ? AND node = ?
-- nodes.list
SELECT cluster, node FROM {ns}.nodes WHERE cluster = ? LIMIT ?
-- nodes.list_ties
SELECT cluster, node FROM {ns}.nodes WHERE cluster = ? AND node > ? LIMIT ?
-- nodes.list_details
SELECT cluster, node, health, resources, heart_beat FROM {ns}.nodes WHERE cluster = ? LIMIT ?
-- nodes.list_details_ties
SELECT cluster, node, health, resources, heart_beat FROM {ns}.nodes WHERE cluster = ? AND node > ? LIMIT ?
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
-- notifications.backup
SELECT kind, key, created, id, msg, level, ban_id, TTL(msg) AS ttl FROM {ns}.notifications WHERE token(kind, key) >= ? AND token(kind, key) <= ?
-- notifications.restore
INSERT INTO {ns}.notifications (kind, key, created, id, msg, level, ban_id) VALUES (?, ?, ?, ?, ?, ?, ?) USING TTL ?
-- logs.insert
INSERT INTO {ns}.logs (reaction, stage, bucket, position, line) VALUES (?, ?, ?, ?, ?)
-- logs.get
SELECT position, line FROM {ns}.logs WHERE reaction = ? AND stage = ? AND bucket IN ? AND position >= ? PER PARTITION LIMIT ?
-- schema_migrations.insert
INSERT INTO {ns}.schema_migrations (kind, id, fingerprint, apply_key, layouts, revision, version, entry, result, value, status, attempt, lease, started, finished, job, backup, counts, report, updated) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- schema_migrations.insert_expiring
INSERT INTO {ns}.schema_migrations (kind, id, fingerprint, apply_key, layouts, revision, version, entry, result, value, status, attempt, lease, started, finished, job, backup, counts, report, updated) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) USING TTL ?
-- schema_migrations.get
SELECT kind, id, fingerprint, apply_key, layouts, revision, version, entry, result, value, status, attempt, lease, started, finished, job, backup, counts, report, updated FROM {ns}.schema_migrations WHERE kind = ? AND id = ?
-- schema_migrations.list
SELECT kind, id, fingerprint, apply_key, layouts, revision, version, entry, result, value, status, attempt, lease, started, finished, job, backup, counts, report, updated FROM {ns}.schema_migrations WHERE kind = ?
-- schema_migrations.delete
DELETE FROM {ns}.schema_migrations WHERE kind = ? AND id = ?
-- schema_migrations.delete_kind
DELETE FROM {ns}.schema_migrations WHERE kind = ?
-- schema_migration_issues.insert
INSERT INTO {ns}.schema_migration_issues (migration, transform, reason, worker, attempt, checkpoint, lease, class, count, first_seen, last_seen, samples, resolved_count, accepted, updated) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- schema_migration_issues.list
SELECT migration, transform, reason, worker, attempt, checkpoint, lease, class, count, first_seen, last_seen, samples, resolved_count, accepted, updated FROM {ns}.schema_migration_issues WHERE migration = ?
-- schema_migration_issues.get_group
SELECT migration, transform, reason, worker, attempt, checkpoint, lease, class, count, first_seen, last_seen, samples, resolved_count, accepted, updated FROM {ns}.schema_migration_issues WHERE migration = ? AND transform = ? AND reason = ?
-- schema_migration_issues.update_worker
UPDATE {ns}.schema_migration_issues SET attempt = ?, checkpoint = ?, lease = ?, updated = ? WHERE migration = ? AND transform = ? AND reason = ? AND worker = ?
-- schema_migration_issues.update_issue
UPDATE {ns}.schema_migration_issues SET attempt = ?, class = ?, count = ?, samples = ?, first_seen = ?, last_seen = ?, updated = ? WHERE migration = ? AND transform = ? AND reason = ? AND worker = ?
-- schema_migration_issues.resolve
UPDATE {ns}.schema_migration_issues SET count = ?, resolved_count = ?, updated = ? WHERE migration = ? AND transform = ? AND reason = ? AND worker = ?
-- schema_migration_issues.accept
UPDATE {ns}.schema_migration_issues SET accepted = ?, updated = ? WHERE migration = ? AND transform = ? AND reason = ? AND worker = ?
-- schema_migration_issues.delete_pass
DELETE FROM {ns}.schema_migration_issues WHERE migration = ?
