use bb8_redis::redis::cmd;
use chrono::prelude::*;
use clap::ValueEnum;
use futures::stream::{self, StreamExt};
use std::collections::HashMap;
use tracing::{Level, event, instrument};
use uuid::Uuid;

use super::keys::{self, StreamKeys, SystemKeys, UserKeys};
use super::{SimpleScyllaCursor, helpers};
use crate::models::schema::queries::NodesInsert;
use crate::models::schema::{CursorLimit, DEFAULT_LIMIT_CAP};
use crate::models::system::{
    BARE_METAL_CACHE_KEY, DEFAULT_IFF, EXTERNAL_CACHE_KEY, K8S_CACHE_KEY, KVM_CACHE_KEY,
    WINDOWS_CACHE_KEY, WorkerStatus,
};
use crate::models::{
    ActiveJob, ApiCursor, GroupStats, ImageScaler, ModelsConf, Node, NodeGetParams, NodeHealth,
    NodeListLine, NodeListParams, NodeRegistration, NodeRow, NodeUpdate, ScalerStats, SystemInfo,
    SystemSettings, SystemStats, User, UserRole, Worker, WorkerDeleteMap, WorkerRegistrationList,
    WorkerUpdate,
};
use crate::utils::{ApiError, Shared};
use crate::{
    conn, deserialize, deserialize_ext, exec_query, internal_err, log_scylla_err, not_found, query,
    serialize, unauthorized,
};

/// Check if Thorium is healthy
///
/// Currently this just checks redis but we should add scylla too.
///
/// # Arguments
///
/// * `shared` - Shared Thorium objects
pub async fn health(shared: &Shared) -> Result<bool, ApiError> {
    // try to ping redis
    let status: String = redis::cmd("PING").query_async(conn!(shared)).await?;
    // if our status is pong then return true
    if status == "PONG" {
        return Ok(true);
    }
    // default to an unhealhy status
    Ok(false)
}

/// Get this Thorium instances IFF string
///
/// # Arguments
///
/// * `shared` - Shared Thorium objects
pub async fn iff(shared: &Shared) -> Result<String, ApiError> {
    // build system keys
    let keys = SystemKeys::new(shared);
    let iff = query!(cmd("hget").arg(&keys.data).arg("iff"), shared).await?;
    Ok(iff)
}

/// Initalize [`SystemInfo`], [`SystemSettings`], and the IFF string in Redis with default values
///
/// # Arguments
///
/// * `shared` - Shared Thorium objects
#[rustfmt::skip]
pub async fn init(shared: &Shared) -> Result<(), ApiError> {
    // build system keys
    let keys = SystemKeys::new(shared);
    let default_info = SystemInfo::default();
    let default_settings = SystemSettings::default();
    let _: () = redis::pipe()
        .atomic()
        .cmd("hsetnx").arg(&keys.data).arg(K8S_CACHE_KEY).arg(default_info.k8s_cache)
        .cmd("hsetnx").arg(&keys.data).arg(BARE_METAL_CACHE_KEY).arg(default_info.bare_metal_cache)
        .cmd("hsetnx").arg(&keys.data).arg(WINDOWS_CACHE_KEY).arg(default_info.windows_cache)
        .cmd("hsetnx").arg(&keys.data).arg(KVM_CACHE_KEY).arg(default_info.kvm_cache)
        .cmd("hsetnx").arg(&keys.data).arg(EXTERNAL_CACHE_KEY).arg(default_info.external_cache)
        .cmd("hsetnx").arg(&keys.settings).arg("reserved_cpu").arg(default_settings.reserved_cpu)
        .cmd("hsetnx").arg(&keys.settings).arg("reserved_memory").arg(default_settings.reserved_memory)
        .cmd("hsetnx").arg(&keys.settings).arg("reserved_storage").arg(default_settings.reserved_storage)
        .cmd("hsetnx").arg(&keys.settings).arg("fairshare_cpu").arg(default_settings.fairshare_cpu)
        .cmd("hsetnx").arg(&keys.settings).arg("fairshare_memory").arg(default_settings.fairshare_memory)
        .cmd("hsetnx").arg(&keys.settings).arg("fairshare_storage").arg(default_settings.fairshare_storage)
        .cmd("hsetnx").arg(&keys.settings).arg("host_path_whitelist").arg(serialize!(&default_settings.host_path_whitelist))
        .cmd("hsetnx").arg(&keys.settings).arg("allow_unrestricted_host_paths").arg(serialize!(&default_settings.allow_unrestricted_host_paths))
        .cmd("hsetnx").arg(&keys.settings).arg("models").arg(serialize!(&default_settings.models))
        .cmd("hsetnx").arg(&keys.data).arg("iff").arg(DEFAULT_IFF)
        .query_async(conn!(shared))
        .await?;
    Ok(())
}

/// Cast a [`HashMap`] into a [`SystemInfo`] object
///
/// # Arguments
///
/// * `raw` - The hashmap to cast
/// * `namespace` - The namespace this API serves
fn cast(mut raw: HashMap<String, String>, namespace: &str) -> Result<SystemInfo, ApiError> {
    // return 404 if hashmap is empty
    if raw.is_empty() {
        return not_found!("SystemInfo not found".to_owned());
    }

    let info = SystemInfo {
        k8s_cache: helpers::extract_bool(&mut raw, K8S_CACHE_KEY)?,
        bare_metal_cache: helpers::extract_bool(&mut raw, BARE_METAL_CACHE_KEY)?,
        windows_cache: helpers::extract_bool(&mut raw, WINDOWS_CACHE_KEY)?,
        kvm_cache: helpers::extract_bool(&mut raw, KVM_CACHE_KEY)?,
        external_cache: helpers::extract_bool(&mut raw, EXTERNAL_CACHE_KEY)?,
        namespace: Some(namespace.to_owned()),
    };
    Ok(info)
}

/// Gets [`SystemInfo`] from redis
///
/// # Arguments
///
/// * `reset` - Whether to also reset certain values when getting them
/// * `shared` - Shared Thorium objects
#[rustfmt::skip]
pub async fn get_info(reset: Option<ImageScaler>, shared: &Shared) -> Result<SystemInfo, ApiError> {
    // build system keys
    let keys = SystemKeys::new(shared);
    // if reset is set get and clear system info otherwise just get it
    if let Some(scaler) = reset {
        // reset and get system info
        let (raw, _): (HashMap<String, String>, i64) = redis::pipe()
            .atomic()
            .cmd("hgetall").arg(&keys.data)
            .cmd("hset").arg(&keys.data).arg(scaler.cache_key()).arg(false)
            .query_async(conn!(shared))
            .await?;
        // cast raw data to system info
        cast(raw, &shared.config.thorium.namespace)
    } else {
        // just get system info
        let raw = query!(cmd("hgetall").arg(&keys.data), shared).await?;
        // cast raw data to system info
        cast(raw, &shared.config.thorium.namespace)
    }
}

/// Initalize the [`SystemInfo`] in redis with default values and sets the IFF string
///
/// # Arguments
///
/// * `shared` - Shared Thorium objects
#[rustfmt::skip]
pub async fn reset_cache(shared: &Shared) -> Result<(), ApiError> {
    // build system keys
    let keys = SystemKeys::new(shared);
    // sets the scaler cache to reset
    () = redis::pipe()
        .cmd("hset").arg(&keys.data).arg(K8S_CACHE_KEY).arg(true)
        .cmd("hset").arg(&keys.data).arg(BARE_METAL_CACHE_KEY).arg(true)
        .cmd("hset").arg(&keys.data).arg(WINDOWS_CACHE_KEY).arg(true)
        .cmd("hset").arg(&keys.data).arg(KVM_CACHE_KEY).arg(true)
        .query_async(conn!(shared))
        .await?;
    Ok(())
}

/// Get the scaler streams whose sizes [`get_stats`] counts, in the order it reads them
///
/// The deadline and running streams of every scaler are counted so the system-wide totals
/// include every job.
fn stats_zcard_targets() -> Vec<(ImageScaler, &'static str)> {
    // count the deadlines and currently running jobs for each scaler type
    ImageScaler::value_variants()
        .iter()
        .flat_map(|scaler| [(*scaler, "deadlines"), (*scaler, "running")])
        .collect()
}

/// Gets [`SystemStats`] from redis
///
/// # Arguments
///
/// * `groups` - The groups to get stats for
/// * `shared` - Shared Thorium objects
#[rustfmt::skip]
pub async fn get_stats(
    groups: HashMap<String, GroupStats>,
    shared: &Shared
) -> Result<SystemStats, ApiError> {
    // get system statistics
    let mut pipe = redis::pipe();
    // get the streams to count for each scaler type
    let targets = stats_zcard_targets();
    // count the number of deadlines and running jobs for each scaler type
    for (scaler, kind) in &targets {
        pipe.cmd("zcard").arg(StreamKeys::system_scaler(*scaler, kind, shared));
    }
    // count number of users
    let mut counts: Vec<i64> = pipe.cmd("scard").arg(UserKeys::global(shared))
        .query_async(conn!(shared)).await?;
    // our user count is last
    let users = counts.pop().unwrap_or_default();
    // build the stats for each scaler
    let mut stats: HashMap<ImageScaler, ScalerStats> = HashMap::with_capacity(targets.len() / 2);
    for ((scaler, kind), count) in targets.into_iter().zip(counts) {
        // get this scalers stats
        let entry = stats.entry(scaler).or_default();
        // add this count to the right stat
        match kind {
            "deadlines" => entry.deadlines = count,
            _ => entry.running = count,
        }
    }
    // get the totals across all scalers
    let deadlines = stats.values().map(|stats| stats.deadlines).sum();
    let running = stats.values().map(|stats| stats.running).sum();
    // cast raw data to system auth keys
    Ok(SystemStats {
        deadlines,
        running,
        users,
        k8s: stats.remove(&ImageScaler::K8s).unwrap_or_default(),
        baremetal: stats.remove(&ImageScaler::BareMetal).unwrap_or_default(),
        external: stats.remove(&ImageScaler::External).unwrap_or_default(),
        windows: stats.remove(&ImageScaler::Windows).unwrap_or_default(),
        kvm: stats.remove(&ImageScaler::Kvm).unwrap_or_default(),
        groups,
    })
}

/// Resets the [`SystemSettings`] in redis
///
/// # Arguments
///
/// * `shared` - Shared Thorium objects
#[rustfmt::skip]
pub async fn reset_settings(shared: &Shared) -> Result<(), ApiError> {
    // build system keys
    let keys = SystemKeys::new(shared);
    // build default settings
    let default = SystemSettings::default();
    // reset system settings
    let _: () = redis::pipe()
        // reset reserved resources
        .cmd("hset").arg(&keys.settings).arg("reserved_cpu").arg(default.reserved_cpu)
        .cmd("hset").arg(&keys.settings).arg("reserved_memory").arg(default.reserved_memory)
        .cmd("hset").arg(&keys.settings).arg("reserved_storage").arg(default.reserved_storage)
        .cmd("hset").arg(&keys.settings).arg("fairshare_cpu").arg(default.fairshare_cpu)
        .cmd("hset").arg(&keys.settings).arg("fairshare_memory").arg(default.fairshare_memory)
        .cmd("hset").arg(&keys.settings).arg("fairshare_storage").arg(default.fairshare_storage)
        .cmd("hset").arg(&keys.settings).arg("host_path_whitelist").arg(serialize!(&default.host_path_whitelist))
        .cmd("hset").arg(&keys.settings).arg("allow_unrestricted_host_paths").arg(serialize!(&default.allow_unrestricted_host_paths))
        .cmd("hset").arg(&keys.settings).arg("models").arg(serialize!(&default.models))
        .query_async(conn!(shared))
        .await?;
    Ok(())
}

/// Cast a [`HashMap`] into a [`SystemSettings`] object
///
/// # Arguments
///
/// * `raw` - The hashmap to cast
#[rustfmt::skip]
fn cast_settings(mut raw: HashMap<String, String>) -> Result<SystemSettings, ApiError> {
    // return 404 if hashmap is empty
    if raw.is_empty() {
        return not_found!("SystemSettings not found".to_owned());
    }
    // deserialize into a SystemSettings object
    let settings = SystemSettings {
        reserved_cpu: deserialize!(&helpers::extract(&mut raw, "reserved_cpu")?),
        reserved_memory: deserialize!(&helpers::extract(&mut raw, "reserved_memory")?),
        reserved_storage: deserialize!(&helpers::extract(&mut raw, "reserved_storage")?),
        fairshare_cpu: deserialize!(&helpers::extract(&mut raw, "fairshare_cpu")?),
        fairshare_memory: deserialize!(&helpers::extract(&mut raw, "fairshare_memory")?),
        fairshare_storage: deserialize!(&helpers::extract(&mut raw, "fairshare_storage")?),
        host_path_whitelist: deserialize!(&helpers::extract(&mut raw, "host_path_whitelist")?),
        allow_unrestricted_host_paths: deserialize!(&helpers::extract(&mut raw, "allow_unrestricted_host_paths")?),
        // settings written before the model whitelist existed allow no endpoints
        models: deserialize_ext!(raw, "models", ModelsConf::default()),
    };
    Ok(settings)
}

/// Gets [`SystemSettings`] from redis
///
/// # Arguments
///
/// * `shared` - Shared Thorium objects
pub async fn get_settings(shared: &Shared) -> Result<SystemSettings, ApiError> {
    // build system keys
    let keys = SystemKeys::new(shared);
    // get system auth keys
    let raw = query!(cmd("hgetall").arg(&keys.settings), shared).await?;
    // cast to a SystemSettings object
    cast_settings(raw)
}

/// Writes the [`SystemSettings`] to redis
///
/// # Arguments
///
/// * `settings` - The settings to write
/// * `shared` - Shared Thorium objects
#[rustfmt::skip]
pub async fn update_settings(settings: &SystemSettings, shared: &Shared) -> Result<(), ApiError> {
    // build system keys
    let keys = SystemKeys::new(shared);
    // reset system settings
    () = redis::pipe().atomic()
        // update reserved settings
        .cmd("hset").arg(&keys.settings).arg("reserved_cpu").arg(settings.reserved_cpu)
        .cmd("hset").arg(&keys.settings).arg("reserved_memory").arg(settings.reserved_memory)
        .cmd("hset").arg(&keys.settings).arg("reserved_storage").arg(settings.reserved_storage)
        // update fairshare settings
        .cmd("hset").arg(&keys.settings).arg("fairshare_cpu").arg(settings.fairshare_cpu)
        .cmd("hset").arg(&keys.settings).arg("fairshare_memory").arg(settings.fairshare_memory)
        .cmd("hset").arg(&keys.settings).arg("fairshare_storage").arg(settings.fairshare_storage)
        // update host path settings
        .cmd("hset").arg(&keys.settings).arg("host_path_whitelist").arg(serialize!(&settings.host_path_whitelist))
        .cmd("hset").arg(&keys.settings).arg("allow_unrestricted_host_paths").arg(serialize!(&settings.allow_unrestricted_host_paths))
        // update the allowed model endpoints
        .cmd("hset").arg(&keys.settings).arg("models").arg(serialize!(&settings.models))
        .query_async(conn!(shared))
        .await?;
    Ok(())
}

/// Restores the [`SystemSettings`] from a backup, replacing any value already stored
///
/// # Arguments
///
/// * `settings` - The settings to restore
/// * `shared` - Shared Thorium objects
#[rustfmt::skip]
pub async fn restore_settings(settings: &SystemSettings, shared: &Shared) -> Result<(), ApiError> {
    // build system keys
    let keys = SystemKeys::new(shared);
    // build a redis pipeline that replaces the defaults init wrote with the backed-up settings
    let _: () = redis::pipe()
        .atomic()
        .cmd("hset").arg(&keys.settings).arg("reserved_cpu").arg(settings.reserved_cpu)
        .cmd("hset").arg(&keys.settings).arg("reserved_memory").arg(settings.reserved_memory)
        .cmd("hset").arg(&keys.settings).arg("reserved_storage").arg(settings.reserved_storage)
        .cmd("hset").arg(&keys.settings).arg("fairshare_cpu").arg(settings.fairshare_cpu)
        .cmd("hset").arg(&keys.settings).arg("fairshare_memory").arg(settings.fairshare_memory)
        .cmd("hset").arg(&keys.settings).arg("fairshare_storage").arg(settings.fairshare_storage)
        .cmd("hset").arg(&keys.settings).arg("host_path_whitelist").arg(serialize!(&settings.host_path_whitelist))
        .cmd("hset").arg(&keys.settings).arg("allow_unrestricted_host_paths").arg(serialize!(&settings.allow_unrestricted_host_paths))
        .cmd("hset").arg(&keys.settings).arg("models").arg(serialize!(&settings.models))
        .query_async(conn!(shared)).await?;
    Ok(())
}

/// How many passes over the namespace's keys a wipe makes at most
///
/// A `SCAN` only promises the keys that exist for its whole run, so a key written during a pass
/// can be missed by it; passes repeat until one finds nothing.
const WIPE_PASSES: usize = 5;

/// How many keys one `SCAN` call of a wipe asks for
const WIPE_BATCH: usize = 1000;

/// Escapes the characters a Redis `MATCH` pattern treats as glob syntax
///
/// # Arguments
///
/// * `text` - The literal text to match
pub(crate) fn escape_glob(text: &str) -> String {
    // put a backslash before every glob character
    let mut escaped = String::with_capacity(text.len());
    for chr in text.chars() {
        if matches!(chr, '*' | '?' | '[' | ']' | '\\') {
            escaped.push('\\');
        }
        escaped.push(chr);
    }
    escaped
}

/// Deletes every Redis key of this API's namespace, leaving other namespaces' keys alone
///
/// Keys are found with `SCAN MATCH {namespace}:*` and deleted with `UNLINK` in batches. A key
/// written by another client while the wipe runs may survive it, as it would a `FLUSHDB`.
///
/// # Arguments
///
/// * `shared` - Shared Thorium objects
pub async fn wipe(shared: &Shared) -> Result<u64, ApiError> {
    // every key of the namespace, and no key of a namespace whose name only starts with it
    let pattern = format!("{}:*", escape_glob(&shared.config.thorium.namespace));
    let mut deleted = 0;
    for _ in 0..WIPE_PASSES {
        // walk the namespace's keys, deleting each page as it comes
        let mut found = 0;
        let mut cursor: u64 = 0;
        loop {
            let (next, keys): (u64, Vec<Vec<u8>>) = query!(
                cmd("scan")
                    .arg(cursor)
                    .arg("match")
                    .arg(&pattern)
                    .arg("count")
                    .arg(WIPE_BATCH),
                shared
            )
            .await?;
            if !keys.is_empty() {
                found += keys.len() as u64;
                let () = exec_query!(cmd("unlink").arg(&keys), shared).await?;
            }
            cursor = next;
            if cursor == 0 {
                break;
            }
        }
        deleted += found;
        // a pass that finds nothing means the namespace is empty
        if found == 0 {
            return Ok(deleted);
        }
    }
    // keys still appearing are being written by another client while the restore runs
    event!(
        Level::WARN,
        msg = "Keys kept appearing in the namespace while it was wiped",
        deleted
    );
    Ok(deleted)
}

/// Deletes every key in this API's Redis database with `FLUSHDB ASYNC`, returning how many keys
/// it held
///
/// Every namespace's keys in the database are deleted, not only this API's; Redis's other
/// databases are left alone, as this is not `FLUSHALL`. The flush returns at once and Redis frees
/// the memory in the background.
///
/// # Arguments
///
/// * `shared` - Shared Thorium objects
pub async fn flush_db(shared: &Shared) -> Result<u64, ApiError> {
    // count the keys and flush them in one transaction, so the count is what the flush deleted
    let (held,): (u64,) = query!(
        redis::pipe()
            .atomic()
            .cmd("dbsize")
            .cmd("flushdb")
            .arg("async")
            .ignore(),
        shared
    )
    .await?;
    Ok(held)
}

/// Save a node to scylla
///
/// # Arguments
///
/// * `node` - This nodes registration info
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::system::register", skip_all, fields(cluster = &node.cluster, node = &node.name), err(Debug))]
pub async fn register(node: &NodeRegistration, shared: &Shared) -> Result<(), ApiError> {
    // save this nodes info in scylla, with its health and resources stored as JSON
    let row = NodesInsert {
        cluster: &node.cluster,
        node: &node.name,
        health: &NodeHealth::Registered,
        resources: &node.resources,
    };
    shared
        .scylla
        .queries
        .nodes
        .insert(&shared.scylla.session, &row)
        .await?;
    Ok(())
}

/// Gets the workers for a specific node
///
/// # Arguments
///
/// * `row` - The node row to get workers for and cast
/// * `scalers` - The different scalers to get workers for
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "db::system::get_workers_for_node",
    skip(row, shared),
    fields(cluster = &row.cluster, node = &row.node),
    err(Debug)
)]
pub async fn get_workers_for_node(
    row: NodeRow,
    scalers: &Vec<ImageScaler>,
    shared: &Shared,
) -> Result<Node, ApiError> {
    // cast this row to a node
    let mut node = Node::try_from(row)?;
    // get the workers on this node for each scaler
    for scaler in scalers {
        // build the key to this nodes worker set and scaler
        let worker_set = keys::system::worker_set(&node.cluster, &node.name, *scaler, shared);
        // get the workers on this node
        let names: Vec<String> = query!(cmd("smembers").arg(worker_set), shared).await?;
        // build a redis pipeline to get these workers info
        let mut pipe = redis::pipe();
        for name in &names {
            // build the key to this workers info
            let data_key = keys::system::worker_data(name, shared);
            // add the command to get this workers info
            pipe.cmd("hgetall").arg(data_key);
        }
        // get our workers info
        let info: Vec<HashMap<String, String>> = pipe.query_async(conn!(shared)).await?;
        // cast our worker info maps to workers
        for map in info {
            // convert this map to a worker
            let worker = Worker::try_from(map)?;
            // add this worker to a node
            node.workers.insert(worker.name.clone(), worker);
        }
    }
    Ok(node)
}

/// Build the values [`get_node_rows`] binds when it queries a chunk of nodes in one cluster
///
/// The prepared statement filters on `cluster in ?`, which expects a list, so the cluster is
/// bound as a one element list.
///
/// # Arguments
///
/// * `cluster` - The cluster to query nodes in
/// * `chunk` - The chunk of nodes to query
fn chunked_node_binds(cluster: &str, chunk: &[String]) -> (Vec<String>, Vec<String>) {
    (vec![cluster.to_owned()], chunk.to_vec())
}

/// Gets a nodes info from scylla
///
/// # Arguments
///
/// * `cluster` - The cluster this node is in
/// * `node` - The name of the node to get info on
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::system::get_node", skip(params, shared), err(Debug))]
pub async fn get_node(
    cluster: &str,
    node: &str,
    params: &NodeGetParams,
    shared: &Shared,
) -> Result<Node, ApiError> {
    // get this nodes info from scylla, trying to get the first row
    let rows = shared
        .scylla
        .queries
        .nodes
        .get(&shared.scylla.session, cluster, node)
        .await?;
    if let Some(row) = rows.into_iter().next().transpose()? {
        // get this nodes workers if any exist
        let node = get_workers_for_node(row, &params.scalers, shared).await?;
        return Ok(node);
    }
    not_found!(format!("Node {}:{} doesn't exist!", cluster, node))
}

/// Gets a nodes info from scylla
///
/// # Arguments
///
/// * `clusters` - The clusters this node is in
/// * `nodes` - The names of the nodes to get info on
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::system::get_node_rows", skip_all, err(Debug))]
#[allow(
    clippy::ptr_arg,
    reason = "public thorium API; changing the parameter types to slices is an API change"
)]
pub async fn get_node_rows(
    clusters: &Vec<String>,
    nodes: &Vec<String>,
    shared: &Shared,
) -> Result<Vec<NodeRow>, ApiError> {
    // build a list to store our queries in
    let mut queries = Vec::with_capacity(1);
    // check if this query can be done in one request or not
    if clusters.len() * nodes.len() < 100 {
        // execute this query
        let query = shared
            .scylla
            .queries
            .nodes
            .get_many_chunk(&shared.scylla.session, clusters, nodes)
            .await?;
        // add our query
        queries.push(query);
    } else {
        // get each clusters node rows one at a time since many clusters is rare
        for cluster in clusters {
            // break out nodes into chunks of one
            for chunk in nodes.chunks(100) {
                // build the values to bind for this chunk
                let (cluster_list, nodes_chunk) = chunked_node_binds(cluster, chunk);
                // execute this query
                let query = shared
                    .scylla
                    .queries
                    .nodes
                    .get_many_chunk(&shared.scylla.session, &cluster_list, &nodes_chunk)
                    .await?;
                // add our query
                queries.push(query);
            }
        }
    }
    // build a list of our node rows
    let mut node_rows = Vec::with_capacity(nodes.len());
    // cast each query into node rows
    for query in queries {
        // log any rows we fail to deserialize
        let typed_iter = query.into_iter().filter_map(|row| log_scylla_err!(row));
        // extend our vec with this queries node rows
        node_rows.extend(typed_iter);
    }
    Ok(node_rows)
}

/// Update a nodes info
///
/// # Arguments
///
/// * `node` - The node to apply this update too
/// * `update` - The update to apply to this node
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::system::update_node", skip_all, err(Debug))]
pub async fn update_node(
    node: &Node,
    update: &NodeUpdate,
    shared: &Shared,
) -> Result<(), ApiError> {
    // determine if heart beat should be updated or not
    if update.heart_beat {
        // this update should update the heart beat timestamp
        let heart_beat = Utc::now();
        // update this nodes info in scylla
        shared
            .scylla
            .queries
            .nodes
            .update_heart_beat(
                &shared.scylla.session,
                &update.health,
                &update.resources,
                Some(heart_beat),
                &node.cluster,
                &node.name,
            )
            .await?;
    } else {
        // this update should not update the heart beat timestamp
        // update this nodes info in scylla
        shared
            .scylla
            .queries
            .nodes
            .update(
                &shared.scylla.session,
                &update.health,
                &update.resources,
                &node.cluster,
                &node.name,
            )
            .await?;
    }
    Ok(())
}

/// List node names for specific clusters
///
/// A page size of 0 or above [`DEFAULT_LIMIT_CAP`] is refused.
///
/// # Arguments
///
/// * `user` - The user that is listing nodes
/// * `params` - The query params to use when listing files
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::system::list_nodes", skip(user, shared), err(Debug))]
pub async fn list_nodes(
    user: &User,
    params: NodeListParams,
    shared: &Shared,
) -> Result<SimpleScyllaCursor<NodeListLine>, ApiError> {
    // make sure our page size is one we can serve
    let page_size = CursorLimit::from_user(params.page_size, DEFAULT_LIMIT_CAP, "page_size")?;
    // if a cursor id was set then get it otherwise make a new cursor scoped to the creating user
    let mut cursor = match params.cursor {
        Some(cursor_id) => {
            SimpleScyllaCursor::get(cursor_id, &user.username, page_size, shared).await?
        }
        None => SimpleScyllaCursor::new(params.clusters, &user.username, page_size),
    };
    // get the next page of data for this cursor
    cursor.next(shared).await?;
    // save or delete this cursor based on if its exhausted or not
    cursor.save(shared).await?;
    Ok(cursor)
}

/// List node details for specific clusters
///
/// A page size of 0 or above [`DEFAULT_LIMIT_CAP`] is refused.
///
/// # Arguments
///
/// * `user` - The user that is listing node details
/// * `params` - The query params to use when listing files
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::system::list_node_details", skip(user, shared), err(Debug))]
pub async fn list_node_details(
    user: &User,
    params: NodeListParams,
    shared: &Shared,
) -> Result<ApiCursor<Node>, ApiError> {
    // make sure our page size is one we can serve
    let page_size = CursorLimit::from_user(params.page_size, DEFAULT_LIMIT_CAP, "page_size")?;
    // if a cursor id was set then get it otherwise make a new cursor scoped to the creating user
    let mut cursor: SimpleScyllaCursor<NodeRow> = match params.cursor {
        Some(cursor_id) => {
            SimpleScyllaCursor::get(cursor_id, &user.username, page_size, shared).await?
        }
        None => SimpleScyllaCursor::new(params.clusters, &user.username, page_size),
    };
    // get the next page of data for this cursor
    cursor.next(shared).await?;
    // save this cursor
    cursor.save(shared).await?;
    // create an empty user facing cursor to store this cursors data
    let mut api_cursor = ApiCursor::empty(cursor.data.len());
    // get all of our nodes workers 25 at a time
    let nodes = stream::iter(cursor.data)
        .map(|row| get_workers_for_node(row, &params.scalers, shared))
        .buffered(25)
        .collect::<Vec<Result<Node, ApiError>>>()
        .await
        .into_iter()
        .filter_map(|res| log_scylla_err!(res));
    // move our nodes to our user facing cursor
    api_cursor.data.extend(nodes);
    Ok(api_cursor)
}

/// Adds new worker to the workers tables in Scylla
///
/// # Arguments
///
/// * `scaler` - The scaler this worker is under
/// * `creates` - The workers to register
/// * `shared` - Shared Thorium objects
#[rustfmt::skip]
#[instrument(name = "db::system::register_workers", skip_all, err(Debug))]
pub async fn register_workers(
    scaler: ImageScaler,
    creates: &WorkerRegistrationList,
    shared: &Shared,
) -> Result<(), ApiError> {
    //  get the current timestamp
    let now = Utc::now();
    // this worker doesn't have a heart beat yet because it just registered
    let heart_beat: Option<DateTime<Utc>> = None;
    // get a redis pipeline
    let mut pipe = redis::pipe();
    // set this pipeline to be atomic
    pipe.atomic();
    // save these workers info in scylla
    for worker in &creates.workers {
        // get the key for this workers data
        let data = keys::system::worker_data(&worker.name, shared);
        // get the key for this workers cluster/node/scaler worker set
        let set = keys::system::worker_set(&worker.cluster, &worker.node, scaler, shared);
        pipe
            // add this workers info
            .cmd("hsetnx").arg(&data).arg("cluster").arg(&worker.cluster)
            .cmd("hsetnx").arg(&data).arg("node").arg(&worker.node)
            .cmd("hsetnx").arg(&data).arg("scaler").arg(serialize!(&scaler))
            .cmd("hsetnx").arg(&data).arg("name").arg(&worker.name)
            .cmd("hsetnx").arg(&data).arg("user").arg(&worker.user)
            .cmd("hsetnx").arg(&data).arg("group").arg(&worker.group)
            .cmd("hsetnx").arg(&data).arg("tool_instance").arg(worker.tool_instance.to_string())
            .cmd("hsetnx").arg(&data).arg("status").arg(serialize!(&WorkerStatus::Spawning))
            .cmd("hsetnx").arg(&data).arg("spawned").arg(serialize!(&now))
            .cmd("hsetnx").arg(&data).arg("heart_beat").arg(serialize!(&heart_beat))
            .cmd("hsetnx").arg(&data).arg("resources").arg(serialize!(&worker.resources))
            .cmd("hsetnx").arg(&data).arg("pool").arg(serialize!(&worker.pool))
            // add this workers to its set
            .cmd("sadd").arg(&set).arg(&worker.name);
        // a per-run worker also records the run it serves
        if let Some(run) = worker.run {
            pipe.cmd("hsetnx").arg(&data).arg("run").arg(run.to_string());
        }
    }
    // execute the redis pipeline we built
    let _: () = pipe.query_async(conn!(shared)).await?;
    Ok(())
}

/// Gets info on a specific worker
///
/// # Arguments
///
/// * `name` - The name of this worker
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::system::get_worker", skip(shared), err(Debug))]
pub async fn get_worker(name: &str, shared: &Shared) -> Result<Worker, ApiError> {
    // get the key for this workers info
    let data_key = keys::system::worker_data(name, shared);
    // get this workers data
    let raw: HashMap<String, String> = query!(cmd("hgetall").arg(data_key), shared).await?;
    // if we got any data then we should have gotten a worker
    if raw.is_empty() {
        not_found!(format!("Worker {} does not exist", name))
    } else {
        Worker::try_from(raw)
    }
}

/// Updates a workers current status
///
/// # Arguments
///
/// * `worker` - The worker to register
/// * `update` - The update to apply to this worker
/// * `shared` - Shared Thorium objects
#[rustfmt::skip]
#[instrument(name = "db::system::update_worker", skip_all, err(Debug))]
pub async fn update_worker(
    worker: &Worker,
    update: &WorkerUpdate,
    shared: &Shared,
) -> Result<(), ApiError> {
    // get the key for this workers data
    let data = keys::system::worker_data(&worker.name, shared);
    //  get the current timestamp
    let heart_beat = Utc::now();
    // get a redis pipeline
    let mut pipe = redis::pipe();
    // set this pipeline to be atomic
    pipe.atomic();
    // update this workers status
    let _: () = pipe.cmd("hset").arg(&data).arg("status").arg(serialize!(&update.status))
        .cmd("hset").arg(&data).arg("heart_beat").arg(serialize!(&heart_beat))
        .query_async(conn!(shared)).await?;
    Ok(())
}

/// Updates a workers current reaction and job
///
/// # Arguments
///
/// * `worker` - The name of the worker to set a job for
/// * `reaction` - The new reaction to set
/// * `job` - The new job to set
/// * `shared` - Shared Thorium objects
/// * `span` - The span to log traces under
#[rustfmt::skip]
#[instrument(
    name = "db::system::update_worker_job",
    skip(worker, shared),
    err(Debug)
)]
pub async fn update_worker_job(
    worker: &Worker,
    reaction: &Uuid,
    job: &Uuid,
    shared: &Shared,
) -> Result<(), ApiError> {
    // get the key for this workers data
    let data = keys::system::worker_data(&worker.name, shared);
    //  get the current timestamp
    let heart_beat = Utc::now();
    // get a redis pipeline
    let mut pipe = redis::pipe();
    // build the active job object for this worker
    let active = ActiveJob{ reaction: *reaction, job: *job};
    // set this pipeline to be atomic
    pipe.atomic();
    // update this workers status
    let _: () = pipe.cmd("hset").arg(&data).arg("status").arg(serialize!(&WorkerStatus::Running))
        .cmd("hset").arg(&data).arg("heart_beat").arg(serialize!(&heart_beat))
        .cmd("hset").arg(&data).arg("active").arg(serialize!(&active))
        .query_async(conn!(shared)).await?;
    Ok(())
}

/// Get the owners for a map of workers we want to delete
///
/// # Arguments
///
/// * `user` - The user that is trying to delete workers
/// * `deletes` - The workers we want to delete
/// * `run` - The reaction of the run the user's token is limited to, if it's a run token
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::system::can_delete_workers", skip_all, err(Debug))]
pub async fn can_delete_workers(
    user: &User,
    deletes: &WorkerDeleteMap,
    run: Option<Uuid>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // only check if we can delete this if we aren't an admin
    if !user.is_admin() {
        // get the owners for all workers we are going to delete 100 at a time
        let chunked = deletes.workers.chunks(100);
        // check the workers in each chunk
        for chunk in chunked {
            // build the redis pipe for checking this chunk
            let mut pipe = redis::pipe();
            // add each worker in this chunk to our redis pipeline
            for worker in chunk {
                // get the key for this workers data
                let data = keys::system::worker_data(worker, shared);
                // get this workers owner, group, and run
                pipe.cmd("hmget")
                    .arg(data)
                    .arg("user")
                    .arg("group")
                    .arg("run");
            }
            // get all of these workers users, groups, and runs
            let reply: redis::Value = pipe.query_async(conn!(shared)).await?;
            // make sure we own every one of these workers, they are in our groups, and a run
            // token only deletes its own run's workers
            check_worker_owners(&user.username, worker_delete_groups(user), run, &reply)?;
        }
    }
    Ok(())
}

/// Get the groups a user may delete their own workers in
///
/// An Analyst's full token lists every group in Thorium, so an Analyst deletes workers only in the
/// groups they are actually a member of. Every other user, and every scoped token, uses its groups.
///
/// # Arguments
///
/// * `user` - The user trying to delete workers
fn worker_delete_groups(user: &User) -> &[String] {
    // analysts can view every group but delete workers only in the groups they belong to
    if user.role == UserRole::Analyst {
        &user.actual_groups
    } else {
        &user.groups
    }
}

/// Check that a user owns every worker in a chunk of workers they are trying to delete and that
/// each of those workers is in one of the user's groups
///
/// The reply holds each worker's owner, group, and run, and a worker that no longer exists has a
/// nil owner, so deleting an already removed worker is a 404.
///
/// # Arguments
///
/// * `username` - The user trying to delete these workers
/// * `groups` - The groups this user (or their scoped token) is in
/// * `run` - The reaction of the run the user's token is limited to, if it's a run token
/// * `reply` - The owner, group, and run of each worker in this chunk
fn check_worker_owners(
    username: &str,
    groups: &[String],
    run: Option<Uuid>,
    reply: &redis::Value,
) -> Result<(), ApiError> {
    // get the owner, group, and run of each worker
    let workers: Vec<(Option<String>, Option<String>, Option<String>)> =
        redis::from_redis_value(reply)?;
    // any worker without an owner doesn't exist
    if workers.iter().any(|(owner, _, _)| owner.is_none()) {
        return not_found!("One or more workers were not found".to_owned());
    }
    // reject this request if any worker isn't ours or isn't in one of our groups
    let allowed = workers.iter().all(|(owner, group, _)| {
        owner.as_deref() == Some(username)
            && group.as_ref().is_some_and(|group| groups.contains(group))
    });
    if !allowed {
        return unauthorized!();
    }
    // a run token may only delete its own run's workers
    if let Some(run) = run {
        let own_run = run.to_string();
        if !workers
            .iter()
            .all(|(_, _, worker_run)| worker_run.as_deref() == Some(own_run.as_str()))
        {
            return unauthorized!();
        }
    }
    Ok(())
}

/// Deletes workers from scylla
///
/// # Arguments
///
/// * `scaler` - The scaler to delete workers form
/// * `deletes` - The workers to delete
/// * `shared` -Shared Thorium objects
/// * `span` - The span to log traces under
#[rustfmt::skip]
#[instrument(name = "db::system::can_delete_workers", skip_all, err(Debug))]
pub async fn delete_workers(
    scaler: ImageScaler,
    deletes: WorkerDeleteMap,
    shared: &Shared,
) -> Result<(), ApiError> {
    // build the pipeline for deleting workers; nothing is deleted until we have checked that
    // every worker exists so a missing worker can't leave a partially applied delete
    let mut delete_pipe = redis::pipe();
    // get these workers info 25 at a time
    let chunked = deletes.workers.chunks(25);
    // get the info for this chunk of workers
    for chunk in chunked {
        // build the redis pipe for checking this chunk
        let mut pipe = redis::pipe();
        // crawl over the workers in this chunk
        for worker in chunk {
            // get the key for this workers data
            let data = keys::system::worker_data(worker, shared);
            // get this workers cluster and node
            pipe.cmd("hget").arg(&data).arg("cluster")
                .cmd("hget").arg(&data).arg("node");
        }
        // get the cluster and node for each worker
        let maybe_info: Vec<Option<String>> = pipe.query_async(conn!(shared)).await?;
        // convert our info list to an iterator
        let mut info_iter = maybe_info.iter();
        // combine our worker and cluster/node info
        for worker in chunk {
            // get this workers cluster/node
            let (cluster, node) = match (info_iter.next(), info_iter.next()) {
                (Some(Some(cluster)), Some(Some(node))) => (cluster, node),
                (Some(None), Some(None)) => return not_found!(format!("Worker {} not found", worker)),
                _ => {
                    // we got partial info on this worker
                    // log something went wrong
                    event!(Level::ERROR, worker = worker, partial_info=true);
                    return internal_err!();
                }
            };
            // build the key to this workers cluster/node worker set
            let set_key = keys::system::worker_set(cluster, node, scaler, shared);
            // delete this worker from its cluster/node worker set and delete its data
            delete_pipe.cmd("srem").arg(set_key).arg(worker)
                .cmd("del").arg(keys::system::worker_data(worker, shared));
        }
    }
    // delete all of our workers at once
    let _: () = delete_pipe.atomic().query_async(conn!(shared)).await?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::escape_glob;
    use super::*;
    use axum::http::StatusCode;
    use clap::ValueEnum;
    use scylla::frame::response::result::{
        CollectionType, ColumnSpec, ColumnType, NativeType, TableSpec,
    };
    use scylla::serialize::row::{RowSerializationContext, SerializeRow};
    use scylla::serialize::writers::RowWriter;

    /// `get_stats` counts the deadline and running streams of every scaler (including Windows
    /// and Kvm), so the "across all scalers" totals in `SystemStats` include every job.
    #[test]
    fn stats_cover_every_scaler() {
        let targets = stats_zcard_targets();
        for scaler in ImageScaler::value_variants() {
            for kind in ["deadlines", "running"] {
                assert!(
                    targets.contains(&(*scaler, kind)),
                    "system stats must count the {scaler:?} {kind} stream"
                );
            }
        }
    }

    /// The K8s deadline and running streams are counted.
    #[test]
    fn stats_count_k8s_streams() {
        let targets = stats_zcard_targets();
        assert!(targets.contains(&(ImageScaler::K8s, "deadlines")));
        assert!(targets.contains(&(ImageScaler::K8s, "running")));
    }

    /// Serialize values bound to `WHERE cluster in ? AND node in ?` like Scylla would
    ///
    /// # Arguments
    ///
    /// * `values` - The values to bind to the node get many statement
    fn serialize_node_get_many(values: &impl SerializeRow) -> bool {
        // both `in ?` bind markers expect a list of text
        let list_text = || ColumnType::Collection {
            frozen: false,
            typ: CollectionType::List(Box::new(ColumnType::Native(NativeType::Text))),
        };
        let table = TableSpec::borrowed("thorium", "nodes");
        let specs = [
            ColumnSpec::borrowed("cluster", list_text(), table.clone()),
            ColumnSpec::borrowed("node", list_text(), table),
        ];
        // serialize our values against these bind markers
        let mut buf = Vec::new();
        values
            .serialize(
                &RowSerializationContext::from_specs(&specs),
                &mut RowWriter::new(&mut buf),
            )
            .is_ok()
    }

    /// When a node get/list spans 100 or more cluster*node combinations, `get_node_rows` queries
    /// each cluster's nodes in chunks and binds the cluster as a list, as `cluster in ?` expects,
    /// so the chunked bind values serialize against the prepared statement.
    #[test]
    fn chunked_node_binds_serialize_as_lists() {
        let cluster = "cluster0".to_owned();
        let nodes: Vec<String> = (0..100).map(|i| format!("node{i}")).collect();
        // the chunked bind values must be accepted by the prepared statement
        assert!(
            serialize_node_get_many(&chunked_node_binds(&cluster, &nodes)),
            "the chunked node query must bind the cluster as a list"
        );
    }

    /// The single request path binds both values as lists, and those serialize.
    #[test]
    fn unchunked_node_binds_serialize() {
        let clusters = vec!["cluster0".to_owned()];
        let nodes = vec!["node0".to_owned()];
        assert!(serialize_node_get_many(&(&clusters, &nodes)));
    }

    /// When a non-admin deletes a worker that was just removed, the owner lookup for it returns
    /// nil, and that missing worker is reported as a 404 rather than an internal error.
    #[test]
    fn missing_worker_owner_is_not_found() {
        // the only worker in this chunk no longer exists
        let reply = redis::Value::Array(vec![worker_reply(None, None)]);
        let error = check_worker_owners("alice", &groups(&["corn"]), None, &reply).unwrap_err();
        assert_eq!(
            error.status(),
            StatusCode::NOT_FOUND,
            "deleting a worker that does not exist must return a 404"
        );
    }

    /// Build the `HMGET user group run` reply for one shared worker
    ///
    /// # Arguments
    ///
    /// * `user` - The worker's owner if it exists
    /// * `group` - The worker's group if it exists
    fn worker_reply(user: Option<&str>, group: Option<&str>) -> redis::Value {
        // cast a field to a redis value
        let field = |value: Option<&str>| {
            value.map_or(redis::Value::Nil, |value| {
                redis::Value::BulkString(value.as_bytes().to_vec())
            })
        };
        // build the reply of every field in the order they were requested
        redis::Value::Array(vec![field(user), field(group), redis::Value::Nil])
    }

    /// Build a list of group names
    ///
    /// # Arguments
    ///
    /// * `names` - The group names
    fn groups(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// A user may delete their own workers but not someone else's.
    #[test]
    fn worker_owner_check_allows_owner_only() {
        // alice owns this worker
        let owned = redis::Value::Array(vec![worker_reply(Some("alice"), Some("corn"))]);
        assert!(check_worker_owners("alice", &groups(&["corn"]), None, &owned).is_ok());
        // bob owns this worker
        let other = redis::Value::Array(vec![worker_reply(Some("bob"), Some("corn"))]);
        assert_eq!(
            check_worker_owners("alice", &groups(&["corn"]), None, &other)
                .unwrap_err()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    /// A run token may only delete its own run's workers, never another run's or a shared worker
    #[test]
    fn worker_owner_check_requires_own_run() {
        // alice's worker of her run, of another run, and a shared worker, all in corn
        let own = Uuid::new_v4();
        let other = Uuid::new_v4();
        let run_reply = |run: Option<Uuid>| {
            let field = |value: &str| redis::Value::BulkString(value.as_bytes().to_vec());
            let run = run.map_or(redis::Value::Nil, |run| field(&run.to_string()));
            redis::Value::Array(vec![field("alice"), field("corn"), run])
        };
        // the run's own worker may be deleted with its token
        let reply = redis::Value::Array(vec![run_reply(Some(own))]);
        assert!(check_worker_owners("alice", &groups(&["corn"]), Some(own), &reply).is_ok());
        // another run's worker and a shared worker may not, alone or beside the run's own
        for reply in [
            redis::Value::Array(vec![run_reply(Some(other))]),
            redis::Value::Array(vec![run_reply(None)]),
            redis::Value::Array(vec![run_reply(Some(own)), run_reply(Some(other))]),
        ] {
            assert_eq!(
                check_worker_owners("alice", &groups(&["corn"]), Some(own), &reply)
                    .unwrap_err()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
        }
        // without a run token every worker of the caller may be deleted
        let reply = redis::Value::Array(vec![run_reply(Some(other)), run_reply(None)]);
        assert!(check_worker_owners("alice", &groups(&["corn"]), None, &reply).is_ok());
    }

    /// A user may only delete their own workers that are in one of their groups
    #[test]
    fn worker_owner_check_requires_group() {
        // alice's worker in corn and her worker in wheat
        let corn = worker_reply(Some("alice"), Some("corn"));
        let wheat = worker_reply(Some("alice"), Some("wheat"));
        // a caller limited to corn may delete the corn worker
        let reply = redis::Value::Array(vec![corn.clone()]);
        assert!(check_worker_owners("alice", &groups(&["corn"]), None, &reply).is_ok());
        // but not the wheat worker, alone or with the corn worker
        for reply in [
            redis::Value::Array(vec![wheat.clone()]),
            redis::Value::Array(vec![corn, wheat]),
        ] {
            assert_eq!(
                check_worker_owners("alice", &groups(&["corn"]), None, &reply)
                    .unwrap_err()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
        }
        // a worker without a group is rejected
        let reply = redis::Value::Array(vec![worker_reply(Some("alice"), None)]);
        assert_eq!(
            check_worker_owners("alice", &groups(&["corn"]), None, &reply)
                .unwrap_err()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    /// A namespace holding glob characters matches only itself once escaped
    #[test]
    fn glob_characters_in_a_namespace_are_escaped() {
        // plain names pass unchanged
        assert_eq!(escape_glob("testing_thorium"), "testing_thorium");
        // each glob character gets a backslash
        assert_eq!(escape_glob("th*r?u[m]\\"), "th\\*r\\?u\\[m\\]\\\\");
    }

    /// A settings hash written before the model whitelist existed loads with an empty list, and
    /// one with the list loads it
    #[test]
    fn settings_without_models_load_empty() {
        // a settings hash with every field but the model whitelist
        let mut raw = HashMap::new();
        for field in [
            "reserved_cpu",
            "reserved_memory",
            "reserved_storage",
            "fairshare_cpu",
            "fairshare_memory",
            "fairshare_storage",
        ] {
            raw.insert(field.to_owned(), "1".to_owned());
        }
        raw.insert("host_path_whitelist".to_owned(), "[]".to_owned());
        raw.insert(
            "allow_unrestricted_host_paths".to_owned(),
            "false".to_owned(),
        );
        let settings = cast_settings(raw.clone()).unwrap();
        assert_eq!(settings.models, crate::models::ModelsConf::default());
        // a hash with the list loads it
        let models = crate::models::ModelsConf {
            allowed_endpoints: std::collections::BTreeSet::from([
                crate::models::AllowedEndpoint::new(
                    crate::models::ModelProviderKind::Anthropic,
                    None,
                ),
            ]),
        };
        raw.insert("models".to_owned(), serde_json::to_string(&models).unwrap());
        assert_eq!(cast_settings(raw).unwrap().models, models);
    }
}
