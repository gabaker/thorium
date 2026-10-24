//! An in-process TCP proxy that forwards to one upstream and injects faults by rule
//!
//! It sits in front of Scylla (thoradm's session), Redis (the API's pool), or the API (thoradm's
//! client). With no rules it forwards bytes unchanged; each rule fires once a chosen number of
//! bytes has passed in one direction (or on chosen connections) and resets, drops, stalls, or
//! refuses.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;

/// When a rule fires and what it does
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyRule {
    /// Fire on connections numbered from this one (0-based); `None` fires on every connection
    pub from_connection: Option<u64>,
    /// Fire once this many bytes have passed in the chosen direction
    pub after_bytes: u64,
    /// Count bytes sent upstream (`true`) or back to the client (`false`)
    pub upstream: bool,
    /// What happens when the rule fires
    pub action: ProxyAction,
    /// Fire at most this many times; `None` keeps the fault on until the rule is removed
    pub times: Option<u32>,
}

/// A fault the proxy injects
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyAction {
    /// Close both sides with a TCP reset
    Reset,
    /// Close both sides cleanly
    Drop,
    /// Stop forwarding for this long, then continue
    Stall(Duration),
    /// Refuse new connections
    Refuse,
}

/// One active rule and how many times it has fired
#[derive(Debug)]
struct ActiveRule {
    /// The rule as configured
    rule: ProxyRule,
    /// How many times it has fired since it was set
    fired: AtomicU64,
}

impl ActiveRule {
    /// Returns whether this rule may still fire on a connection
    ///
    /// # Arguments
    ///
    /// * `connection` - The connection's number (0-based)
    fn applies_to(&self, connection: u64) -> bool {
        // the rule only covers connections from its starting number on
        let in_range = self
            .rule
            .from_connection
            .is_none_or(|from| connection >= from);
        // a rule with a limit stops once it has fired that many times
        let has_budget = self
            .rule
            .times
            .is_none_or(|times| self.fired.load(Ordering::SeqCst) < u64::from(times));
        in_range && has_budget
    }

    /// Claims one firing of this rule, returning false when its budget ran out meanwhile
    fn claim(&self) -> bool {
        // count the firing first so concurrent connections can't overspend the budget
        let previous = self.fired.fetch_add(1, Ordering::SeqCst);
        self.rule
            .times
            .is_none_or(|times| previous < u64::from(times))
    }
}

/// The rules shared between the proxy handle and its connections
type Rules = Arc<RwLock<Vec<Arc<ActiveRule>>>>;

/// A byte pattern whose next appearance upstream resets its connection, shared with the
/// connections; taken once it fires
type Trigger = Arc<std::sync::Mutex<Option<Vec<u8>>>>;

/// An in-process TCP proxy that forwards to one upstream and injects faults by rule
#[derive(Debug)]
pub struct FaultProxy {
    /// The local address clients connect to
    pub listen: SocketAddr,
    /// The address bytes are forwarded to (Scylla, Redis, or the API)
    pub upstream: SocketAddr,
    /// The active rules, checked in order for each connection; replaceable while running
    rules: Rules,
    /// How many connections have been accepted
    connections: Arc<AtomicU64>,
    /// The pattern that resets the connection sending it next, if one is set
    trigger: Trigger,
}

/// How a forwarded connection ended
enum Ending {
    /// One side closed its half cleanly or failed
    Closed,
    /// A rule asked for a reset
    Reset,
    /// A rule asked for a clean close
    Drop,
}

impl FaultProxy {
    /// Starts a proxy on an ephemeral local port
    ///
    /// # Arguments
    ///
    /// * `upstream` - The service to forward to
    pub async fn start(upstream: SocketAddr) -> std::io::Result<Self> {
        // bind an ephemeral port and spawn the accept loop
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let proxy = FaultProxy {
            listen: listener.local_addr()?,
            upstream,
            rules: Arc::default(),
            connections: Arc::default(),
            trigger: Arc::default(),
        };
        tokio::spawn(accept_loop(
            listener,
            upstream,
            proxy.rules.clone(),
            proxy.connections.clone(),
            proxy.trigger.clone(),
        ));
        Ok(proxy)
    }

    /// Replaces the active rules; an empty list makes the proxy forward unchanged
    ///
    /// # Arguments
    ///
    /// * `rules` - The rules to apply from now on
    pub async fn set_rules(&self, rules: Vec<ProxyRule>) {
        // wrap each rule with a fresh firing count
        let active = rules
            .into_iter()
            .map(|rule| {
                Arc::new(ActiveRule {
                    rule,
                    fired: AtomicU64::new(0),
                })
            })
            .collect();
        // swap the rules under the lock; connections read them per chunk
        *self.rules.write().await = active;
    }

    /// Resets the next connection that sends a byte pattern upstream, once, without forwarding
    /// the bytes that hold it; `None` clears a pattern that hasn't fired
    ///
    /// # Arguments
    ///
    /// * `pattern` - The bytes to watch for
    pub fn reset_on(&self, pattern: Option<Vec<u8>>) {
        // replace the shared pattern; connections check it per chunk
        *self
            .trigger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = pattern;
    }

    /// Returns whether a pattern set with `reset_on` is still waiting to fire
    #[must_use]
    pub fn waiting(&self) -> bool {
        // a fired pattern is taken
        self.trigger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// Returns how many times the current rules have fired in total, counted since `set_rules`
    pub async fn firings(&self) -> u64 {
        // add up each rule's firing count
        self.rules
            .read()
            .await
            .iter()
            .map(|rule| rule.fired.load(Ordering::SeqCst))
            .sum()
    }

    /// Returns how many connections the proxy has accepted
    #[must_use]
    pub fn connections(&self) -> u64 {
        // read the shared counter
        self.connections.load(Ordering::SeqCst)
    }
}

/// Accepts connections and forwards each on its own task
///
/// # Arguments
///
/// * `listener` - The bound listener
/// * `upstream` - The service to forward to
/// * `rules` - The shared rules
/// * `connections` - The shared connection counter
/// * `trigger` - The shared pattern that resets a connection
async fn accept_loop(
    listener: TcpListener,
    upstream: SocketAddr,
    rules: Rules,
    connections: Arc<AtomicU64>,
    trigger: Trigger,
) {
    // accept until the runtime shuts down
    while let Ok((client, _)) = listener.accept().await {
        // number this connection
        let number = connections.fetch_add(1, Ordering::SeqCst);
        // refuse it when a refuse rule covers it
        if refuses(&rules, number).await {
            reset(&client);
            drop(client);
            continue;
        }
        // forward it on its own task
        tokio::spawn(forward(
            client,
            upstream,
            rules.clone(),
            trigger.clone(),
            number,
        ));
    }
}

/// Returns whether a refuse rule covers a new connection, claiming one firing if so
///
/// # Arguments
///
/// * `rules` - The shared rules
/// * `number` - The new connection's number
async fn refuses(rules: &Rules, number: u64) -> bool {
    // check the refuse rules in order
    for rule in rules.read().await.iter() {
        if rule.rule.action == ProxyAction::Refuse && rule.applies_to(number) && rule.claim() {
            return true;
        }
    }
    false
}

/// Makes the next close of a socket send a TCP reset instead of a clean FIN
///
/// # Arguments
///
/// * `stream` - The socket to reset when it closes
fn reset(stream: &TcpStream) {
    // a zero linger time turns close into an abortive reset
    let _ = socket2::SockRef::from(stream).set_linger(Some(Duration::ZERO));
}

/// Finds the first rule that fires within a chunk, returning how many of its bytes pass first
///
/// A rule fires at most once per connection; its `times` limits firings across connections.
///
/// # Arguments
///
/// * `rules` - The shared rules
/// * `number` - The connection's number
/// * `upstream` - Whether the chunk is going upstream
/// * `passed` - How many bytes already passed in this direction
/// * `len` - The chunk's length
/// * `fired_here` - The rules that already fired on this connection, by address
async fn firing_rule(
    rules: &Rules,
    number: u64,
    upstream: bool,
    passed: u64,
    len: u64,
    fired_here: &mut Vec<usize>,
) -> Option<(usize, ProxyAction)> {
    // check the byte rules for this direction in order
    for rule in rules.read().await.iter() {
        // identify the rule by its address, which stays fixed while it is set
        let id = Arc::as_ptr(rule) as usize;
        // skip refuse rules, rules for the other direction or other connections, and spent rules
        if rule.rule.action == ProxyAction::Refuse
            || rule.rule.upstream != upstream
            || !rule.applies_to(number)
            || fired_here.contains(&id)
        {
            continue;
        }
        // a rule fires once its byte count falls inside this chunk or was already passed
        if rule.rule.after_bytes <= passed + len && rule.claim() {
            fired_here.push(id);
            let before =
                usize::try_from(rule.rule.after_bytes.saturating_sub(passed)).unwrap_or(usize::MAX);
            return Some((before, rule.rule.action));
        }
    }
    None
}

/// Forwards one connection in both directions, applying the rules
///
/// # Arguments
///
/// * `client` - The accepted client connection
/// * `upstream` - The service to forward to
/// * `rules` - The shared rules
/// * `trigger` - The shared pattern that resets a connection
/// * `number` - The connection's number
async fn forward(
    mut client: TcpStream,
    upstream: SocketAddr,
    rules: Rules,
    trigger: Trigger,
    number: u64,
) {
    // connect to the upstream, closing the client if it is down
    let Ok(mut server) = TcpStream::connect(upstream).await else {
        reset(&client);
        return;
    };
    // pump both directions until a side closes or a rule ends the connection
    let ending = pump(&mut client, &mut server, &rules, &trigger, number).await;
    // close both sides the way the ending asks for
    match ending {
        Ending::Reset => {
            reset(&client);
            reset(&server);
        }
        Ending::Drop | Ending::Closed => {
            let _ = client.shutdown().await;
            let _ = server.shutdown().await;
        }
    }
}

/// Copies bytes between a client and the upstream until one closes or a rule fires
///
/// # Arguments
///
/// * `client` - The client connection
/// * `server` - The upstream connection
/// * `rules` - The shared rules
/// * `trigger` - The shared pattern that resets a connection
/// * `number` - The connection's number
async fn pump(
    client: &mut TcpStream,
    server: &mut TcpStream,
    rules: &Rules,
    trigger: &Trigger,
    number: u64,
) -> Ending {
    // split both sockets so either direction can be read while the other is written
    let (mut client_read, mut client_write) = client.split();
    let (mut server_read, mut server_write) = server.split();
    let mut up = vec![0_u8; 65_536];
    let mut down = vec![0_u8; 65_536];
    let mut up_passed: u64 = 0;
    let mut down_passed: u64 = 0;
    let mut fired_here = Vec::new();
    let mut tail: Vec<u8> = Vec::new();
    let (mut client_open, mut server_open) = (true, true);
    loop {
        // wait for bytes from whichever sides are still open
        let (upstream, len) = tokio::select! {
            read = client_read.read(&mut up), if client_open => (true, read),
            read = server_read.read(&mut down), if server_open => (false, read),
            else => return Ending::Closed,
        };
        // a failed side ends the connection
        let Ok(len) = len else {
            return Ending::Closed;
        };
        // a side that closed cleanly closes its half toward the other side
        if len == 0 {
            if upstream {
                client_open = false;
                let _ = server_write.shutdown().await;
            } else {
                server_open = false;
                let _ = client_write.shutdown().await;
            }
            continue;
        }
        // a chunk going upstream that completes the trigger pattern resets the connection
        if upstream && triggered(trigger, &mut tail, &up[..len]) {
            return Ending::Reset;
        }
        // pick this direction's buffer, writer, and byte count
        let (buf, writer, passed) = if upstream {
            (&up[..len], &mut server_write, &mut up_passed)
        } else {
            (&down[..len], &mut client_write, &mut down_passed)
        };
        // find a rule that fires within this chunk
        let fired = firing_rule(
            rules,
            number,
            upstream,
            *passed,
            len as u64,
            &mut fired_here,
        )
        .await;
        // forward the bytes before the firing point, or the whole chunk when nothing fires
        let before = fired.map_or(len, |(before, _)| before.min(len));
        if writer.write_all(&buf[..before]).await.is_err() {
            return Ending::Closed;
        }
        let _ = writer.flush().await;
        *passed += before as u64;
        // apply the rule's action
        match fired {
            None | Some((_, ProxyAction::Refuse)) => (),
            Some((_, ProxyAction::Reset)) => return Ending::Reset,
            Some((_, ProxyAction::Drop)) => return Ending::Drop,
            Some((_, ProxyAction::Stall(delay))) => {
                // hold the connection, then forward the rest of the chunk
                tokio::time::sleep(delay).await;
                if writer.write_all(&buf[before..]).await.is_err() {
                    return Ending::Closed;
                }
                *passed += (len - before) as u64;
            }
        }
    }
}

/// Returns whether a chunk going upstream completes the trigger pattern, taking the pattern if
/// so; the end of each chunk is kept so a pattern split across two chunks is still found
///
/// # Arguments
///
/// * `trigger` - The shared pattern
/// * `tail` - The end of the connection's previous upstream chunk
/// * `chunk` - The chunk going upstream
fn triggered(trigger: &Trigger, tail: &mut Vec<u8>, chunk: &[u8]) -> bool {
    // nothing to watch for unless a pattern is set
    let mut pattern = trigger
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(wanted) = pattern.as_ref() else {
        tail.clear();
        return false;
    };
    // look for the pattern across the previous chunk's end and this chunk
    let mut window = std::mem::take(tail);
    window.extend_from_slice(chunk);
    if window
        .windows(wanted.len())
        .any(|bytes| bytes == wanted.as_slice())
    {
        *pattern = None;
        return true;
    }
    // keep just enough of the end to complete a pattern split across chunks
    let keep = wanted.len().saturating_sub(1).min(window.len());
    *tail = window.split_off(window.len() - keep);
    false
}

/// Maps every address a cluster's nodes can advertise to a proxy in front of it
///
/// The driver connects to each node it learns of from `system.local` and `system.peers`, on the
/// node's own port or the proxy's, so every advertised address is mapped on both ports.
///
/// # Arguments
///
/// * `node` - The address the test reaches the cluster at
/// * `proxy` - The proxy's listening address
/// * `advertised` - The `rpc_address` of every node in the cluster
#[must_use]
pub fn translations(
    node: SocketAddr,
    proxy: SocketAddr,
    advertised: &[std::net::IpAddr],
) -> std::collections::HashMap<SocketAddr, SocketAddr> {
    // the node's own address and the proxy itself
    let mut map = std::collections::HashMap::from([(node, proxy), (proxy, proxy)]);
    // every advertised address, on either port
    for rpc in advertised {
        map.insert(SocketAddr::new(*rpc, proxy.port()), proxy);
        map.insert(SocketAddr::new(*rpc, node.port()), proxy);
    }
    map
}

/// Builds the translator that sends every address a Scylla cluster can advertise to a proxy in
/// front of it
///
/// The driver pairs the `rpc_address` a node reports in `system.local`, and each peer in
/// `system.peers`, with the port it reached the node on. With Scylla on host networking that address is the proxy's own; with Scylla on a
/// container network it is the container's address, which the host can reach on the node's port
/// but not on the proxy's. Every pairing is mapped, so no connection bypasses the proxy.
///
/// # Arguments
///
/// * `scylla` - A session connected to the node directly
/// * `node` - The node's address
/// * `proxy` - The proxy's listening address
///
/// # Panics
///
/// Panics if the node's `rpc_address` or its peers can't be read.
pub async fn scylla_translator(
    scylla: &scylla::client::session::Session,
    node: SocketAddr,
    proxy: SocketAddr,
) -> std::collections::HashMap<SocketAddr, SocketAddr> {
    // read the address the node advertises, and those of its peers
    let (rpc,): (std::net::IpAddr,) = scylla
        .query_unpaged("SELECT rpc_address FROM system.local", &[])
        .await
        .expect("failed to read the node's rpc_address")
        .into_rows_result()
        .expect("system.local returned no rows")
        .single_row()
        .expect("system.local has one row");
    let peers = scylla
        .query_unpaged("SELECT peer, rpc_address FROM system.peers", &[])
        .await
        .expect("failed to read the node's peers")
        .into_rows_result()
        .expect("system.peers returned no rows result");
    let mut advertised = vec![rpc];
    for peer in peers
        .rows::<(std::net::IpAddr, Option<std::net::IpAddr>)>()
        .expect("system.peers has the expected columns")
    {
        // a peer is reached at its rpc address, or at its peer address when it has none
        let (address, rpc_address) = peer.expect("a peer row decodes");
        advertised.push(rpc_address.unwrap_or(address));
    }
    translations(node, proxy, &advertised)
}
