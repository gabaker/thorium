CREATE TABLE IF NOT EXISTS {ns}.associations ( group TEXT, year INT, bucket INT, created TIMESTAMP, direction TEXT, kind TEXT, source TEXT, target TEXT, submitter TEXT, extra_source TEXT, extra_target TEXT, submissions_source UUID, submissions_other UUID, PRIMARY KEY ((group, year, bucket, source), created, target, direction))
          WITH CLUSTERING ORDER BY (created DESC);
CREATE TABLE IF NOT EXISTS {ns}.entities (kind TEXT,
            group TEXT,
            year INT,
            bucket INT,
            created TIMESTAMP,
            id UUID,
            name TEXT,
            submitter TEXT,
            kind_data TEXT,
            description TEXT,
            image TEXT,
            PRIMARY KEY ((kind, group, year, bucket), created, id));
CREATE MATERIALIZED VIEW IF NOT EXISTS {ns}.entities_by_id AS SELECT id, group, kind, created, year, bucket, name, submitter, kind_data, description, image FROM {ns}.entities WHERE id IS NOT NULL AND group IS NOT NULL AND kind IS NOT NULL AND created IS NOT NULL AND year IS NOT NULL AND bucket IS NOT NULL AND name IS NOT NULL
            PRIMARY KEY (id, group, kind, created, year, bucket, name);
CREATE TABLE IF NOT EXISTS {ns}.comments (group TEXT,
            sha256 TEXT,
            uploaded TIMESTAMP,
            id UUID,
            author TEXT,
            comment TEXT,
            files TEXT,
            PRIMARY KEY ((group, sha256), uploaded, id)) WITH CLUSTERING ORDER BY (uploaded ASC);
CREATE MATERIALIZED VIEW IF NOT EXISTS {ns}.comments_by_id AS SELECT group, sha256, uploaded, id FROM {ns}.comments WHERE id IS NOT NULL AND sha256 IS NOT NULL AND group IS NOT NULL AND uploaded IS NOT NULL PRIMARY KEY (id, sha256, group, uploaded);
CREATE TABLE IF NOT EXISTS {ns}.commitishes (kind TEXT,
            group TEXT,
            repo TEXT,
            key TEXT,
            timestamp TIMESTAMP,
            data TEXT,
            repo_data TEXT,
            PRIMARY KEY ((kind, group, repo, key)));
CREATE TABLE IF NOT EXISTS {ns}.commitish_list (kind TEXT,
            group TEXT,
            year INT,
            bucket INT,
            repo TEXT,
            timestamp TIMESTAMP,
            key TEXT,
            repo_data TEXT,
            PRIMARY KEY ((kind, group, year, bucket, repo), timestamp, key)) WITH CLUSTERING ORDER BY (timestamp DESC, key DESC);
CREATE MATERIALIZED VIEW IF NOT EXISTS {ns}.committed_repo_data AS SELECT group, repo, repo_data, key FROM {ns}.commitish_list WHERE kind IS NOT NULL AND group IS NOT NULL AND repo IS NOT NULL AND repo_data IS NOT NULL AND key IS NOT NULL AND year IS NOT NULL AND bucket IS NOT NULL AND timestamp IS NOT NULL PRIMARY KEY ((repo_data, year, bucket), kind, group, repo, key, timestamp);
CREATE TABLE IF NOT EXISTS {ns}.logs (reaction UUID,
        stage TEXT,
        bucket INT,
        position BIGINT,
        line TEXT,
        PRIMARY KEY ((reaction, stage, bucket), position))
        WITH default_time_to_live = {ttl};
CREATE TABLE IF NOT EXISTS {ns}.network_policies (group TEXT, name TEXT, id UUID, k8s_name TEXT, created TIMESTAMP, ingress TEXT, egress TEXT, forced_policy BOOLEAN, default_policy BOOLEAN, PRIMARY KEY ((group), name));
CREATE MATERIALIZED VIEW IF NOT EXISTS {ns}.network_policies_by_name AS SELECT name, group, id, k8s_name, created, ingress, egress, forced_policy, default_policy FROM {ns}.network_policies WHERE name IS NOT NULL AND group IS NOT NULL PRIMARY KEY (name, group);
CREATE MATERIALIZED VIEW IF NOT EXISTS {ns}.network_policies_default AS SELECT group, name, id, default_policy FROM {ns}.network_policies WHERE group IS NOT NULL AND name IS NOT NULL AND default_policy = true PRIMARY KEY (group, name, default_policy);
CREATE TABLE IF NOT EXISTS {ns}.nodes (cluster TEXT,
            node TEXT,
            health TEXT,
            resources TEXT,
            heart_beat TIMESTAMP,
            PRIMARY KEY ((cluster), node));
CREATE TABLE IF NOT EXISTS {ns}.notifications (kind TEXT, key TEXT, created TIMESTAMP, id UUID, msg TEXT, level TEXT, ban_id UUID, PRIMARY KEY ((kind, key), created, id))
            WITH default_time_to_live = {ttl};
CREATE TABLE IF NOT EXISTS {ns}.repo_data (hash TEXT,
            repo TEXT,
            PRIMARY KEY ((repo), hash));
CREATE TABLE IF NOT EXISTS {ns}.repos_list (group TEXT,
            year INT,
            bucket INT,
            uploaded TIMESTAMP,
            id UUID,
            url TEXT,
            provider TEXT,
            user TEXT,
            name TEXT,
            creator TEXT,
            scheme TEXT,
            default_checkout TEXT,
            earliest TIMESTAMP,
            PRIMARY KEY ((group, year, bucket), uploaded, id)) WITH CLUSTERING ORDER BY (uploaded DESC, id DESC);
CREATE MATERIALIZED VIEW IF NOT EXISTS {ns}.repos AS SELECT group, year, bucket, uploaded, id, url, provider, user, name, creator, scheme, default_checkout, earliest FROM {ns}.repos_list WHERE group IS NOT NULL AND year IS NOT NULL AND bucket IS NOT NULL AND uploaded IS NOT NULL AND ID IS NOT NULL AND url IS NOT NULL PRIMARY KEY (url, group, uploaded, year, bucket, id)
            WITH CLUSTERING ORDER BY (uploaded DESC, year DESC, bucket DESC, id DESC);
CREATE TABLE IF NOT EXISTS {ns}.results_stream (kind TEXT,
            group TEXT,
            year INT,
            bucket INT,
            uploaded TIMESTAMP,
            id UUID,
            key TEXT,
            tool TEXT,
            tool_version TEXT,
            display_type TEXT,
            cmd TEXT,
            PRIMARY KEY ((kind, group, year, bucket), uploaded, id, key)) WITH CLUSTERING ORDER BY (uploaded DESC);
CREATE TABLE IF NOT EXISTS {ns}.results (id UUID,
            uploaded TIMESTAMP,
            tool TEXT,
            tool_version TEXT,
            cmd TEXT,
            result TEXT,
            files Set<TEXT>,
            display_type TEXT,
            children MAP<TEXT, UUID>,
            entities MAP<TEXT, BIGINT>,
            PRIMARY KEY (id, uploaded)) WITH CLUSTERING ORDER BY (uploaded DESC);
CREATE MATERIALIZED VIEW IF NOT EXISTS {ns}.results_auth AS SELECT kind, group, year, bucket, uploaded, id, key, tool, tool_version, display_type, cmd FROM {ns}.results_stream WHERE kind IS NOT NULL AND group IS NOT NULL AND year IS NOT NULL AND bucket IS NOT NULL AND uploaded IS NOT NULL AND id IS NOT NULL AND key IS NOT NULL AND tool IS NOT NULL PRIMARY KEY (key, kind, group, tool, id, year, bucket, uploaded);
CREATE MATERIALIZED VIEW IF NOT EXISTS {ns}.results_ids AS SELECT kind, group, year, bucket, uploaded, id, key, tool FROM {ns}.results_stream WHERE kind IS NOT NULL AND group IS NOT NULL AND year IS NOT NULL AND bucket IS NOT NULL AND uploaded IS NOT NULL AND id IS NOT NULL AND key IS NOT NULL PRIMARY KEY (key, id, kind, group, year, bucket, uploaded);
CREATE TABLE IF NOT EXISTS {ns}.s3_ids (type TEXT,
            id UUID,
            sha256 TEXT,
            PRIMARY KEY ((type, id), sha256));
CREATE MATERIALIZED VIEW IF NOT EXISTS {ns}.s3_sha256s AS SELECT type, id, sha256 FROM {ns}.s3_ids WHERE type IS NOT NULL AND id IS NOT NULL AND sha256 IS NOT NULL PRIMARY KEY (sha256, type, id);
CREATE TABLE IF NOT EXISTS {ns}.samples_list (group TEXT, year INT, bucket INT, uploaded TIMESTAMP, id UUID, sha256 TEXT, sha1 TEXT, md5 TEXT, name TEXT, description TEXT, submitter TEXT, origin TEXT, PRIMARY KEY ((group, year, bucket), uploaded, id)) WITH CLUSTERING ORDER BY (uploaded DESC, id DESC);
CREATE MATERIALIZED VIEW IF NOT EXISTS {ns}.samples AS SELECT group, year, bucket, uploaded, id, sha256, sha1, md5, name, description, submitter, origin FROM {ns}.samples_list WHERE group IS NOT NULL AND year IS NOT NULL AND bucket IS NOT NULL AND uploaded IS NOT NULL AND id IS NOT NULL AND sha256 IS NOT NULL PRIMARY KEY (sha256, group, uploaded, id, year, bucket)
            WITH CLUSTERING ORDER BY (uploaded DESC, id DESC);
CREATE TABLE IF NOT EXISTS {ns}.tags (type TEXT,
            group TEXT,
            item TEXT,
            year INT,
            bucket INT,
            key TEXT,
            value TEXT,
            uploaded TIMESTAMP,
            key_lower TEXT,
            value_lower TEXT,
            PRIMARY KEY ((type, group, year, bucket, key, value), uploaded, item, key_lower, value_lower)) WITH CLUSTERING ORDER BY (uploaded DESC);
CREATE MATERIALIZED VIEW IF NOT EXISTS {ns}.tags_by_item AS SELECT type, group, item, year, bucket, key, value, key_lower, value_lower, uploaded FROM {ns}.tags WHERE type IS NOT NULL AND group IS NOT NULL AND item IS NOT NULL AND year IS NOT NULL
            AND bucket IS NOT NULL AND key IS NOT NULL AND value IS NOT NULL AND uploaded IS NOT NULL AND key_lower IS NOT NULL AND value_lower IS NOT NULL PRIMARY KEY ((type, item), group, year, bucket, uploaded, key, value, key_lower, value_lower);
CREATE MATERIALIZED VIEW IF NOT EXISTS {ns}.tags_case_insensitive AS SELECT type, group, item, year, bucket, key_lower, value_lower, uploaded, key, value FROM {ns}.tags WHERE type IS NOT NULL AND group IS NOT NULL AND item IS NOT NULL AND year IS NOT NULL
            AND bucket IS NOT NULL AND key_lower IS NOT NULL AND value_lower IS NOT NULL AND uploaded IS NOT NULL AND key IS NOT NULL AND value IS NOT NULL PRIMARY KEY ((type, group, year, bucket, key_lower, value_lower), uploaded, item, key, value);
