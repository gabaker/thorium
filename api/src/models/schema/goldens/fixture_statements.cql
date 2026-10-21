-- fixture_samples.insert
INSERT INTO {ns}.fixture_samples (group, year, bucket, uploaded, id, sha256, sha1, md5, name, description, submitter, origin) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
-- fixture_samples.select_all
SELECT group, year, bucket, uploaded, id, sha256, sha1, md5, name, description, submitter, origin FROM {ns}.fixture_samples
-- fixture_samples.scan
SELECT group, year, bucket, uploaded, id, sha256, sha1, md5, name, description, submitter, origin FROM {ns}.fixture_samples WHERE token(group, year, bucket) >= ? AND token(group, year, bucket) <= ?
-- fixture_samples.census
SELECT group, year, bucket, count(*) AS count FROM {ns}.fixture_samples WHERE token(group, year, bucket) >= ? AND token(group, year, bucket) <= ? GROUP BY group, year, bucket
-- fixture_counts.insert
INSERT INTO {ns}.fixture_counts (id, total, per_kind) VALUES (?, ?, ?)
-- fixture_counts.select_all
SELECT id, total, per_kind FROM {ns}.fixture_counts
-- fixture_counts.scan
SELECT id, total, per_kind FROM {ns}.fixture_counts WHERE token(id) >= ? AND token(id) <= ?
-- fixture_counts.census
SELECT id, count(*) AS count FROM {ns}.fixture_counts WHERE token(id) >= ? AND token(id) <= ? GROUP BY id
-- fixture_secrets.insert
INSERT INTO {ns}.fixture_secrets (group, url, password) VALUES (?, ?, ?)
-- fixture_secrets.select_all
SELECT group, url, password FROM {ns}.fixture_secrets
-- fixture_secrets.scan
SELECT group, url, password FROM {ns}.fixture_secrets WHERE token(group) >= ? AND token(group) <= ?
-- fixture_secrets.census
SELECT group, count(*) AS count FROM {ns}.fixture_secrets WHERE token(group) >= ? AND token(group) <= ? GROUP BY group
