//! Network communication / C2 capability detection (MITRE ATT&CK **T1071** /
//! **T1071.001** / **T1071.004** / **T1095** / **T1571** / **T1573**).
//!
//! Surfaces how a sample talks to the network — one of the first questions an analyst
//! reversing a binary asks — and, crucially, **extracts the useful information** from
//! whatever it finds so each flag is actionable rather than a bare "calls `connect`".
//! Four angles, so at least one fires across native, dynamically-linked, and
//! statically-linked (Go/Rust) binaries:
//!
//! 1. **Networking APIs** — HTTP (WinINet/WinHTTP/urlmon/libcurl), DNS resolvers, raw
//!    TCP/UDP sockets (Winsock + libc), QUIC/HTTP-3 libraries, and TLS libraries.
//!    Found via imports (each catalog fed to [`api_call_hits`] under its technique).
//! 2. **Runtime symbols** — statically-linked Go/Rust binaries have no import table,
//!    so their networking shows up only as package symbols (`net/http`, `crypto/tls`,
//!    `reqwest`, `quinn`, …). Found by scanning the function list.
//! 3. **Network IOCs in strings** — hardcoded URLs (parsed into scheme/host/port/
//!    path), host:port endpoints, `.onion` addresses, bare IPv4/IPv6 literals, and the
//!    HTTP **request fabric** the program builds its traffic out of: User-Agents,
//!    header names and templates, cookie material, URI paths and templates, methods,
//!    and media types. Each flag carries the parsed fields for pivoting/hunting, and
//!    each class is capped separately so no class starves another. Validity gates
//!    fold the junk shapes — fragment-bearing documentation URLs, dotted quads with
//!    no networking code reference or sitting in VERSIONINFO/resource data — plus
//!    any per-class overflow into one unseeded-IOC digest instead of per-lead flags.
//!    The recovered indicators are then grouped into pools by [`super::ioc_pools`].
//! 4. **Hardcoded ports** — the constant passed to `htons()`/`ntohs()` is recovered
//!    from the calling code (and, in an optimized build where the compiler folded the
//!    byte swap, from the 16-bit store into `sockaddr_in.sin_port` — accepted only
//!    inside functions already carrying raw-socket hits, with character-store
//!    immediates rejected); a non-standard port is flagged as T1571.
//!
//! All hits are [`EvidenceKind::ContextOnly`]: networking is capability/context
//! evidence, validated by reachability + clustering (a socket call's return value
//! does not gate a branch the way an anti-debug check does).

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::thorpal::detect::confidence::Confidence;
use anyhow::{Context, Result};
use ghidra_cli::models::{Instruction, StringData, Symbol};

use crate::thorpal::detect::context::{AnalysisContext, SectionBytes, fmt_addr, parse_addr};
use crate::thorpal::detect::decode::buffers::{is_data_section, referencing_function};
use crate::thorpal::detect::detections::{
    ApiSpec, ArtifactClass, AttackTechnique, Detection, DetectionHit, EvidenceKind, HitArtifact,
    api_call_hits, build_api_hit, confidence_rank, high, is_library_function, low, medium,
    normalize, parse_imm,
};
use crate::thorpal::detect::indicators::{
    HttpArtifact, UrlIoc, classify_http, is_standard_port, parse_endpoint, parse_ipv4_literal,
    parse_ipv6_literal, parse_url,
};

use super::{T1071, T1071_001, T1071_004, T1095, T1571, T1573};

/// The max distance (bytes) from a referenced data address at which an unreferenced
/// IOC string is still attributed to the function that references its buffer. Matches
/// the shared `string_artifact_hits` window so data attribution stays consistent.
const DATA_REF_WINDOW: u64 = 4096;

/// The max reference sites listed in one indicator's flag.
///
/// The addresses are for spot-checking, and the count that follows them carries the
/// "this literal is used everywhere" signal on its own, so listing every one of a
/// hundred sites only buries the parsed fields above it.
const MAX_LISTED_SITES: usize = 12;

/// The max hardcoded-URL flags surfaced from strings.
///
/// Overflow is not lost: values past the cap land in the unseeded-IOC digest, so
/// trimming the cap trades per-lead flags for one digest line each.
const MAX_URL_HITS: usize = 16;

/// The max bare `host:port` endpoint flags surfaced from strings.
const MAX_ENDPOINT_HITS: usize = 16;

/// The max bare IP-literal flags surfaced from strings.
const MAX_IP_HITS: usize = 8;

/// The max flags surfaced *per* HTTP request-fabric class.
///
/// Counted per class rather than in total. One global cap lets whichever class happens
/// to come first in the string table starve every other one, which is how a single
/// hand-built request template ends up missing from a report full of URI paths — and
/// the template is the more useful of the two.
const MAX_HTTP_CLASS_HITS: usize = 6;

/// The max functions flagged per runtime-symbol marker (a Go binary may have hundreds
/// of `net/http` functions; a handful of representatives is enough for an analyst).
const MAX_FUNCS_PER_MARKER: usize = 8;

/// The max runtime-symbol flags surfaced in total (across every marker).
const MAX_SYMBOL_HITS: usize = 40;

/// The max recovered-port flags surfaced (bounds the socket-setup scan).
const MAX_PORT_HITS: usize = 24;

/// How many instructions before a `htons()` call are searched for its argument.
const PORT_ARG_WINDOW: usize = 8;

/// The byte distance within which a defined VERSIONINFO key string marks a bare IP
/// literal as resource metadata rather than an indicator.
///
/// In a `VS_VERSION_INFO` block the key strings (`FileVersion`, `ProductVersion`)
/// sit immediately before their values, so a dotted quad this close to one is a
/// version number, not an address.
const VERSIONINFO_NEIGHBOR_WINDOW: u64 = 512;

/// The max values itemized in the unseeded-IOC digest's body.
const MAX_DIGEST_LINES: usize = 40;

/// The Network communication detection (ATT&CK T1071 family).
pub struct NetworkComms;

// ===========================================================================
// Protocol families (shared by the symbol scan and the string-IOC pass)
// ===========================================================================

/// The protocol family a networking marker or recovered IOC evidences.
///
/// Shared by the runtime-symbol scan and the string-IOC pass so the same kind of
/// traffic always maps to the same ATT&CK technique.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Proto {
    /// HTTP/HTTPS/WebSocket application traffic (T1071.001).
    Web,
    /// DNS name resolution (T1071.004).
    Dns,
    /// Raw TCP/UDP sockets, below the application layer (T1095).
    Socket,
    /// QUIC / HTTP-3, which rides UDP rather than TCP (T1071).
    Quic,
    /// A TLS/SSL-encrypted channel (T1573).
    Tls,
}

impl Proto {
    /// The ATT&CK technique this protocol family maps to.
    fn technique(self) -> AttackTechnique {
        // each family has a dedicated technique; QUIC falls back to the T1071 parent
        match self {
            Proto::Web => T1071_001,
            Proto::Dns => T1071_004,
            Proto::Socket => T1095,
            Proto::Quic => T1071,
            Proto::Tls => T1573,
        }
    }

    /// A short analyst-facing label for this protocol family.
    fn label(self) -> &'static str {
        // used in flag content/reasoning so the protocol is obvious at a glance
        match self {
            Proto::Web => "HTTP/web",
            Proto::Dns => "DNS",
            Proto::Socket => "raw TCP/UDP socket",
            Proto::Quic => "QUIC/HTTP-3",
            Proto::Tls => "TLS",
        }
    }
}

// ===========================================================================
// Networking API catalogs (grouped by ATT&CK technique)
// ===========================================================================

/// Web/HTTP APIs (T1071.001): WinINet, WinHTTP, urlmon, and libcurl.
const WEB_APIS: &[ApiSpec] = &[
    // --- WinINet (wininet.dll) ---
    ApiSpec {
        key: "InternetOpen",
        always_noise: false,
        severity: low(),
        reasoning: "Calls InternetOpen (WinINet) to initialize an HTTP/FTP session — the setup step \
                    before connecting to a remote server. Common, hence low on its own.",
    },
    ApiSpec {
        key: "InternetConnect",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls InternetConnect (WinINet) to open a connection to a named host/port — a \
                    direct network-communication capability; the host and port passed here name \
                    the endpoint this code talks to.",
    },
    ApiSpec {
        key: "InternetOpenUrl",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls InternetOpenUrl (WinINet) to fetch a URL directly — the one-call fetch a \
                    downloader or beacon would use, and equally the one an updater uses. The URL \
                    argument names the endpoint worth recovering.",
    },
    ApiSpec {
        key: "HttpOpenRequest",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls HttpOpenRequest (WinINet) to build an HTTP request (GET/POST) — the \
                    request-construction step of any HTTP exchange, hand-rolled C2 included.",
    },
    ApiSpec {
        key: "HttpSendRequest",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls HttpSendRequest (WinINet) to send an HTTP request — active HTTP \
                    communication with a remote server; whether that is a beacon, an upload, or \
                    ordinary client traffic depends on what the surrounding code sends.",
    },
    ApiSpec {
        key: "InternetReadFile",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls InternetReadFile (WinINet) to read the HTTP response body — the receive \
                    half of an HTTP exchange, where a downloader or beacon would take delivery and \
                    where any client reads its response.",
    },
    ApiSpec {
        key: "InternetWriteFile",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls InternetWriteFile (WinINet) to upload data over HTTP — an HTTP upload \
                    capability, the primitive an exfiltration or form post rides.",
    },
    // --- WinHTTP (winhttp.dll) ---
    ApiSpec {
        key: "WinHttpOpen",
        always_noise: false,
        severity: low(),
        reasoning: "Calls WinHttpOpen to initialize a WinHTTP session — the setup step before an \
                    HTTP(S) connection. Common, hence low on its own.",
    },
    ApiSpec {
        key: "WinHttpConnect",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls WinHttpConnect to open a connection to a named host/port over WinHTTP — a \
                    direct network-communication capability; the host and port passed here name \
                    the endpoint this code talks to.",
    },
    ApiSpec {
        key: "WinHttpOpenRequest",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls WinHttpOpenRequest to build an HTTP request over WinHTTP — the \
                    request-construction step of any HTTP exchange, hand-rolled C2 included.",
    },
    ApiSpec {
        key: "WinHttpSendRequest",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls WinHttpSendRequest to send an HTTP(S) request via WinHTTP — active HTTP \
                    communication with a remote server; whether that is a beacon, an upload, or \
                    ordinary client traffic depends on what the surrounding code sends.",
    },
    ApiSpec {
        key: "WinHttpReceiveResponse",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls WinHttpReceiveResponse to receive an HTTP response via WinHTTP — the \
                    receive half of a WinHTTP exchange, where a beacon or downloader would take \
                    delivery.",
    },
    ApiSpec {
        key: "WinHttpReadData",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls WinHttpReadData to read an HTTP(S) response body via WinHTTP — the \
                    receive half of an HTTP(S) exchange.",
    },
    ApiSpec {
        key: "WinHttpWriteData",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls WinHttpWriteData to upload a request body via WinHTTP — an HTTP upload \
                    capability, the primitive an exfiltration or form post rides.",
    },
    // --- urlmon (one-call downloaders) ---
    ApiSpec {
        key: "URLDownloadToFile",
        always_noise: false,
        severity: high(),
        reasoning: "Calls URLDownloadToFile (urlmon) to fetch a URL straight to disk — a one-call \
                    downloader, overwhelmingly used to stage a second-stage payload.",
    },
    ApiSpec {
        key: "URLDownloadToCacheFile",
        always_noise: false,
        severity: high(),
        reasoning: "Calls URLDownloadToCacheFile (urlmon) to fetch a URL into the WinINet cache — a \
                    one-call downloader used to stage a second-stage payload.",
    },
    // --- libcurl (cross-platform HTTP client) ---
    ApiSpec {
        key: "curl_global_init",
        always_noise: false,
        severity: low(),
        reasoning: "Calls curl_global_init — the sample links libcurl and initializes it, the setup \
                    step before any HTTP(S) transfer. Common, hence low on its own.",
    },
    ApiSpec {
        key: "curl_easy_init",
        always_noise: false,
        severity: low(),
        reasoning: "Calls curl_easy_init to create a libcurl transfer handle — the sample carries a \
                    full HTTP(S) client. Setup only, hence low on its own.",
    },
    ApiSpec {
        key: "curl_easy_setopt",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls curl_easy_setopt to configure a libcurl transfer (URL, POST body, headers, \
                    user-agent, TLS verification) — the endpoint and request shape are set here, so \
                    this call is where the transfer's target is pinned down.",
    },
    ApiSpec {
        key: "curl_easy_perform",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls curl_easy_perform to execute an HTTP(S) transfer via libcurl — active \
                    network communication, moving whatever the handle was configured to move.",
    },
];

/// DNS resolution APIs (T1071.004): Windows DNS, Winsock, and libc/libresolv.
const DNS_APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "DnsQuery",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls DnsQuery to issue a DNS request directly — resolves a hostname of the \
                    program's choosing, and (with TXT/NULL record types) is the primitive behind \
                    DNS-tunnelled C2.",
    },
    // Ghidra reports the real Windows exports `DnsQuery_A` / `DnsQuery_W`, which the
    // catalog normalizer reduces to `DnsQuery_` (a trailing A/W is stripped).
    ApiSpec {
        key: "DnsQuery_",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls DnsQuery_A/W to issue a DNS request directly — resolves a hostname of the \
                    program's choosing, and (with TXT/NULL record types) is the primitive behind \
                    DNS-tunnelled C2.",
    },
    ApiSpec {
        key: "DnsQuery_UTF8",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls DnsQuery_UTF8 to issue a DNS request directly — resolves a hostname of \
                    the program's choosing, and (with TXT/NULL record types) is the primitive \
                    behind DNS-tunnelled C2.",
    },
    ApiSpec {
        key: "DnsQueryEx",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls DnsQueryEx to issue an (asynchronous) DNS request directly — resolves a \
                    hostname without going through the higher-level connect APIs.",
    },
    ApiSpec {
        key: "getaddrinfo",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls getaddrinfo to resolve a hostname to an address — the standard first step \
                    of connecting to any named host; the artifact worth hunting is the name it is \
                    given.",
    },
    ApiSpec {
        key: "GetAddrInfo",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls GetAddrInfoA/W to resolve a hostname to an address — the standard first \
                    step of connecting to any named host; the artifact worth hunting is the name \
                    it is given.",
    },
    ApiSpec {
        key: "GetAddrInfoEx",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls GetAddrInfoEx to resolve a hostname (optionally asynchronously) — the \
                    first step of connecting to a named host.",
    },
    ApiSpec {
        key: "gethostbyname",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls gethostbyname to resolve a hostname to an address — the legacy resolver; \
                    when a family hardcodes a domain, this is the classic API that resolves it.",
    },
    ApiSpec {
        key: "WSAAsyncGetHostByName",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls WSAAsyncGetHostByName to resolve a hostname asynchronously — resolves a \
                    domain without blocking the calling thread.",
    },
    ApiSpec {
        key: "getnameinfo",
        always_noise: false,
        severity: low(),
        reasoning: "Calls getnameinfo to reverse-resolve an address to a hostname — network-aware \
                    code, but weak on its own.",
    },
    ApiSpec {
        key: "res_query",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls res_query (libresolv) to issue a raw DNS query — bypasses the system \
                    resolver cache and is the usual primitive for DNS-tunnelled C2.",
    },
    ApiSpec {
        key: "res_search",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls res_search (libresolv) to issue a raw DNS query — bypasses the system \
                    resolver cache and is the usual primitive for DNS-tunnelled C2.",
    },
];

/// Raw socket APIs (T1095): Winsock (`ws2_32`) and BSD/libc sockets.
///
/// Setup calls (`socket`, `WSAStartup`, option/IP helpers) are `low`; the calls that
/// actually move traffic (`connect`, `send`, `recv`, `sendto`, `recvfrom`) are
/// `medium`.
const SOCKET_APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "WSAStartup",
        always_noise: false,
        severity: low(),
        reasoning: "Calls WSAStartup to initialize Winsock — the mandatory setup step before any \
                    socket use. Ubiquitous in network-capable code, hence low on its own.",
    },
    ApiSpec {
        key: "socket",
        always_noise: false,
        severity: low(),
        reasoning: "Calls socket() to create a TCP/UDP socket — the sample can speak a raw \
                    transport protocol directly, without an HTTP library.",
    },
    ApiSpec {
        key: "WSASocket",
        always_noise: false,
        severity: low(),
        reasoning: "Calls WSASocket to create a socket (often with overlapped I/O) — the sample can \
                    speak a raw transport protocol directly.",
    },
    ApiSpec {
        key: "connect",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls connect() to open a TCP connection to a remote endpoint — direct \
                    non-application-layer communication; the sockaddr built nearby names the \
                    host:port this code reaches for.",
    },
    ApiSpec {
        key: "WSAConnect",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls WSAConnect to open a connection to a remote endpoint — direct \
                    non-application-layer communication; the sockaddr built nearby names the \
                    host:port this code reaches for.",
    },
    ApiSpec {
        key: "bind",
        always_noise: false,
        severity: low(),
        reasoning: "Calls bind() to bind a socket to a local address/port — with listen()/accept() \
                    this is a listener (a bind shell or reverse-connect fallback).",
    },
    ApiSpec {
        key: "listen",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls listen() to accept inbound connections — the sample runs a server, the \
                    shape of a bind shell or an implant awaiting operator connections.",
    },
    ApiSpec {
        key: "accept",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls accept() to take an inbound connection — the sample runs a server, the \
                    shape of a bind shell or an implant awaiting operator connections.",
    },
    ApiSpec {
        key: "send",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls send() to write to a connected socket — active outbound traffic, the \
                    write a beacon check-in or an exfiltration push would ride and that any \
                    protocol client makes.",
    },
    ApiSpec {
        key: "WSASend",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls WSASend to write to a connected socket — active outbound traffic, the \
                    write a beacon check-in or an exfiltration push would ride and that any \
                    protocol client makes.",
    },
    ApiSpec {
        key: "recv",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls recv() to read from a connected socket — inbound traffic, the read a \
                    command loop or any protocol client blocks on.",
    },
    ApiSpec {
        key: "WSARecv",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls WSARecv to read from a connected socket — inbound traffic, the read a \
                    command loop or any protocol client blocks on.",
    },
    ApiSpec {
        key: "sendto",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls sendto() to send a UDP datagram to an explicit destination — \
                    connectionless traffic that leaves no TCP session behind, the shape of \
                    UDP-based C2 and of ordinary datagram protocols alike.",
    },
    ApiSpec {
        key: "recvfrom",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls recvfrom() to receive a UDP datagram — the receive half of connectionless \
                    (UDP) traffic.",
    },
    ApiSpec {
        key: "closesocket",
        always_noise: false,
        severity: low(),
        reasoning: "Calls closesocket to tear down a socket — corroborates socket use, but carries \
                    no signal on its own.",
    },
    ApiSpec {
        key: "setsockopt",
        always_noise: false,
        severity: low(),
        reasoning: "Calls setsockopt to configure a socket (timeouts, keep-alive, reuse) — typical \
                    of a resilient beacon loop, but weak on its own.",
    },
    ApiSpec {
        key: "ioctlsocket",
        always_noise: false,
        severity: low(),
        reasoning: "Calls ioctlsocket to change socket I/O mode (e.g. non-blocking) — typical of a \
                    polling beacon loop, but weak on its own.",
    },
    ApiSpec {
        key: "select",
        always_noise: false,
        severity: low(),
        reasoning: "Calls select() to multiplex socket I/O — the shape of a polling C2 loop, but far \
                    too common to mean much on its own.",
    },
    ApiSpec {
        key: "poll",
        always_noise: false,
        severity: low(),
        reasoning: "Calls poll() to multiplex socket I/O — the shape of a polling C2 loop, but far \
                    too common to mean much on its own.",
    },
    ApiSpec {
        key: "inet_addr",
        always_noise: false,
        severity: low(),
        reasoning: "Calls inet_addr to convert a dotted-quad string into an address — paired with \
                    connect() this reaches an IP literal with no DNS artifact to hunt, but the \
                    same call also feeds local addresses and configuration.",
    },
    ApiSpec {
        key: "inet_pton",
        always_noise: false,
        severity: low(),
        reasoning: "Calls inet_pton to convert an IPv4/IPv6 literal into an address — the sample \
                    can address an IP literal directly rather than a resolved name.",
    },
];

/// QUIC / HTTP-3 libraries (T1071): msquic, quiche, ngtcp2, and lsquic.
///
/// QUIC rides UDP and is TLS-encrypted end to end, so this traffic is invisible to
/// HTTP-aware inspection and easily mistaken for ordinary UDP noise.
const QUIC_APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "MsQuicOpen",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls MsQuicOpen (Microsoft msquic) to start a QUIC stack — HTTP/3-over-UDP \
                    traffic that is TLS-encrypted end to end and bypasses HTTP-aware inspection.",
    },
    ApiSpec {
        key: "MsQuicOpenVersion",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls MsQuicOpenVersion (Microsoft msquic) to start a QUIC stack — \
                    HTTP/3-over-UDP traffic that bypasses HTTP-aware inspection.",
    },
    ApiSpec {
        key: "quiche_connect",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls quiche_connect to open a QUIC connection — HTTP/3-over-UDP traffic that \
                    is TLS-encrypted end to end and bypasses HTTP-aware inspection.",
    },
    ApiSpec {
        key: "quiche_conn_send",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls quiche_conn_send to write QUIC datagrams — active HTTP/3-over-UDP \
                    traffic, the send half of the channel.",
    },
    ApiSpec {
        key: "quiche_conn_recv",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls quiche_conn_recv to read QUIC datagrams — the receive half of an \
                    HTTP/3-over-UDP channel.",
    },
    ApiSpec {
        key: "ngtcp2_conn_client_new",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls ngtcp2_conn_client_new to create a QUIC client connection — \
                    HTTP/3-over-UDP traffic that bypasses HTTP-aware inspection.",
    },
    ApiSpec {
        key: "ngtcp2_conn_writev_stream",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls ngtcp2_conn_writev_stream to write a QUIC stream — active \
                    HTTP/3-over-UDP traffic, the send half of the channel.",
    },
    ApiSpec {
        key: "lsquic_engine_new",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls lsquic_engine_new to start a QUIC engine — HTTP/3-over-UDP traffic that is \
                    TLS-encrypted end to end and bypasses HTTP-aware inspection.",
    },
];

/// TLS/SSL libraries (T1573): OpenSSL, mbedTLS, and Windows SChannel.
const TLS_APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "SSL_CTX_new",
        always_noise: false,
        severity: low(),
        reasoning: "Calls SSL_CTX_new (OpenSSL) to build a TLS context — the setup step of an \
                    encrypted channel. Common in benign software, hence low on its own.",
    },
    ApiSpec {
        key: "TLS_client_method",
        always_noise: false,
        severity: low(),
        reasoning: "Calls TLS_client_method (OpenSSL) to select the client-side TLS method — the \
                    sample initiates (rather than accepts) encrypted connections.",
    },
    ApiSpec {
        key: "SSL_new",
        always_noise: false,
        severity: low(),
        reasoning: "Calls SSL_new (OpenSSL) to create a TLS session object — the setup step of an \
                    encrypted channel. Common in benign software, hence low on its own.",
    },
    ApiSpec {
        key: "SSL_connect",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls SSL_connect (OpenSSL) to perform the TLS handshake as a client — the \
                    program's channel is encrypted, so its contents are opaque on the wire.",
    },
    ApiSpec {
        key: "SSL_do_handshake",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls SSL_do_handshake (OpenSSL) to drive the TLS handshake — the program's \
                    channel is encrypted, so its contents are opaque on the wire.",
    },
    ApiSpec {
        key: "SSL_write",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls SSL_write (OpenSSL) to send data over TLS — encrypted outbound traffic \
                    that network inspection cannot read.",
    },
    ApiSpec {
        key: "SSL_read",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls SSL_read (OpenSSL) to receive data over TLS — encrypted inbound traffic \
                    that network inspection cannot read.",
    },
    ApiSpec {
        key: "mbedtls_ssl_setup",
        always_noise: false,
        severity: low(),
        reasoning: "Calls mbedtls_ssl_setup to configure an mbedTLS session — the setup step of an \
                    encrypted channel (mbedTLS is common in small, statically-linked implants).",
    },
    ApiSpec {
        key: "mbedtls_ssl_handshake",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls mbedtls_ssl_handshake to perform a TLS handshake — the program's channel \
                    is encrypted, so its contents are opaque on the wire.",
    },
    ApiSpec {
        key: "AcquireCredentialsHandle",
        always_noise: false,
        severity: low(),
        reasoning: "Calls AcquireCredentialsHandle (SChannel/SSPI) to obtain TLS credentials — the \
                    setup step of a Windows-native encrypted channel.",
    },
    ApiSpec {
        key: "InitializeSecurityContext",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls InitializeSecurityContext (SChannel/SSPI) to drive a TLS handshake using \
                    the Windows-native TLS stack — an encrypted channel built without OpenSSL.",
    },
    ApiSpec {
        key: "EncryptMessage",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls EncryptMessage (SChannel/SSPI) to encrypt an outbound record — data sent \
                    over a Windows-native TLS channel that inspection cannot read.",
    },
    ApiSpec {
        key: "DecryptMessage",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls DecryptMessage (SChannel/SSPI) to decrypt an inbound record — data \
                    received over a Windows-native TLS channel that inspection cannot read.",
    },
];

// ===========================================================================
// 1b) Winsock imported by ordinal
// ===========================================================================

/// The `ws2_32.dll` / `wsock32.dll` export ordinals for the BSD-compatible socket
/// block (1-30) plus the startup/teardown pair.
///
/// Those exports are **ordinal-only** in the DLL, so when Ghidra has no export map
/// for it the import table reads `Ordinal_23` instead of `socket` and the
/// name-keyed catalogs above never fire — which is exactly what happens on the
/// al-khaser test sample. Mapping the ordinal back to its name recovers the whole
/// socket/DNS pass (and the `htons` port recovery) on those binaries. Only the
/// unambiguous, long-stable block is listed; anything unmapped is simply ignored.
const WS2_32_ORDINALS: &[(u32, &str)] = &[
    (1, "accept"),
    (2, "bind"),
    (3, "closesocket"),
    (4, "connect"),
    (5, "getpeername"),
    (6, "getsockname"),
    (7, "getsockopt"),
    (8, "htonl"),
    (9, "htons"),
    (10, "ioctlsocket"),
    (11, "inet_addr"),
    (12, "inet_ntoa"),
    (13, "listen"),
    (14, "ntohl"),
    (15, "ntohs"),
    (16, "recv"),
    (17, "recvfrom"),
    (18, "select"),
    (19, "send"),
    (20, "sendto"),
    (21, "setsockopt"),
    (22, "shutdown"),
    (23, "socket"),
    (24, "gethostbyaddr"),
    (25, "gethostbyname"),
    (26, "getprotobyname"),
    (27, "getprotobynumber"),
    (28, "getservbyname"),
    (29, "getservbyport"),
    (30, "gethostname"),
    (115, "WSAStartup"),
    (116, "WSACleanup"),
];

/// Fetch the importing library of every external symbol, keyed by symbol address.
///
/// The typed `symbol_list` payload [`AnalysisContext`] is built from carries no
/// library name, so the ordinal pass asks the bridge's `list_imports` command —
/// which reports one per import — through the untyped escape hatch. Both commands
/// address symbols identically, so the address is a safe join key. It is kept as
/// the **raw string** because an import lives in Ghidra's `EXTERNAL` address space
/// and renders as e.g. `EXTERNAL:00000002`, which is not a plain hex number.
///
/// A bridge that cannot answer is not fatal: the map comes back empty and the
/// ordinal pass simply finds nothing, which is logged rather than swallowed.
///
/// # Arguments
///
/// * `ctx` - The analysis context
#[tracing::instrument(name = "network_comms::import_libraries", skip_all)]
async fn import_libraries(ctx: &AnalysisContext) -> HashMap<String, String> {
    // ask the bridge for the import table, which unlike `symbol_list` names libraries
    let payload = match ctx.ghidra.command("list_imports", None).await {
        Ok(payload) => payload,
        // an unavailable import table only costs us the ordinal pass, so warn and go on
        Err(err) => {
            tracing::warn!(error = ?err, "import table unavailable, skipping Winsock ordinals");
            return HashMap::new();
        }
    };
    // pair every import's address with its library, dropping any malformed entry
    payload["imports"]
        .as_array()
        .map(|imports| {
            imports
                .iter()
                .filter_map(|import| {
                    let address = import["address"].as_str()?.to_string();
                    Some((address, import["library"].as_str()?.to_ascii_lowercase()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether an importing library provides the Winsock socket API.
///
/// # Arguments
///
/// * `library` - The importing library name, in any case
fn is_winsock_library(library: &str) -> bool {
    // both the modern and the legacy Winsock DLLs export the same ordinal block
    let library = library.to_ascii_lowercase();
    library.contains("ws2_32") || library.contains("wsock32")
}

/// Map an `Ordinal_N` import name back to its Winsock export name.
///
/// # Arguments
///
/// * `name` - The imported symbol's name
fn winsock_ordinal(name: &str) -> Option<&'static str> {
    // `Ordinal_N` is Ghidra's placeholder name for an unnamed ordinal export
    let ordinal: u32 = name.strip_prefix("Ordinal_")?.parse().ok()?;
    // resolve it through the table above
    WS2_32_ORDINALS
        .iter()
        .find(|(number, _)| *number == ordinal)
        .map(|(_, name)| *name)
}

/// Resolve an ordinal-imported Winsock symbol back to its export name.
///
/// Returns `None` for anything that is not an `Ordinal_N` import from
/// `ws2_32`/`wsock32` — the library check matters because every other DLL numbers
/// its own exports from 1 too (oleaut32's `Ordinal_2` is not `bind`).
///
/// # Arguments
///
/// * `symbol` - The imported symbol to resolve
/// * `libraries` - The importing library of each import, from [`import_libraries`]
fn ws2_32_ordinal(symbol: &Symbol, libraries: &HashMap<String, String>) -> Option<&'static str> {
    // look the import's library up by address, then keep only Winsock providers
    let library = libraries.get(&symbol.address)?;
    if !is_winsock_library(library) {
        return None;
    }
    // the name is only an ordinal placeholder, so map it back to the export
    winsock_ordinal(&symbol.name)
}

/// Look up an ordinal-resolved Winsock API in the socket/DNS catalogs.
///
/// Reuses the catalogs above so an ordinal import carries the same technique,
/// severity, and reasoning as the equivalent named import.
///
/// # Arguments
///
/// * `api` - The resolved export name
fn ordinal_spec(api: &str) -> Option<(AttackTechnique, &'static ApiSpec)> {
    // raw socket calls first, then the resolver calls
    if let Some(spec) = SOCKET_APIS.iter().find(|spec| spec.key == api) {
        return Some((T1095, spec));
    }
    if let Some(spec) = DNS_APIS.iter().find(|spec| spec.key == api) {
        return Some((T1071_004, spec));
    }
    None
}

/// Flag Winsock call sites whose API is imported **by ordinal**.
///
/// The name-keyed [`api_call_hits`] helper cannot do this: matching `Ordinal_23`
/// by name alone would collide with every other library's ordinal 23, so the
/// library has to be checked first. Everything after the lookup mirrors
/// `api_call_hits` — thunk-aware call-site resolution, with the sites grouped by
/// their calling function into one hit apiece.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `libraries` - The importing library of each import, from [`import_libraries`]
///
/// # Errors
///
/// Returns an error if a bridge call-site query fails.
#[tracing::instrument(name = "network_comms::ordinal_hits", skip_all, err(Debug))]
async fn ordinal_hits(
    ctx: &AnalysisContext,
    libraries: &HashMap<String, String>,
) -> Result<Vec<DetectionHit>> {
    let mut hits = Vec::new();
    for import in &ctx.imports {
        // resolve the ordinal to an export name, then to a catalog entry
        let Some(api) = ws2_32_ordinal(import, libraries) else {
            continue;
        };
        let Some((technique, spec)) = ordinal_spec(api) else {
            continue;
        };
        // an ordinal import that is never called is too weak to report
        let sites = ctx
            .resolve_call_sites(&import.address)
            .await
            .with_context(|| format!("resolving call sites of ordinal import '{}'", import.name))?;
        // thunked and direct sites alike group by their calling function, so an API
        // one function calls repeatedly is one finding naming every call
        let mut thunked: BTreeMap<Option<u64>, Vec<u64>> = BTreeMap::new();
        let mut direct: BTreeMap<Option<u64>, Vec<u64>> = BTreeMap::new();
        let mut thunk_entry: Option<u64> = None;
        for call in sites {
            if call.via_thunk.is_some() {
                // remember the thunk (for the reasoning note) and collect the site
                thunk_entry = thunk_entry.or(call.via_thunk);
                thunked.entry(call.function).or_default().push(call.site);
            } else {
                // ordinary direct call: collect it under its calling function
                direct.entry(call.function).or_default().push(call.site);
            }
        }
        // one hit per calling function, for the thunked then the direct call sites
        for (via_thunk, groups) in [(thunk_entry, thunked), (None, direct)] {
            for (function, mut group) in groups {
                // sort + dedupe the call addresses for stable, clean output
                group.sort_unstable();
                group.dedup();
                hits.push(build_api_hit(
                    technique.clone(),
                    EvidenceKind::ContextOnly,
                    spec,
                    api,
                    function,
                    &group,
                    via_thunk,
                ));
            }
        }
    }
    Ok(hits)
}

// ===========================================================================
// 1c) Setup/plumbing collapse (one capability finding per function)
// ===========================================================================

/// The setup/plumbing APIs collapsed into one per-function capability finding.
///
/// Exactly the `low()`-graded session/socket/TLS setup calls from the catalogs above:
/// ubiquitous in network-capable code, so a function calling several of them reads
/// better as one "network capability" anchor than as one finding per API. The calls
/// that move traffic (`connect`/`send`/`recv`/`HttpSendRequest`/…) are `medium` and
/// keep their own findings.
const PLUMBING_APIS: &[&str] = &[
    "WSAStartup",
    "socket",
    "WSASocket",
    "bind",
    "setsockopt",
    "ioctlsocket",
    "select",
    "poll",
    "closesocket",
    "inet_addr",
    "inet_pton",
    "InternetOpen",
    "WinHttpOpen",
    "curl_global_init",
    "curl_easy_init",
    "SSL_CTX_new",
    "SSL_new",
    "TLS_client_method",
    "AcquireCredentialsHandle",
];

/// The plumbing API a call-site hit flags, when the hit is collapsible.
///
/// Reads the API name off the hit's content head (the shared `build_api_hit` format
/// leads with the normalized name in every variant) and admits only resolved
/// call-site hits: the hit must sit in a known function and carry the `low()` setup
/// grade — a `medium` traffic-moving call, a symbol/IOC hit, and the function-less
/// "imported but never called" hits all keep their own flags.
///
/// # Arguments
///
/// * `hit` - The candidate hit
fn plumbing_key(hit: &DetectionHit) -> Option<&'static str> {
    // only a hit attributed to a function can join a per-function capability group
    hit.function?;
    // plumbing is exactly the low() setup grade; anything stronger keeps its flag
    if !matches!(hit.confidence, Confidence::Untrusted) {
        return None;
    }
    // the content head is the normalized API name in every build_api_hit format
    let head = hit.content.split_whitespace().next()?;
    PLUMBING_APIS
        .iter()
        .copied()
        .find(|api| api.eq_ignore_ascii_case(head))
}

/// Merge one function's plumbing hits into a single capability finding.
///
/// Keeps everything the per-API findings carried — every API name and every call
/// site — but says it once, in capability language, with no per-call intent claim.
///
/// # Arguments
///
/// * `name` - The function's name, when it has one
/// * `entry` - The function's entry address
/// * `members` - The `(api, hit)` pairs collapsing into this finding
fn plumbing_hit(
    name: Option<&str>,
    entry: u64,
    members: &[(&'static str, DetectionHit)],
) -> DetectionHit {
    // group every call site under its API so repeats (thunked + direct) merge
    let mut by_api: BTreeMap<&'static str, Vec<u64>> = BTreeMap::new();
    for (api, hit) in members {
        // a hit with no explicit site list is anchored at its primary address
        let sites = if hit.sites.is_empty() {
            vec![hit.address]
        } else {
            hit.sites.clone()
        };
        by_api.entry(api).or_default().extend(sites);
    }
    // render each API with its (sorted, deduped) sites and pool the full site list
    let mut all_sites: Vec<u64> = Vec::new();
    let mut listed: Vec<String> = Vec::new();
    for (api, sites) in &mut by_api {
        sites.sort_unstable();
        sites.dedup();
        all_sites.extend(sites.iter().copied());
        let rendered = sites
            .iter()
            .map(|site| fmt_addr(*site))
            .collect::<Vec<_>>()
            .join(", ");
        listed.push(format!("{api} @ {rendered}"));
    }
    all_sites.sort_unstable();
    all_sites.dedup();
    // the members' technique when they agree, else the T1071 family parent
    let technique = match members.first() {
        Some((_, first)) => {
            // any disagreement means mixed setup, which only the parent covers
            if members
                .iter()
                .all(|(_, hit)| hit.technique.id == first.technique.id)
            {
                first.technique.clone()
            } else {
                T1071
            }
        }
        None => T1071,
    };
    // anchor at the earliest call site so the finding opens on real code
    let address = all_sites.first().copied().unwrap_or(entry);
    // name the function so the finding reads without cross-referencing
    let func = name.map_or_else(|| fmt_addr(entry), str::to_string);
    let content = format!("network capability setup in {func} — {}", listed.join("; "));
    let apis: Vec<&str> = by_api.keys().copied().collect();
    let reasoning = format!(
        "This function performs the setup half of network communication: {}. Each of these APIs \
         is ubiquitous in network-capable code (benign and malicious alike), so they are reported \
         once per function as a capability anchor rather than one finding per API — every API \
         name and call site is listed above. Corroborate what the capability is used for at the \
         traffic-moving calls (connect/send/recv/HttpSendRequest and peers) and at the endpoints \
         this function builds.",
        apis.join(", ")
    );
    // assemble the merged hit; attach the site list only when there is more than one
    let mut hit = DetectionHit::new(technique, address, Some(entry), low(), content, reasoning)
        .name(format!(
            "Network Capability Setup - {} APIs @ {}",
            by_api.len(),
            fmt_addr(address)
        ))
        .kind(EvidenceKind::ContextOnly);
    if all_sites.len() > 1 {
        hit = hit.sites(all_sites);
    }
    hit
}

/// Collapse the per-API setup/plumbing hits that share one function into single
/// capability findings.
///
/// A networked binary otherwise wallpapers the report with one `low()` finding per
/// setup API per calling function (`WSAStartup`, `socket`, `setsockopt`, …) —
/// capability restatements that bury the traffic-moving calls. Functions with a
/// single plumbing hit keep the ordinary per-API finding, and everything `medium()`
/// and above passes through untouched.
///
/// # Arguments
///
/// * `ctx` - The analysis context (names the merged findings' functions)
/// * `hits` - The API/ordinal call-site hits to collapse
fn collapse_plumbing(ctx: &AnalysisContext, hits: Vec<DetectionHit>) -> Vec<DetectionHit> {
    // split the collapsible plumbing calls out from everything else
    let mut kept = Vec::with_capacity(hits.len());
    let mut groups: BTreeMap<u64, Vec<(&'static str, DetectionHit)>> = BTreeMap::new();
    for hit in hits {
        // a hit outside the plumbing set keeps its own finding untouched
        match plumbing_key(&hit) {
            Some(api) => {
                // plumbing_key only admits hits with a resolved function
                let entry = hit.function.unwrap_or_default();
                groups.entry(entry).or_default().push((api, hit));
            }
            None => kept.push(hit),
        }
    }
    // merge each function's plumbing group; a lone setup call is not wallpaper
    for (entry, members) in groups {
        if members.len() < 2 {
            kept.extend(members.into_iter().map(|(_, hit)| hit));
            continue;
        }
        kept.push(plumbing_hit(ctx.function_name(entry), entry, &members));
    }
    kept
}

// ===========================================================================
// 2) Runtime symbol scan (statically-linked Go / Rust)
// ===========================================================================

/// One runtime networking marker: a lower-case substring of a function name that
/// betrays a networking package compiled into the binary.
struct SymbolMarker {
    /// The lower-case substring matched against each Ghidra function name.
    marker: &'static str,
    /// The protocol family the package implements.
    proto: Proto,
    /// The package/runtime the marker identifies (named in the flag content).
    package: &'static str,
}

/// Networking package markers for statically-linked Go and Rust binaries.
///
/// A Go/Rust binary has no import table, so its networking is only visible in the
/// symbol names Ghidra recovers. Go package paths appear verbatim (`net/http.…`);
/// Rust symbols appear either demangled (`hyper::client::…`) or mangled with a
/// length prefix (`_ZN5hyper6client…`), so both forms are listed. Markers are kept
/// `::`/length-qualified where a bare word would collide with unrelated code — e.g.
/// a plain `hyper` would match `hypervisor` in an anti-VM sample.
const SYMBOL_MARKERS: &[SymbolMarker] = &[
    // --- Go standard library / ecosystem ---
    SymbolMarker {
        marker: "net/http",
        proto: Proto::Web,
        package: "Go net/http",
    },
    SymbolMarker {
        marker: "net/url",
        proto: Proto::Web,
        package: "Go net/url",
    },
    SymbolMarker {
        marker: "crypto/tls",
        proto: Proto::Tls,
        package: "Go crypto/tls",
    },
    SymbolMarker {
        marker: "net.dial",
        proto: Proto::Socket,
        package: "Go net.Dial*",
    },
    SymbolMarker {
        marker: "net.listen",
        proto: Proto::Socket,
        package: "Go net.Listen*",
    },
    SymbolMarker {
        marker: "net.lookup",
        proto: Proto::Dns,
        package: "Go net.Lookup*",
    },
    SymbolMarker {
        marker: "net.resolver",
        proto: Proto::Dns,
        package: "Go net.Resolver",
    },
    SymbolMarker {
        marker: "quic-go",
        proto: Proto::Quic,
        package: "Go quic-go",
    },
    SymbolMarker {
        marker: "lucas-clemente",
        proto: Proto::Quic,
        package: "Go quic-go (lucas-clemente)",
    },
    // --- Rust crates (demangled `crate::` form and mangled `_ZN<len><crate>` form) ---
    SymbolMarker {
        marker: "reqwest",
        proto: Proto::Web,
        package: "Rust reqwest",
    },
    SymbolMarker {
        marker: "hyper::",
        proto: Proto::Web,
        package: "Rust hyper",
    },
    SymbolMarker {
        marker: "5hyper",
        proto: Proto::Web,
        package: "Rust hyper",
    },
    SymbolMarker {
        marker: "rustls",
        proto: Proto::Tls,
        package: "Rust rustls",
    },
    SymbolMarker {
        marker: "native_tls",
        proto: Proto::Tls,
        package: "Rust native-tls",
    },
    SymbolMarker {
        marker: "quinn::",
        proto: Proto::Quic,
        package: "Rust quinn (QUIC)",
    },
    SymbolMarker {
        marker: "5quinn",
        proto: Proto::Quic,
        package: "Rust quinn (QUIC)",
    },
    SymbolMarker {
        marker: "tokio::net",
        proto: Proto::Socket,
        package: "Rust tokio::net",
    },
    SymbolMarker {
        marker: "std::net::",
        proto: Proto::Socket,
        package: "Rust std::net",
    },
    SymbolMarker {
        marker: "mio::net",
        proto: Proto::Socket,
        package: "Rust mio::net",
    },
    SymbolMarker {
        marker: "trust_dns",
        proto: Proto::Dns,
        package: "Rust trust-dns",
    },
    SymbolMarker {
        marker: "hickory_dns",
        proto: Proto::Dns,
        package: "Rust hickory-dns",
    },
];

/// Return the networking marker a function name matches, if any.
///
/// Matching is a lower-case substring test in catalog order, so the first marker
/// wins. Kept pure (no context) so the false-positive guards are unit-testable.
///
/// # Arguments
///
/// * `name` - The Ghidra function name to classify
fn symbol_marker(name: &str) -> Option<&'static SymbolMarker> {
    // the matcher itself is shared with the other runtime-symbol catalogs
    crate::thorpal::detect::indicators::match_runtime_marker(name, SYMBOL_MARKERS, |m| m.marker)
}

/// Flag functions whose names betray a linked networking package (Go/Rust).
///
/// This is the angle that surfaces networking in statically-linked binaries, where
/// the import table is empty and the API catalogs above never fire. Each marker is
/// capped at [`MAX_FUNCS_PER_MARKER`] representative functions (a Go binary has
/// hundreds of `net/http` functions) and the pass as a whole at [`MAX_SYMBOL_HITS`].
///
/// # Arguments
///
/// * `ctx` - The analysis context (its cached function list is the only input)
#[tracing::instrument(name = "network_comms::symbol_hits", skip_all)]
fn symbol_hits(ctx: &AnalysisContext) -> Vec<DetectionHit> {
    let mut hits = Vec::new();
    // how many functions each marker has contributed so far (per-marker cap)
    let mut per_marker: HashMap<&'static str, usize> = HashMap::new();
    // one flag per function, even when several markers would match its name
    let mut seen: HashSet<u64> = HashSet::new();
    for function in &ctx.functions {
        // stop once the pass has produced enough representative examples
        if hits.len() >= MAX_SYMBOL_HITS {
            break;
        }
        // a compiler/runtime support function is noise even if its name matches
        if is_library_function(&function.name) {
            continue;
        }
        // classify the name against the marker catalog
        let Some(marker) = symbol_marker(&function.name) else {
            continue;
        };
        // honor the per-marker cap so one package cannot flood the report
        let count = per_marker.entry(marker.marker).or_default();
        if *count >= MAX_FUNCS_PER_MARKER {
            continue;
        }
        // locate the function; skip any whose entry we cannot parse
        let Some(entry) =
            parse_addr(&function.entry_point).or_else(|| parse_addr(&function.address))
        else {
            continue;
        };
        // never flag the same function twice
        if !seen.insert(entry) {
            continue;
        }
        *count += 1;
        // the flag is anchored at the function entry it names
        let proto = marker.proto;
        hits.push(
            DetectionHit::new(
                proto.technique(),
                entry,
                Some(entry),
                medium(),
                format!(
                    "{} symbol — {} @ {} ({})",
                    marker.package,
                    function.name,
                    fmt_addr(entry),
                    proto.label()
                ),
                format!(
                    "The function name shows {} linked into this binary, giving it {} capability. \
                     Statically-linked Go/Rust binaries have no import table, so networking is only \
                     visible in the symbol names — this is the same evidence an import of \
                     `connect`/`WinHttpSendRequest` would be in a dynamically-linked sample. Follow \
                     the callers of this function to find the endpoint it talks to.",
                    marker.package,
                    proto.label()
                ),
            )
            .kind(EvidenceKind::ContextOnly),
        );
    }
    hits
}

// ===========================================================================
// 3) Network IOC extraction from strings
// ===========================================================================

/// A network indicator of compromise recovered from a defined string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NetIoc {
    /// A full URL, parsed into scheme/host/port/path.
    Url(UrlIoc),
    /// A bare `host:port` (or `ip:port`) endpoint literal.
    Endpoint {
        /// The lower-case host (domain or IPv4 literal).
        host: String,
        /// The explicit port.
        port: u16,
        /// The literal as it appears in the binary.
        raw: String,
    },
    /// A piece of the HTTP request fabric: a User-Agent, header, URI path, or the
    /// template one of those is built from.
    Http {
        /// Which piece of the fabric this is.
        artifact: HttpArtifact,
        /// The string as it appears in the binary.
        raw: String,
    },
    /// A bare IP literal with no port.
    Ip {
        /// The normalized address text.
        value: String,
        /// The address family (`"IPv4"` / `"IPv6"`).
        family: &'static str,
    },
}

impl NetIoc {
    /// The IOC type name, used as the flag's headline noun and the per-class cap key.
    ///
    /// Each HTTP artifact reports under its own name rather than a shared "HTTP
    /// artifact" label, so a User-Agent and a URI template are counted, capped, and
    /// grouped separately.
    pub(super) fn kind(&self) -> &'static str {
        // one stable label per class (also the report's grouping hint)
        match self {
            NetIoc::Url(_) => "URL",
            NetIoc::Endpoint { .. } => "Network Endpoint",
            NetIoc::Http { artifact, .. } => artifact.label(),
            NetIoc::Ip { .. } => "IP Address",
        }
    }

    /// The max flags this IOC's class contributes to the report.
    fn cap(&self) -> usize {
        // each class is bounded on its own so no class can starve another
        match self {
            NetIoc::Url(_) => MAX_URL_HITS,
            NetIoc::Endpoint { .. } => MAX_ENDPOINT_HITS,
            NetIoc::Http { .. } => MAX_HTTP_CLASS_HITS,
            NetIoc::Ip { .. } => MAX_IP_HITS,
        }
    }

    /// The report artifact class this IOC is inventoried under.
    fn artifact_class(&self) -> ArtifactClass {
        match self {
            // everything address-shaped is an endpoint, however it was written
            NetIoc::Url(_) | NetIoc::Endpoint { .. } | NetIoc::Ip { .. } => {
                ArtifactClass::NetworkEndpoint
            }
            NetIoc::Http { artifact, .. } => match artifact {
                HttpArtifact::UserAgent => ArtifactClass::UserAgent,
                // a whole request line is read for its path the same way a bare one is
                HttpArtifact::UriPath
                | HttpArtifact::UriTemplate
                | HttpArtifact::RequestTemplate => ArtifactClass::UriPath,
                HttpArtifact::HeaderName
                | HttpArtifact::HeaderTemplate
                | HttpArtifact::CookieName
                | HttpArtifact::ContentType => ArtifactClass::HttpHeader,
            },
        }
    }

    /// The raw text the IOC was recovered from (the flag's snippet + dedup key).
    pub(super) fn value(&self) -> &str {
        // every variant keeps the literal as it appears in the binary
        match self {
            NetIoc::Url(url) => &url.raw,
            NetIoc::Endpoint { raw, .. } => raw,
            NetIoc::Http { raw, .. } => raw,
            NetIoc::Ip { value, .. } => value,
        }
    }

    /// The ATT&CK technique this IOC evidences.
    pub(super) fn technique(&self) -> AttackTechnique {
        match self {
            // web schemes are application-layer web protocols; anything else (ftp…)
            // falls back to the T1071 parent
            NetIoc::Url(url) => match url.scheme.as_str() {
                "http" | "https" | "ws" | "wss" => T1071_001,
                _ => T1071,
            },
            // a bare endpoint / IP literal is transport-level addressing
            NetIoc::Endpoint { .. } | NetIoc::Ip { .. } => T1095,
            // user-agents and request templates are web-protocol artifacts
            NetIoc::Http { .. } => T1071_001,
        }
    }

    /// The `(confidence, suspicion)` class severity for this IOC.
    ///
    /// Class-level: the unreferenced-string downgrade is applied by [`ioc_hit`], not
    /// here, so the pool pass can also read this as a member's substance. Fragment
    /// URLs never reach it — they are folded into the digest before scoring.
    pub(super) fn severity(&self) -> (Confidence, i64) {
        match self {
            // a Tor hidden service is a deliberate anonymity choice — strong signal
            NetIoc::Url(url) if url.is_onion() => high(),
            // a hardcoded URL or endpoint is a meaningful, pivotable indicator
            NetIoc::Url(_) | NetIoc::Endpoint { .. } => medium(),
            // the request fabric scores by how much of it the program had to write
            // itself: a presented User-Agent or a hand-built template is a fingerprint,
            // while a bare media type is shared with every HTTP client alive
            NetIoc::Http { artifact, .. } => match artifact {
                HttpArtifact::UserAgent
                | HttpArtifact::RequestTemplate
                | HttpArtifact::HeaderTemplate
                | HttpArtifact::UriTemplate
                | HttpArtifact::CookieName => medium(),
                HttpArtifact::UriPath | HttpArtifact::HeaderName | HttpArtifact::ContentType => {
                    low()
                }
            },
            // a bare dotted quad is indistinguishable from a version number statically
            NetIoc::Ip { .. } => low(),
        }
    }

    /// The extracted fields rendered as `(label, value)` pairs for the flag content.
    fn details(&self) -> Vec<(&'static str, String)> {
        match self {
            NetIoc::Url(url) => {
                // spell out every part an analyst would pivot on
                let mut out = vec![("Scheme", url.scheme.clone()), ("Host", url.host.clone())];
                // distinguish an explicit port from the scheme's implied default
                match (url.port, url.effective_port()) {
                    (Some(port), _) => out.push(("Port", format!("{port} (explicit)"))),
                    (None, Some(port)) => out.push(("Port", format!("{port} (scheme default)"))),
                    (None, None) => {}
                }
                out.push(("Path", url.path.clone()));
                // call out Tor explicitly; it changes how the endpoint is hunted
                if url.is_onion() {
                    out.push(("Tor", "yes — .onion hidden service".to_string()));
                }
                out
            }
            NetIoc::Endpoint { host, port, .. } => vec![
                ("Host", host.clone()),
                (
                    "Port",
                    format!(
                        "{port} ({})",
                        if is_standard_port(*port) {
                            "well-known service port"
                        } else {
                            "non-standard port"
                        }
                    ),
                ),
            ],
            NetIoc::Http { artifact, raw } => {
                let mut out = vec![("Artifact", artifact.label().to_string())];
                // a template is filled in at request time, so the literal is the shape
                // of what goes on the wire rather than the value itself
                if artifact.is_template() {
                    out.push((
                        "Filled at runtime",
                        "yes — carries a format specifier or placeholder".to_string(),
                    ));
                }
                // for a header line, the field name is the part that survives into a
                // proxy log independently of the value
                if let Some((field, _)) = raw.split_once(':') {
                    if !field.contains(' ') && !field.is_empty() && field.len() <= 40 {
                        out.push(("Field", field.to_string()));
                    }
                }
                out
            }
            NetIoc::Ip { value, family } => {
                vec![
                    ("Address", value.clone()),
                    ("Family", (*family).to_string()),
                ]
            }
        }
    }

    /// The analyst-facing explanation of why this IOC matters.
    fn reasoning(&self) -> String {
        match self {
            // a .onion URL is never incidental
            NetIoc::Url(url) if url.is_onion() => format!(
                "Hardcoded Tor hidden-service URL ({}). A .onion address only resolves through Tor, \
                 so the operator deliberately anonymized the C2 endpoint and it cannot be sinkholed \
                 or resolved by ordinary DNS telemetry. Recovered statically from a defined string, \
                 with the scheme/host/port/path parsed out for hunting.",
                url.raw
            ),
            NetIoc::Url(url) => format!(
                "Hardcoded {} URL ({}) embedded in the binary — a candidate C2 endpoint, payload \
                 download location, or exfiltration target. The scheme/host/port/path are parsed \
                 out so the host and URI path can be pivoted on directly (URI paths are often the \
                 most distinctive part of a beacon).",
                url.scheme.to_uppercase(),
                url.raw
            ),
            NetIoc::Endpoint { host, port, .. } => format!(
                "Hardcoded network endpoint {host}:{port} embedded in the binary — a candidate C2 \
                 address the sample connects to directly, with no URL or DNS name to resolve first.{}",
                if is_standard_port(*port) {
                    ""
                } else {
                    " The port is non-standard, which is typical of a bespoke C2 listener."
                }
            ),
            NetIoc::Http { artifact, raw } => http_reasoning(*artifact, raw),
            NetIoc::Ip { value, family } => format!(
                "Hardcoded {family} literal ({value}) embedded in the binary — a candidate C2 or \
                 exfiltration address used without a DNS lookup (leaving no DNS artifact to hunt). \
                 Low confidence: a dotted quad is statically indistinguishable from a version \
                 number or an unrelated constant, so confirm the string is used in socket setup.",
            ),
        }
    }
}

/// The analyst-facing explanation for one recovered piece of the request fabric.
///
/// Each class is worth a different thing to the person reading the report, so each
/// gets its own sentence rather than a shared "HTTP artifact" line: what the literal
/// is, what it fingerprints, and — for the weak classes — why it is weak.
///
/// # Arguments
///
/// * `artifact` - Which piece of the fabric the literal is
/// * `raw` - The literal as it appears in the binary
fn http_reasoning(artifact: HttpArtifact, raw: &str) -> String {
    match artifact {
        HttpArtifact::UserAgent => format!(
            "Hardcoded User-Agent ({raw:?}) the program presents on its requests. A baked-in \
             User-Agent is one of the strongest network fingerprints a sample has: it is emitted \
             verbatim on every beacon, so it hunts in proxy and IDS logs even after the C2 host \
             rotates."
        ),
        HttpArtifact::RequestTemplate => format!(
            "Hand-built HTTP request template ({raw:?}). The program formats its own request line \
             rather than delegating to an HTTP client library, so the method, path shape, and \
             header order are all attacker-chosen and reproduce exactly on every request — which \
             is what makes them signaturable."
        ),
        HttpArtifact::HeaderTemplate => format!(
            "HTTP header template ({raw:?}) filled in at request time. A header the program builds \
             itself carries whatever it chooses to put in it — a victim or task identifier, a stage \
             marker, or encoded data riding in a header value where body inspection will not see it."
        ),
        HttpArtifact::HeaderName => format!(
            "Hardcoded HTTP header ({raw:?}) the program sends. The specific header set and its \
             order fingerprint the client that built the request, distinguishing hand-rolled \
             traffic from a browser or a stock library client."
        ),
        HttpArtifact::CookieName => format!(
            "Cookie material ({raw:?}). A cookie the program sets itself is a common carrier for a \
             beacon or session identifier, and unlike a URI path it survives redirects and appears \
             in proxy logs as its own field."
        ),
        HttpArtifact::UriPath => format!(
            "URI path ({raw:?}) used against an endpoint. The path is often the most distinctive \
             part of a beacon and hunts independently of the host, which rotates far more often. \
             Low confidence on its own: a leading-slash literal is also how a filesystem path \
             looks, so confirm it is concatenated onto a URL before treating it as network data."
        ),
        HttpArtifact::UriTemplate => format!(
            "URI path template ({raw:?}) filled in at request time. The fixed segments still hunt \
             as a pattern, and the placeholder marks where the program injects per-victim data — an \
             identifier, a task number, or encoded output."
        ),
        HttpArtifact::ContentType => format!(
            "Media type ({raw:?}) the program sends or accepts. Weak on its own, but it says what \
             the request bodies are shaped like, which narrows what is being uploaded and how the \
             receiving side parses it."
        ),
    }
}

/// Classify a defined string as a network IOC, if it is one.
///
/// Tried most-specific first: a full URL, then a piece of the HTTP request fabric (a
/// request template may also embed a bare host), then a `host:port` endpoint, then a
/// bare IP literal.
///
/// # Arguments
///
/// * `value` - The defined string's value
fn classify_ioc(value: &str) -> Option<NetIoc> {
    // ignore strings too short to hold an endpoint or pathologically long ones
    let trimmed = value.trim();
    if trimmed.len() < 5 || trimmed.len() > 2048 {
        return None;
    }
    // 1) a full URL carries the most information, so it wins
    if let Some(url) = parse_url(trimmed) {
        return Some(NetIoc::Url(url));
    }
    // 2) a piece of the HTTP request fabric (user-agent, header, URI path, template).
    // Safe to run ahead of the endpoint parse: a header name is an undotted token, and
    // an endpoint host is a dotted name or an IP literal, so no literal satisfies both.
    if let Some(artifact) = classify_http(trimmed) {
        return Some(NetIoc::Http {
            artifact,
            raw: trimmed.to_string(),
        });
    }
    // 3) a bare host:port / ip:port endpoint
    if let Some((host, port)) = parse_endpoint(trimmed) {
        return Some(NetIoc::Endpoint {
            host,
            port,
            raw: trimmed.to_string(),
        });
    }
    // 4) a bare IP literal (weakest — a dotted quad may just be a version)
    if let Some(addr) = parse_ipv4_literal(trimmed) {
        return Some(NetIoc::Ip {
            value: addr.to_string(),
            family: "IPv4",
        });
    }
    if let Some(addr) = parse_ipv6_literal(trimmed) {
        return Some(NetIoc::Ip {
            value: addr.to_string(),
            family: "IPv6",
        });
    }
    None
}

/// Shorten a recovered value for embedding in a flag name.
///
/// # Arguments
///
/// * `value` - The value to shorten
fn short_value(value: &str) -> String {
    /// The max characters kept in a flag-name snippet.
    const MAX: usize = 80;
    // return as-is when short enough, else clip with an ellipsis
    if value.chars().count() <= MAX {
        value.to_string()
    } else {
        let clipped: String = value.chars().take(MAX - 1).collect();
        format!("{clipped}…")
    }
}

/// Build the flag `content` markdown for a recovered network IOC: the literal in a
/// fenced block, the extracted fields, and where it lives / is referenced from.
///
/// # Arguments
///
/// * `ioc` - The classified IOC
/// * `string_addr` - The address of the defined string it came from
/// * `sites` - The code sites that reference the string (may be empty)
fn ioc_content(ioc: &NetIoc, string_addr: u64, sites: &[u64]) -> String {
    let mut out = String::new();
    // the literal itself, fenced so the report renders it verbatim
    out.push_str(&format!("### Network IOC — {}\n\n", ioc.kind()));
    out.push_str("```\n");
    out.push_str(ioc.value());
    out.push_str("\n```\n");
    // the parsed fields an analyst pivots on
    out.push_str("\n### Extracted\n");
    for (label, value) in ioc.details() {
        out.push_str(&format!("- **{label}:** {value}\n"));
    }
    // where the literal lives and who uses it
    out.push_str("\n### Metadata\n");
    out.push_str(&format!(
        "- **String address:** {}\n",
        fmt_addr(string_addr)
    ));
    if sites.is_empty() {
        out.push_str("- **Referenced from:** (no code reference resolved)\n");
    } else {
        // a heavily used literal can carry a hundred sites; list the first few and say
        // how many were left off rather than printing a wall of addresses
        let rendered = sites
            .iter()
            .take(MAX_LISTED_SITES)
            .map(|a| fmt_addr(*a))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("- **Referenced from:** {rendered}"));
        if let Some(extra) = sites.len().checked_sub(MAX_LISTED_SITES).filter(|n| *n > 0) {
            out.push_str(&format!(" (+{extra} more; {} sites in total)", sites.len()));
        }
        out.push('\n');
    }
    out
}

/// A network IOC recovered from a defined string, together with its attribution.
///
/// Produced **uncapped** by [`ioc_records`] so that the pooling pass in
/// [`super::ioc_pools`] sees every recovered indicator. A pool's shape is only
/// meaningful over the whole set: grouping over the capped subset would report a
/// six-member pool as a three-member one with no sign that anything was missing.
#[derive(Debug, Clone)]
pub(super) struct IocRecord {
    /// The classified indicator.
    pub(super) ioc: NetIoc,
    /// The address of the defined string it was recovered from.
    pub(super) string_addr: u64,
    /// Every code site referencing the string, in address order (may be empty).
    pub(super) sites: Vec<u64>,
    /// The function the indicator is attributed to, when one was resolved.
    pub(super) function: Option<u64>,
}

/// Recover every distinct network IOC (URLs, endpoints, request fabric, IP literals)
/// from the defined strings **and** from the raw bytes of the data sections, with the
/// function each is attributed to.
///
/// Attribution is the first code site that references the string, or — when nothing
/// references it — whichever function references the surrounding buffer, matching
/// `string_artifact_hits`' attribution. Deduplicated by class + value, and deliberately
/// uncapped; [`ioc_string_hits`] applies the per-class caps.
///
/// # Arguments
///
/// * `ctx` - The analysis context
///
/// # Errors
///
/// Returns an error if a bridge xref query fails.
#[tracing::instrument(name = "network_comms::ioc_records", skip_all, err(Debug))]
pub(super) async fn ioc_records(ctx: &AnalysisContext) -> Result<Vec<IocRecord>> {
    let mut records = Vec::new();
    // dedup by IOC class + value, so the same URL in three places is recorded once
    let mut seen: HashSet<String> = HashSet::new();
    // the reverse data-reference index, built lazily only if an unreferenced IOC needs it
    let mut index: Option<Vec<(u64, u64)>> = None;
    for string in &ctx.strings {
        // classify the string; most strings are not network IOCs
        let Some(ioc) = classify_ioc(&string.value) else {
            continue;
        };
        // one record per distinct extracted value
        if !seen.insert(format!("{}|{}", ioc.kind(), ioc.value())) {
            continue;
        }
        // locate the string itself
        let string_addr = parse_addr(&string.address).unwrap_or(0);
        // resolve every code site that references it, in address order
        let mut sites: Vec<u64> = ctx
            .xrefs_to(&string.address)
            .await
            .with_context(|| format!("resolving references to string at {}", string.address))?
            .iter()
            .filter_map(|xref| parse_addr(&xref.from))
            .collect();
        sites.sort_unstable();
        sites.dedup();
        // attribute to the first use site's function; with no use site, fall back to
        // whichever function references its buffer (else leave it sample-scoped)
        let function = match sites.first() {
            Some(site) => ctx.containing_function(*site),
            None => {
                // build the reference index once on first need, then bind it
                // directly so the fallback lookup needs no unwrap
                let index = match &mut index {
                    Some(index) => index,
                    slot => slot.insert(ctx.reference_index().await?),
                };
                ctx.containing_function(string_addr)
                    .or_else(|| referencing_function(index, string_addr, DATA_REF_WINDOW))
            }
        };
        records.push(IocRecord {
            ioc,
            string_addr,
            sites,
            function,
        });
    }
    // the defined strings only cover what the analyzer managed to bound; sweep the raw
    // data bytes for the URLs it could not
    records.extend(carved_url_records(ctx, &mut seen, &mut index).await?);
    Ok(records)
}

/// The scheme prefixes the raw-byte sweep anchors on.
///
/// Only the schemes that are self-delimiting enough to carve safely: `://` preceded by
/// a known scheme is a strong enough anchor to start a run inside an unstructured blob,
/// which a bare host name is not.
const SWEEP_SCHEMES: &[&str] = &["https://", "http://"];

/// The max characters kept from a single carved URL.
///
/// Longer than any URL worth reporting, so this only ever truncates a run that the
/// terminator rules failed to bound — and the authority, which is the part worth
/// pivoting on, sits at the front of the run either way.
const MAX_CARVED_URL_LEN: usize = 256;

/// The max URLs the raw-byte sweep carves out of a sample.
const MAX_CARVED_URLS: usize = 200;

/// Whether a byte may appear inside a URL carved out of raw section bytes.
///
/// Printable ASCII minus the characters RFC 3986 requires to be percent-encoded, so the
/// run stops on its own at whitespace, at a NUL, and at the quoting a URL cannot cross.
/// `#` also terminates the run even though it is legal in a URL: a fragment is never
/// transmitted (RFC 3986 §3.5), so carving past one only merges a documentation link
/// with whatever literal follows it in the blob.
///
/// # Arguments
///
/// * `byte` - The candidate byte
fn is_url_byte(byte: u8) -> bool {
    byte.is_ascii_graphic()
        && !matches!(
            byte,
            b'"' | b'<' | b'>' | b'\\' | b'^' | b'`' | b'{' | b'|' | b'}' | b'#'
        )
}

/// The resource extensions used to cut a carved URL loose from the literal that follows
/// it, ordered longest first so `.aspx` is never cut short to `.asp`.
///
/// Deliberately only extensions that end a *web* resource path. A generic extension
/// list would cut real paths in half — plenty of hosts serve `/download/setup` with no
/// extension at all — whereas these are the ones a URL ends on.
const WEB_EXTENSIONS: &[&str] = &[
    "aspx", "html", "jpeg", "json", "woff", "wasm", "asp", "cgi", "css", "dll", "exe", "gif",
    "htm", "ico", "jpg", "jsp", "pdf", "php", "png", "svg", "txt", "xml", "zip", "js",
];

/// Cut a run at the point a second URL starts inside it.
///
/// Two literals concatenated with no separator is the normal case in a statically
/// linked runtime's string blob, and when the second one is itself a URL its scheme is
/// an exact marker of where the first one ended.
///
/// # Arguments
///
/// * `run` - The candidate URL run
fn cut_at_nested_scheme(run: &str) -> &str {
    // search past the first character so the run's own scheme is not the match
    let end = SWEEP_SCHEMES
        .iter()
        .filter_map(|scheme| run[1..].find(scheme).map(|idx| idx + 1))
        .min();
    match end {
        Some(idx) => &run[..idx],
        None => run,
    }
}

/// Cut a run just after a resource extension that is followed by more lowercase text.
///
/// A URL ending `.css` is complete; `.csshz8l2` is that URL with the start of the next
/// literal stuck to it. The lookahead is the whole test — a following `?`, `/` or
/// end-of-run all mean the extension is genuinely part of a longer, valid URL, so only
/// a letter continuing straight on is treated as the seam.
///
/// # Arguments
///
/// * `run` - The candidate URL run
fn cut_after_web_extension(run: &str) -> &str {
    let bytes = run.as_bytes();
    // walk every dot in the run, so the earliest seam wins
    for (idx, _) in run.match_indices('.') {
        // longest extension first, so `.aspx` is not matched as `.asp`
        for ext in WEB_EXTENSIONS {
            let end = idx + 1 + ext.len();
            // the extension has to sit at this dot and be followed by more lowercase
            if run[idx + 1..].len() > ext.len()
                && run[idx + 1..idx + 1 + ext.len()].eq_ignore_ascii_case(ext)
                && bytes[end].is_ascii_lowercase()
            {
                return &run[..end];
            }
        }
    }
    run
}

/// Drop a trailing path segment that contains a colon.
///
/// A colon is legal in a path segment and vanishingly rare in one, but it is how a
/// great many runtime messages are prefixed (`idna: invalid label`), so a path segment
/// carrying one is the next literal rather than part of this URL. Only the path is
/// examined: a colon in the authority is a port, and a colon in a query or fragment is
/// ordinary. The whole segment goes, not just the text from the colon on, because the
/// segment was never part of the URL to begin with.
///
/// # Arguments
///
/// * `run` - The candidate URL run
fn cut_trailing_colon_segment(run: &str) -> &str {
    // find the authority so its port colon is not mistaken for a path one
    let Some(rest) = SWEEP_SCHEMES
        .iter()
        .find_map(|scheme| run.strip_prefix(*scheme))
    else {
        return run;
    };
    let body = run.len() - rest.len();
    let Some(path) = rest.find('/').map(|idx| body + idx) else {
        return run;
    };
    // the path ends where a query or fragment begins
    let end = run[path..]
        .find(['?', '#'])
        .map_or(run.len(), |idx| path + idx);
    // a colon in the path means this segment belongs to whatever follows the URL
    match run[path..end].find(':') {
        Some(idx) => match run[path..path + idx].rfind('/') {
            Some(slash) => &run[..path + slash + 1],
            None => run,
        },
        None => run,
    }
}

/// Carve one URL out of raw bytes, starting at the scheme.
///
/// Reads while the bytes stay legal in a URL, then bounds what it took with the three
/// seam rules — a nested scheme, a resource extension followed by more text, and a path
/// segment with a colon in it. Requires a dotted authority as a last check: a run with
/// an undotted host is either a runtime placeholder (`http://invalid`) or a bare scheme
/// that ran straight into the next literal, and neither is an indicator.
///
/// # Arguments
///
/// * `bytes` - The section bytes
/// * `start` - The offset of the scheme's first byte
fn carve_url(bytes: &[u8], start: usize) -> Option<&str> {
    // take while the bytes stay legal in a URL, bounded so a blob cannot run away
    let limit = bytes.len().min(start + MAX_CARVED_URL_LEN);
    let end = start
        + bytes[start..limit]
            .iter()
            .take_while(|b| is_url_byte(**b))
            .count();
    // legal URL bytes are ASCII, so this cannot fail, but never panic on odd input
    let run = std::str::from_utf8(&bytes[start..end]).ok()?;
    // bound the run at each seam the next literal can leave behind
    let run = cut_trailing_colon_segment(cut_after_web_extension(cut_at_nested_scheme(run)));
    // a scheme with no dotted host behind it is a placeholder, not an indicator
    let rest = SWEEP_SCHEMES
        .iter()
        .find_map(|scheme| run.strip_prefix(*scheme))?;
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    authority.contains('.').then_some(run)
}

/// Carve every URL out of the raw bytes of the data-like sections.
///
/// The defined strings only hold what the analyzer could bound. A statically linked
/// runtime concatenates its literals into one unseparated blob, and the analyzer sees
/// that blob as a single enormous string or as nothing at all, so a program's C2 URLs
/// can be entirely absent from the string list while sitting in plain ASCII in
/// `.rdata`. Anchoring on the scheme finds them without needing the boundaries.
///
/// # Arguments
///
/// * `sections` - The initialized section bytes
fn carve_urls(sections: &[SectionBytes]) -> Vec<(u64, String)> {
    let mut found = Vec::new();
    for (name, base, bytes) in sections {
        // only data-like sections; a scheme in `.text` is an immediate, not a literal
        if !is_data_section(name) {
            continue;
        }
        // anchor on every scheme occurrence and carve a URL out from there
        for offset in 0..bytes.len() {
            // the schemes share no prefix, so at most one can start here
            if !SWEEP_SCHEMES
                .iter()
                .any(|scheme| bytes[offset..].starts_with(scheme.as_bytes()))
            {
                continue;
            }
            if let Some(url) = carve_url(bytes, offset) {
                found.push((base + offset as u64, url.to_string()));
            }
        }
    }
    found
}

/// Turn the carved URLs into attributed records, skipping any already recovered.
///
/// Shares the caller's dedup set so a URL that was also a defined string is recorded
/// once, from the defined string, where the boundaries are known to be right.
/// Attribution follows the same path as an unreferenced defined string: the function
/// containing the address, else whichever function references the surrounding buffer.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `seen` - The `kind|value` keys already recorded
/// * `index` - The lazily built reverse data-reference index
///
/// # Errors
///
/// Returns an error if a bridge xref or section query fails.
#[tracing::instrument(name = "network_comms::carved_url_records", skip_all, err(Debug))]
async fn carved_url_records(
    ctx: &AnalysisContext,
    seen: &mut HashSet<String>,
    index: &mut Option<Vec<(u64, u64)>>,
) -> Result<Vec<IocRecord>> {
    let sections = ctx.initialized_bytes().await?;
    let carved = carve_urls(&sections);
    let mut records = Vec::new();
    for (addr, url) in carved {
        // a carved run still has to pass the same classifier as a defined string
        let Some(ioc) = classify_ioc(&url) else {
            continue;
        };
        // one record per distinct extracted value, defined strings taking precedence
        if !seen.insert(format!("{}|{}", ioc.kind(), ioc.value())) {
            continue;
        }
        // stop before the sweep can flood the report, and say that it did
        if records.len() >= MAX_CARVED_URLS {
            tracing::warn!(
                cap = MAX_CARVED_URLS,
                "raw-byte URL sweep hit its per-sample cap; later carvings were dropped"
            );
            break;
        }
        // a runtime references its blob literals by exact address, so ask for sites
        let mut sites: Vec<u64> = ctx
            .xrefs_to(&fmt_addr(addr))
            .await
            .with_context(|| format!("resolving references to carved URL at {}", fmt_addr(addr)))?
            .iter()
            .filter_map(|xref| parse_addr(&xref.from))
            .collect();
        sites.sort_unstable();
        sites.dedup();
        // attribute exactly as an unreferenced defined string is attributed
        let function = match sites.first() {
            Some(site) => ctx.containing_function(*site),
            None => {
                // build the reference index once on first need, then bind it
                // directly so the fallback lookup needs no unwrap
                let index = match &mut *index {
                    Some(index) => index,
                    slot => slot.insert(ctx.reference_index().await?),
                };
                ctx.containing_function(addr)
                    .or_else(|| referencing_function(index, addr, DATA_REF_WINDOW))
            }
        };
        records.push(IocRecord {
            ioc,
            string_addr: addr,
            sites,
            function,
        });
    }
    Ok(records)
}

/// Whether a section holds compiled-in resources (`.rsrc`) rather than program data.
///
/// # Arguments
///
/// * `name` - The section name
fn is_resource_section(name: &str) -> bool {
    // PE resources live in .rsrc; no data section shares the token
    name.to_ascii_lowercase().contains("rsrc")
}

/// The defined-string values that key a PE `VS_VERSION_INFO` block.
///
/// A dotted quad stored next to one of these is a FileVersion/ProductVersion value,
/// not an address.
const VERSIONINFO_KEYS: &[&str] = &["VS_VERSION_INFO", "FileVersion", "ProductVersion"];

/// The validity gates a recovered IOC record must clear before it becomes a finding.
///
/// Built once per run from facts the earlier sub-passes already have — the networking
/// functions found by the API/symbol passes, the section table, and the defined
/// strings — so the per-record test is pure and cheap. A record a gate rejects is not
/// dropped: [`ioc_string_hits`] folds it into the unseeded-IOC digest, where its value
/// stays visible verbatim for corroboration.
struct IocGates<'a> {
    /// Entries of functions carrying a networking API/symbol hit.
    network_funcs: &'a HashSet<u64>,
    /// The `[start, end)` address ranges of resource sections.
    resource_ranges: Vec<(u64, u64)>,
    /// The addresses of defined VERSIONINFO key strings.
    versioninfo_addrs: Vec<u64>,
}

impl<'a> IocGates<'a> {
    /// Build the gates from the section table and defined strings.
    ///
    /// # Arguments
    ///
    /// * `sections` - The initialized section bytes (names + bounds)
    /// * `strings` - The defined strings (for the VERSIONINFO key addresses)
    /// * `network_funcs` - Entries of functions carrying a networking API/symbol hit
    fn new(
        sections: &[SectionBytes],
        strings: &[StringData],
        network_funcs: &'a HashSet<u64>,
    ) -> Self {
        // the resource sections' address ranges, from the section table
        let resource_ranges = sections
            .iter()
            .filter(|(name, _, _)| is_resource_section(name))
            .map(|(_, base, bytes)| (*base, base + bytes.len() as u64))
            .collect();
        // where the VERSIONINFO keys live, so their neighbours can be recognized
        let versioninfo_addrs = strings
            .iter()
            .filter(|string| {
                let trimmed = string.value.trim();
                VERSIONINFO_KEYS
                    .iter()
                    .any(|key| trimmed.eq_ignore_ascii_case(key))
            })
            .filter_map(|string| parse_addr(&string.address))
            .collect();
        IocGates {
            network_funcs,
            resource_ranges,
            versioninfo_addrs,
        }
    }

    /// The reason a record must go to the digest instead of its own finding, if any.
    ///
    /// Fragment URLs are reader-facing by construction (the fragment never goes on the
    /// wire), and a bare IP literal only earns a finding when real code in a
    /// networking-capable function references it and it does not sit in
    /// resource/VERSIONINFO data — a dotted quad is otherwise statically
    /// indistinguishable from a version number.
    ///
    /// # Arguments
    ///
    /// * `record` - The recovered indicator and its attribution
    /// * `resolve` - Resolves a code site to its containing function's entry
    fn reject(
        &self,
        record: &IocRecord,
        resolve: impl Fn(u64) -> Option<u64>,
    ) -> Option<&'static str> {
        match &record.ioc {
            // a fragment is never transmitted, so the URL was written for a reader
            NetIoc::Url(url) if url.has_fragment() => Some(
                "carries a `#` fragment, so it is a reader-facing (documentation) link that is \
                 never transmitted",
            ),
            NetIoc::Ip { .. } => {
                // a quad inside a resource section is version metadata, not an address
                if self
                    .resource_ranges
                    .iter()
                    .any(|(start, end)| record.string_addr >= *start && record.string_addr < *end)
                {
                    return Some(
                        "stored in a resource section, where a dotted quad is version \
                                 metadata",
                    );
                }
                // a quad next to the VERSIONINFO keys is a FileVersion-style value
                if self
                    .versioninfo_addrs
                    .iter()
                    .any(|addr| record.string_addr.abs_diff(*addr) <= VERSIONINFO_NEIGHBOR_WINDOW)
                {
                    return Some(
                        "adjacent to VERSIONINFO keys, so it reads as a \
                                 FileVersion/ProductVersion value",
                    );
                }
                // a bare IP needs a true code reference from networking-capable code
                let used = record.sites.iter().any(|site| {
                    resolve(*site).is_some_and(|entry| self.network_funcs.contains(&entry))
                });
                if !used {
                    return Some(
                        "not referenced from any function that does networking, so it is \
                                 indistinguishable from a version number",
                    );
                }
                None
            }
            _ => None,
        }
    }
}

/// Whether a function is linked runtime or library code rather than the program's own.
///
/// A statically linked HTTP or TLS stack ships the entire request fabric as literals —
/// every standard header name, every method, a stock User-Agent — so a literal reached
/// only from runtime code says nothing about what the sample's author wrote. Both
/// tests are needed: `is_library_function` catches the recognized library naming
/// conventions, and [`symbol_marker`] catches the Go/Rust networking packages, whose
/// function names are the only marker a stripped static binary leaves.
///
/// # Arguments
///
/// * `name` - The referencing function's name, if it has one
pub(super) fn is_runtime_function(name: Option<&str>) -> bool {
    // no name, no claim — an unnamed function is the program's until shown otherwise
    name.is_some_and(|name| is_library_function(name) || symbol_marker(name).is_some())
}

/// Name the observed relationship between an indicator and the function using it.
///
/// Only what this pass actually saw: that some function references the literal, and
/// whether that function is runtime code. The pass does not read the reference, so it
/// never claims the function *issues* a request with it.
///
/// # Arguments
///
/// * `record` - The recovered indicator and its attribution
/// * `runtime` - Whether the attributed function is linked runtime/library code
fn ioc_role(record: &IocRecord, runtime: bool) -> Option<String> {
    // no function, no role — `ioc_detail` says why instead
    record.function?;
    let noun = record.ioc.kind().to_ascii_lowercase();
    Some(if runtime {
        format!("linked runtime/library code that references this {noun}")
    } else if record.sites.is_empty() {
        format!(
            "references the data buffer this {noun} is stored in; no direct code reference to the \
             literal itself was resolved"
        )
    } else {
        format!("references this {noun}")
    })
}

/// Record the indicator's other users, or say plainly that attribution failed.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `record` - The recovered indicator and its attribution
/// * `runtime` - Whether the attributed function is linked runtime/library code
fn ioc_detail(ctx: &AnalysisContext, record: &IocRecord, runtime: bool) -> String {
    let mut parts = vec![format!("stored at {}", fmt_addr(record.string_addr))];
    // an unattributed artifact is still worth reporting, but it must not silently look
    // attributed, so say outright that nothing was found to own it
    if record.function.is_none() {
        parts.push(format!(
            "no function references this literal within {DATA_REF_WINDOW} bytes, so it is reported \
             unattributed"
        ));
    }
    // every other function that uses it, so a shared indicator is visibly shared
    let mut others: Vec<u64> = record
        .sites
        .iter()
        .filter_map(|site| ctx.containing_function(*site))
        .filter(|entry| Some(*entry) != record.function)
        .collect();
    others.sort_unstable();
    others.dedup();
    if !others.is_empty() {
        let rendered = others
            .iter()
            .map(|entry| match ctx.function_name(*entry) {
                Some(name) => format!("`{name}`"),
                None => fmt_addr(*entry),
            })
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("also referenced by {rendered}"));
    }
    if runtime {
        parts.push("the referencing function is linked runtime/library code".to_string());
    }
    parts.join("; ")
}

/// Whether an IOC hit's recovered value earns the pivot-protected artifact payload.
///
/// Only the endpoint class is gated: a `NetworkEndpoint` artifact vetoes rollup
/// suppression wherever its bucket sits (see the seeder's protected-artifact rule),
/// so it has to be backed by a code xref or a medium-or-better hit. The fabric
/// classes (`UriPath`/`UserAgent`/`HttpHeader`) carry no veto and keep their
/// artifacts unconditionally.
///
/// # Arguments
///
/// * `class` - The artifact class the IOC inventories under
/// * `sites` - The code sites referencing the string
/// * `severity` - The hit's `(confidence, suspicion)` severity as reported
fn endpoint_artifact_justified(
    class: ArtifactClass,
    sites: &[u64],
    severity: (Confidence, i64),
) -> bool {
    // non-endpoint classes are never pivot-protected, so they are never gated
    if class != ArtifactClass::NetworkEndpoint {
        return true;
    }
    // a code xref is direct evidence; a medium+ grade carries its own weight
    !sites.is_empty() || confidence_rank(severity.0) >= confidence_rank(Confidence::Unsure)
}

/// Build the flag for one recovered indicator.
///
/// Returns `None` for a fragment-bearing URL (a reader-facing link the program never
/// transmits — the gates route those to the digest) and for the request-fabric classes
/// that carry nothing distinguishing once they are known to be the linked runtime's
/// own — a bare method, media type, or standard header name reached only from library
/// code tells an analyst nothing, and listing it invites the reader to attribute it to
/// the sample's author. Every other class is kept and weakened instead of dropped,
/// because a User-Agent or URI path in runtime code may still be the one the program
/// actually sends.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `record` - The recovered indicator and its attribution
fn ioc_hit(ctx: &AnalysisContext, record: &IocRecord) -> Option<DetectionHit> {
    let IocRecord {
        ioc,
        string_addr,
        sites,
        function,
    } = record;
    // a URL carrying a fragment is addressed at a reader, never a server (RFC 3986
    // §3.5: the fragment never goes on the wire), so it is digest material only
    if matches!(ioc, NetIoc::Url(url) if url.has_fragment()) {
        return None;
    }
    // who owns the literal, and whether that owner is the program or its runtime
    let name = function.and_then(|entry| ctx.function_name(entry));
    let runtime = is_runtime_function(name);
    // the classes that say nothing once they are the runtime's are not worth a flag
    if runtime
        && matches!(ioc, NetIoc::Http { artifact, .. }
        if matches!(
            artifact,
            HttpArtifact::ContentType | HttpArtifact::HeaderName
        ))
    {
        return None;
    }
    // anchor at the first use site, or at the string itself when nothing references it
    let address = sites.first().copied().unwrap_or(*string_addr);
    // an unreferenced IOC is weaker evidence: it may be dead data
    let severity = if sites.is_empty() {
        low()
    } else {
        ioc.severity()
    };
    let mut reasoning = ioc.reasoning();
    if sites.is_empty() {
        reasoning.push_str(" (string not referenced by code in this analysis)");
    }
    if runtime {
        reasoning.push_str(&format!(
            " The function referencing this literal (`{}`) is linked runtime/library code, so the \
             literal most likely ships with the linked HTTP or TLS stack rather than having been \
             written by the sample's author. Reported, but not attributed to the sample.",
            name.unwrap_or("?")
        ));
    }
    // the content block, plus the owning function the flag boundary would drop
    let mut content = ioc_content(ioc, *string_addr, sites);
    match (function, name) {
        (Some(entry), Some(name)) => {
            content.push_str(&format!("- **Used by:** `{name}` @ {}\n", fmt_addr(*entry)));
        }
        (Some(entry), None) => {
            content.push_str(&format!("- **Used by:** {}\n", fmt_addr(*entry)));
        }
        (None, _) => content.push_str("- **Used by:** (no referencing function resolved)\n"),
    }
    let mut hit = DetectionHit::new(
        ioc.technique(),
        address,
        *function,
        severity,
        content,
        reasoning,
    )
    .name(format!(
        "{} - \"{}\" @ {}",
        ioc.kind(),
        short_value(ioc.value()),
        fmt_addr(address)
    ))
    .kind(EvidenceKind::ContextOnly);
    // a pivot-protected endpoint artifact needs real evidence behind it: it vetoes
    // rollup suppression wherever its bucket sits, so an unreferenced low-grade value
    // keeps its content but not the artifact payload
    if endpoint_artifact_justified(ioc.artifact_class(), sites, severity) {
        let mut artifact = HitArtifact::new(ioc.artifact_class(), ioc.value());
        if let Some(role) = ioc_role(record, runtime) {
            artifact = artifact.role(role);
        }
        artifact = artifact.detail(ioc_detail(ctx, record, runtime));
        hit = hit.artifact(artifact);
    }
    // attach every other use site so the validator sees them all
    if sites.len() > 1 {
        hit = hit.sites(sites.clone());
    }
    // a runtime-owned literal is real but not the sample's, so it drops a rank
    if runtime {
        hit.weaken();
    }
    Some(hit)
}

/// Turn the recovered indicators into one flag each, capped per class, folding the
/// gate-rejected and cap-overflow values into one unseeded-IOC digest.
///
/// The cap is counted per [`NetIoc::kind`] rather than over the whole pass, and every
/// class that truncates is named in a warning — a silent cap reads as "everything was
/// covered" when it was not, and the digest keeps the folded values visible besides.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `records` - Every recovered indicator, from [`ioc_records`]
/// * `gates` - The validity gates junk-shaped records are rejected by
fn ioc_string_hits(
    ctx: &AnalysisContext,
    records: &[IocRecord],
    gates: &IocGates,
) -> Vec<DetectionHit> {
    let mut hits = Vec::new();
    // the records folded into the digest instead of seeding their own flag
    let mut digest: Vec<(&IocRecord, &'static str)> = Vec::new();
    // how many of each class have been flagged, and how many its cap turned away
    let mut flagged: HashMap<&'static str, usize> = HashMap::new();
    let mut dropped: BTreeMap<&'static str, usize> = BTreeMap::new();
    for record in records {
        let kind = record.ioc.kind();
        // the validity gates route junk-shaped records to the digest, not to flags
        if let Some(reason) = gates.reject(record, |site| ctx.containing_function(site)) {
            digest.push((record, reason));
            continue;
        }
        // stop flagging this class once it has had its share, folding the overflow
        // into the digest (and keep counting, so the warning below stays honest)
        if flagged.get(kind).copied().unwrap_or(0) >= record.ioc.cap() {
            *dropped.entry(kind).or_default() += 1;
            digest.push((record, "its class's per-sample flag cap was reached"));
            continue;
        }
        let Some(hit) = ioc_hit(ctx, record) else {
            continue;
        };
        *flagged.entry(kind).or_default() += 1;
        hits.push(hit);
    }
    // never truncate silently — name the class and how many of it went unflagged
    for (kind, count) in dropped {
        tracing::warn!(
            class = kind,
            dropped = count,
            "network IOC class cap reached; additional distinct indicators of this class were \
             folded into the digest",
        );
    }
    // one digest post keeps every folded value visible without a flag apiece
    if !digest.is_empty() {
        hits.push(digest_hit(&digest));
    }
    hits
}

/// How early a folded record's class lists in the digest (lower lists first).
///
/// The digest's line cap ([`MAX_DIGEST_LINES`]) truncates in listing order, so the
/// order has to put the values an analyst can pivot on ahead of the ones that only
/// corroborate: addresses first (a folded C2 IP — `220.181.87.80` — must never fall
/// off the list behind header fabric), URI paths next (they localize a server), the
/// header classes after that, and whole-request fabric last, since it is rebuilt
/// from the pieces above it.
///
/// # Arguments
///
/// * `ioc` - The folded record's classified indicator
fn digest_priority(ioc: &NetIoc) -> u8 {
    match ioc {
        // addresses are what the digest exists to keep visible
        NetIoc::Ip { .. } | NetIoc::Url(_) | NetIoc::Endpoint { .. } => 0,
        NetIoc::Http { artifact, .. } => match artifact {
            // URI paths and templates localize a server, so they list next
            HttpArtifact::UriPath | HttpArtifact::UriTemplate => 1,
            // header classes corroborate rather than pivot
            HttpArtifact::UserAgent
            | HttpArtifact::CookieName
            | HttpArtifact::ContentType
            | HttpArtifact::HeaderTemplate
            | HttpArtifact::HeaderName => 2,
            // whole-request fabric lists last
            HttpArtifact::RequestTemplate => 3,
        },
    }
}

/// Build the one digest finding that lists every unseeded IOC string verbatim.
///
/// Folded values are records, not leads: none earns a finding of its own, but each
/// stays visible — verbatim, with its address and the reason it was folded — so an
/// analyst can still corroborate or pivot on one that turns out to matter. Entries
/// list in class-priority order ([`digest_priority`]; the sort is stable, so ties
/// keep their insertion order), which is also the order the line cap truncates in.
/// The digest carries no artifact payload, so the seeding machinery is free to fold
/// it in turn.
///
/// # Arguments
///
/// * `entries` - The folded records and the reason each was folded
fn digest_hit(entries: &[(&IocRecord, &'static str)]) -> DetectionHit {
    // list addresses ahead of paths, headers and fabric, so the line cap below
    // never drops a pivotable value in favor of request fabric
    let mut ordered: Vec<&(&IocRecord, &'static str)> = entries.iter().collect();
    ordered.sort_by_key(|(record, _)| digest_priority(&record.ioc));
    // one line per folded value, capped so a flood stays readable
    let mut lines: Vec<String> = ordered
        .iter()
        .take(MAX_DIGEST_LINES)
        .map(|(record, reason)| {
            format!(
                "- `{}` — {} @ {} — {reason}",
                record.ioc.value(),
                record.ioc.kind(),
                fmt_addr(record.string_addr)
            )
        })
        .collect();
    // never truncate silently, in the digest either
    if entries.len() > MAX_DIGEST_LINES {
        lines.push(format!(
            "- … and {} more not listed here",
            entries.len() - MAX_DIGEST_LINES
        ));
    }
    // the digest body: what the list is, then the list itself
    let content = format!(
        "### Unseeded network IOC strings\n\n{} strings matched a network-IOC shape but were \
         folded into this digest instead of seeding findings. Each value is listed verbatim for \
         corroboration.\n\n{}\n",
        entries.len(),
        lines.join("\n")
    );
    DetectionHit::new(
        T1071,
        0,
        None,
        low(),
        content,
        "These strings matched a network-IOC shape but failed a validity gate (a reader-facing \
         fragment URL; a version-number-shaped quad in resource data or with no networking code \
         reference) or overflowed their class's flag cap. They are recorded here verbatim rather \
         than seeded as individual findings: treat a value as an indicator only if something \
         else corroborates it.",
    )
    .name(format!(
        "Network IOC Digest - {} unseeded strings",
        entries.len()
    ))
    .kind(EvidenceKind::ContextOnly)
}

// ===========================================================================
// 4) Hardcoded port recovery
// ===========================================================================

/// Convert a recovered immediate into a port number.
///
/// Rejects anything that cannot be a port: a value wider than 16 bits (so the
/// register did not hold a port) or zero (a wildcard, not an endpoint).
///
/// # Arguments
///
/// * `imm` - The recovered immediate value
fn port_from_imm(imm: u64) -> Option<u16> {
    // must fit in 16 bits and name a real port
    u16::try_from(imm).ok().filter(|port| *port > 0)
}

/// Recover the first integer argument of a `htons()` call from the code before it.
///
/// Walks backwards to the *closest* write of a first-argument register — `edi`/`rdi`
/// (System V), `ecx`/`rcx` (Windows x64) — or a pushed immediate (32-bit cdecl/
/// stdcall, where `htons`' single argument is the last thing pushed. The value is
/// the port in **host** order: `htons` is what converts it to network order.
///
/// # Arguments
///
/// * `preceding` - The instructions before the call site, in program order
fn port_arg(preceding: &[Instruction]) -> Option<u16> {
    // the closest write to the call is the one that reaches it
    for insn in preceding.iter().rev() {
        let mnemonic = insn.mnemonic.to_ascii_uppercase();
        let ops = crate::thorpal::detect::op_str(insn).to_ascii_lowercase();
        // `push imm` — the 32-bit ABIs pass the argument on the stack
        if mnemonic == "PUSH" {
            // a pushed register tells us nothing; keep looking back
            let Some(imm) = parse_imm(ops.trim()) else {
                continue;
            };
            return port_from_imm(imm);
        }
        // `mov <arg reg>, imm` — the 64-bit ABIs pass it in a register
        if mnemonic == "MOV" || mnemonic == "MOVZX" {
            for reg in ["edi,", "rdi,", "ecx,", "rcx,", "di,", "cx,"] {
                if ops.starts_with(reg) {
                    return ops
                        .split(',')
                        .nth(1)
                        .and_then(parse_imm)
                        .and_then(port_from_imm);
                }
            }
        }
    }
    None
}

/// Recover a port from a 16-bit store into `sockaddr_in.sin_port`.
///
/// An optimizing compiler folds `htons(4444)` into the constant it produces, so the
/// call disappears and the network-order value is stored directly
/// (`MOV word ptr [RBP + -0x1e], 0x5c11`). `sin_port` is big-endian, so the host
/// port is the byte-swapped immediate. Character-store artifacts are rejected before
/// the swap: a single-byte immediate (an ASCII/UTF-16 char, a size field, a flag)
/// byte-swaps to a multiple-of-256 "port", and an immediate whose both bytes are
/// ASCII alphanumeric is a two-character string fragment (`"rs"`, `"nW"`) laid down
/// by stack string building. The swapped result is then only accepted when it is
/// plausible — a well-known port or one above 1023 — since any 16-bit store would
/// otherwise match.
///
/// # Arguments
///
/// * `insn` - The candidate store instruction
fn folded_port(insn: &Instruction) -> Option<u16> {
    // only a 16-bit store of an immediate into memory can be a sin_port write
    if !insn.mnemonic.eq_ignore_ascii_case("MOV") {
        return None;
    }
    // op_str yields an empty string when there are no operands; the prefix check below
    // then rejects it just as an early `None` return would have
    let ops = crate::thorpal::detect::op_str(insn).to_ascii_lowercase();
    if !ops.starts_with("word ptr [") {
        return None;
    }
    // the last operand is the stored immediate, in network (big-endian) order
    let imm = parse_imm(ops.rsplit(',').next()?.trim())?;
    let stored = port_from_imm(imm)?;
    // a single-byte store is a character/size/flag write, never a sockaddr port:
    // byte-swapped it manufactures exactly the multiple-of-256 "ports" ('G' → 18176)
    if stored < 0x100 {
        return None;
    }
    // a store whose both bytes are ASCII alphanumerics is a two-character string
    // fragment ("rs" → 29299, "nW" → 28247), not a network-order port
    let bytes = stored.to_le_bytes();
    if bytes[0].is_ascii_alphanumeric() && bytes[1].is_ascii_alphanumeric() {
        return None;
    }
    let port = stored.swap_bytes();
    // reject implausible results: any 16-bit store would otherwise look like a port
    (port >= 1024 || is_standard_port(port)).then_some(port)
}

/// Recover hardcoded ports from socket-setup code.
///
/// Two sub-passes: the argument of every `htons()`/`ntohs()` call (found via the
/// import table, and via any call whose operand names the helper — which covers a
/// statically-linked build), and — inside functions already carrying raw-socket
/// (T1095) hits — the folded `sin_port` store an optimizing compiler leaves behind.
/// A non-standard port is reported as **T1571**; a well-known one as T1095 context.
/// Bounded by [`MAX_PORT_HITS`], and recognized runtime/library functions are
/// skipped.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `socket_funcs` - Entries of functions carrying raw-socket (T1095) API/symbol
///   hits, which gates the (weaker) folded-store sub-pass — only sockaddr-building
///   code stores ports, while WinINet/WinHTTP callers build API-name strings on the
///   stack that read as byte-swapped "ports"
/// * `libraries` - The importing library of each import, from [`import_libraries`]
///
/// # Errors
///
/// Returns an error if disassembling the program or a bridge xref query fails.
#[tracing::instrument(name = "network_comms::port_hits", skip_all, err(Debug))]
async fn port_hits(
    ctx: &AnalysisContext,
    socket_funcs: &HashSet<u64>,
    libraries: &HashMap<String, String>,
) -> Result<Vec<DetectionHit>> {
    // the whole-program disassembly (cached), indexed by function entry for lookups
    let all = ctx.all_function_disassembly().await?;
    let by_entry: HashMap<u64, &Vec<Instruction>> = all.iter().map(|(e, i)| (*e, i)).collect();

    // ---- collect every byte-order-conversion call site --------------------
    let mut sites: Vec<(u64, u64)> = Vec::new();
    // a) call sites resolved through the import table (dynamically-linked binaries),
    // by name or by ordinal (htons is ws2_32 ordinal 9, ntohs ordinal 15)
    for import in &ctx.imports {
        let norm = ws2_32_ordinal(import, libraries).map_or_else(
            || normalize(&import.name).to_ascii_lowercase(),
            str::to_ascii_lowercase,
        );
        if !matches!(norm.as_str(), "htons" | "ntohs") {
            continue;
        }
        let resolved = ctx
            .resolve_call_sites(&import.address)
            .await
            .with_context(|| format!("resolving call sites of import '{}'", import.name))?;
        // keep only sites we can attribute to a function (we need its instructions)
        sites.extend(
            resolved
                .iter()
                .filter_map(|cs| cs.function.map(|f| (cs.site, f))),
        );
    }
    // b) calls whose operand names the helper (statically-linked / symbol-named thunks)
    for (entry, insns) in &all {
        for insn in insns {
            if !insn.mnemonic.eq_ignore_ascii_case("CALL") {
                continue;
            }
            let ops = crate::thorpal::detect::op_str(insn).to_ascii_lowercase();
            if !ops.contains("htons") && !ops.contains("ntohs") {
                continue;
            }
            if let Some(site) = parse_addr(&insn.address) {
                sites.push((site, *entry));
            }
        }
    }
    // sort + dedupe so the two sub-passes never double-report one call
    sites.sort_unstable();
    sites.dedup();

    // ---- recover the port at each site ------------------------------------
    let mut hits = Vec::new();
    // one flag per (function, port): a beacon loop may build the same address twice
    let mut seen: HashSet<(u64, u16)> = HashSet::new();
    for (site, entry) in sites {
        // stop once the report has enough recovered ports
        if hits.len() >= MAX_PORT_HITS {
            break;
        }
        // a runtime/library function's port constants are not the sample's C2
        if ctx.function_name(entry).is_some_and(is_library_function) {
            continue;
        }
        // locate the call within its function so we can look back at its argument
        let Some(insns) = by_entry.get(&entry) else {
            continue;
        };
        let Some(idx) = insns
            .iter()
            .position(|i| parse_addr(&i.address) == Some(site))
        else {
            continue;
        };
        // recover the argument from the instructions immediately before the call
        let Some(port) = port_arg(&insns[idx.saturating_sub(PORT_ARG_WINDOW)..idx]) else {
            continue;
        };
        if !seen.insert((entry, port)) {
            continue;
        }
        hits.push(port_hit(ctx, site, entry, port, false));
    }

    // ---- folded htons: a constant sin_port store in a raw-socket function --
    for (entry, insns) in &all {
        // stop once the report has enough recovered ports
        if hits.len() >= MAX_PORT_HITS {
            break;
        }
        // only trust a bare 16-bit store inside a function already carrying raw-socket
        // hits: sockaddr builders live there, while WinINet/WinHTTP callers only
        // manufacture folded "ports" out of their stack-built strings
        if !socket_funcs.contains(entry) {
            continue;
        }
        // a runtime/library function's constants are not the sample's C2
        if ctx.function_name(*entry).is_some_and(is_library_function) {
            continue;
        }
        for insn in insns {
            if hits.len() >= MAX_PORT_HITS {
                break;
            }
            let Some(port) = folded_port(insn) else {
                continue;
            };
            if !seen.insert((*entry, port)) {
                continue;
            }
            let Some(site) = parse_addr(&insn.address) else {
                continue;
            };
            hits.push(port_hit(ctx, site, *entry, port, true));
        }
    }
    Ok(hits)
}

/// Build one recovered-port hit.
///
/// A non-standard port is the interesting case: it is reported under **T1571**
/// (Non-Standard Port) at `medium`, while a well-known port is T1095 context at
/// `low`.
///
/// # Arguments
///
/// * `ctx` - The analysis context (used to name the containing function)
/// * `site` - The address the port was recovered at
/// * `entry` - The containing function's entry address
/// * `port` - The recovered port, in host byte order
/// * `folded` - Whether the port came from a folded `sin_port` store rather than a
///   `htons()` argument (a weaker, inferred recovery)
fn port_hit(ctx: &AnalysisContext, site: u64, entry: u64, port: u16, folded: bool) -> DetectionHit {
    // a well-known port is unremarkable; anything else is a deliberate choice
    let standard = is_standard_port(port);
    let (technique, severity) = if standard {
        (T1095, low())
    } else {
        (T1571, medium())
    };
    // name the containing function so the flag reads without cross-referencing
    let func = ctx
        .function_name(entry)
        .map_or_else(|| fmt_addr(entry), str::to_string);
    // how the value was recovered, which changes how much to trust it
    let how = if folded {
        "byte-swapped from a constant stored into sockaddr_in.sin_port (the compiler folded the \
         htons() call away)"
    } else {
        "recovered from the argument of the htons()/ntohs() call at this site"
    };
    let content = format!(
        "hardcoded port {port} in {func} @ {} ({})",
        fmt_addr(site),
        if standard {
            "well-known service port"
        } else {
            "non-standard port"
        }
    );
    let reasoning = if standard {
        format!(
            "Socket-setup code here builds an endpoint on port {port}, {how}. Port {port} is a \
             well-known service port, so the choice itself is unremarkable — but it pins down the \
             protocol this code speaks and, with the host recovered nearby, the full endpoint."
        )
    } else {
        format!(
            "Socket-setup code here builds an endpoint on port {port}, {how}. That is not a \
             well-known service port: communicating over a non-standard port is a deliberate choice \
             by the operator (it evades port-based filtering and blends into unmonitored traffic), \
             and the port number is one of the more durable indicators a family has — hunt for \
             outbound connections to port {port}."
        )
    };
    DetectionHit::new(technique, site, Some(entry), severity, content, reasoning)
        .kind(EvidenceKind::ContextOnly)
}

#[async_trait::async_trait]
impl Detection for NetworkComms {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "network_comms"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1071
    }

    /// Run the API, runtime-symbol, string-IOC, and port-recovery passes.
    #[tracing::instrument(name = "NetworkComms::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // collect hits from each sub-pass
        let mut hits = Vec::new();
        // 1) networking API call sites, one catalog per technique
        hits.extend(api_call_hits(ctx, T1071_001, WEB_APIS, EvidenceKind::ContextOnly).await?);
        hits.extend(api_call_hits(ctx, T1071_004, DNS_APIS, EvidenceKind::ContextOnly).await?);
        hits.extend(api_call_hits(ctx, T1095, SOCKET_APIS, EvidenceKind::ContextOnly).await?);
        hits.extend(api_call_hits(ctx, T1071, QUIC_APIS, EvidenceKind::ContextOnly).await?);
        hits.extend(api_call_hits(ctx, T1573, TLS_APIS, EvidenceKind::ContextOnly).await?);
        // 1b) the same Winsock APIs where they are imported by ordinal, which the
        // name-keyed catalogs above cannot see; the library map tells the ordinals apart
        let libraries = import_libraries(ctx).await;
        hits.extend(ordinal_hits(ctx, &libraries).await?);
        // 2) statically-linked Go/Rust networking package symbols (no import table)
        hits.extend(symbol_hits(ctx));
        // the functions already shown to do networking gate the bare-IP IOC pass and
        // the fabric-only pools; the raw-socket (T1095) subset gates the folded
        // sin_port sub-pass, since only sockaddr-building code stores ports
        let network_funcs: HashSet<u64> = hits.iter().filter_map(|hit| hit.function).collect();
        let socket_funcs: HashSet<u64> = hits
            .iter()
            .filter(|hit| hit.technique.id == T1095.id)
            .filter_map(|hit| hit.function)
            .collect();
        // 1c) collapse the low-severity setup/plumbing API hits into one capability
        // finding per function, so the wallpaper does not bury the traffic-moving calls
        let mut hits = collapse_plumbing(ctx, hits);
        // 3) network IOCs (URLs, endpoints, request fabric, IP literals) from strings.
        // The records are recovered once, uncapped: the per-indicator flags apply the
        // per-class caps, while the pooling pass needs the whole set to see structure.
        let records = ioc_records(ctx).await?;
        // the validity gates that fold junk-shaped records into the digest instead
        let gates = IocGates::new(
            &ctx.initialized_bytes().await?,
            &ctx.strings,
            &network_funcs,
        );
        hits.extend(ioc_string_hits(ctx, &records, &gates));
        // 3b) the pools those indicators fall into, and how the code selects from them
        hits.extend(super::ioc_pools::pool_hits(ctx, &records, &network_funcs).await?);
        // 4) hardcoded ports recovered from socket-setup code (non-standard → T1571)
        hits.extend(port_hits(ctx, &socket_funcs, &libraries).await?);
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render a severity as a comparable pair.
    ///
    /// [`Confidence`] does not implement `PartialEq`, so its `Debug` form stands in
    /// for the confidence half of the comparison.
    ///
    /// # Arguments
    ///
    /// * `severity` - The `(confidence, suspicion)` pair to render
    fn severity_key(severity: (Confidence, i64)) -> (String, i64) {
        // the Debug form is a stable, one-per-variant string
        (format!("{:?}", severity.0), severity.1)
    }

    /// `classify_ioc` prefers the most specific interpretation of a string.
    #[test]
    fn classify_ioc_prefers_the_richest_match() {
        // a URL wins over its embedded host
        assert!(matches!(
            classify_ioc("https://c2.example.com/panel"),
            Some(NetIoc::Url(_))
        ));
        // a bare endpoint is an endpoint, and its technique is the transport one
        let endpoint = classify_ioc("10.0.0.5:4444").expect("endpoint classifies");
        assert!(matches!(endpoint, NetIoc::Endpoint { .. }));
        assert_eq!(endpoint.technique().id, T1095.id);
        // a bare quad is a low-confidence IP literal
        let ip = classify_ioc("10.0.0.5").expect("IP classifies");
        assert_eq!(ip.kind(), "IP Address");
        assert_eq!(severity_key(ip.severity()), severity_key(low()));
        // short/irrelevant strings classify as nothing
        assert!(classify_ioc("abc").is_none());
        assert!(classify_ioc("C:\\Windows\\System32").is_none());
    }

    /// Each piece of the request fabric classifies, caps, and inventories separately.
    #[test]
    fn classify_ioc_types_the_request_fabric_per_class() {
        let agent =
            classify_ioc("Mozilla/5.0 (Windows NT 10.0; Win64) Gecko").expect("UA classifies");
        assert_eq!(agent.kind(), "User-Agent");
        assert_eq!(agent.artifact_class(), ArtifactClass::UserAgent);
        assert_eq!(severity_key(agent.severity()), severity_key(medium()));

        let path = classify_ioc("/api/v2/status").expect("URI path classifies");
        assert_eq!(path.kind(), "URI Path");
        assert_eq!(path.artifact_class(), ArtifactClass::UriPath);
        // a bare path is also how a filesystem path looks, so it stays at the floor
        assert_eq!(severity_key(path.severity()), severity_key(low()));

        let header = classify_ioc("X-Session-Id: %s").expect("header template classifies");
        assert_eq!(header.kind(), "HTTP Header Template");
        assert_eq!(header.artifact_class(), ArtifactClass::HttpHeader);

        // every request-fabric class shares one per-class cap, separate from the
        // address-shaped classes, so neither can starve the other
        assert_eq!(agent.cap(), MAX_HTTP_CLASS_HITS);
        assert_eq!(header.cap(), MAX_HTTP_CLASS_HITS);
        assert_eq!(
            classify_ioc("https://c2.example.com/panel")
                .expect("URL")
                .cap(),
            MAX_URL_HITS
        );
    }

    /// Classifying the request fabric first does not swallow endpoint literals.
    #[test]
    fn classify_ioc_keeps_headers_and_endpoints_apart() {
        // a header name is an undotted token, so an endpoint can never match one
        assert!(matches!(
            classify_ioc("c2.example.com:8443"),
            Some(NetIoc::Endpoint { .. })
        ));
        assert!(matches!(
            classify_ioc("10.0.0.5:4444"),
            Some(NetIoc::Endpoint { .. })
        ));
        // and a real header line still reads as a header
        assert!(matches!(
            classify_ioc("Host: c2.example.com"),
            Some(NetIoc::Http { .. })
        ));
    }

    /// The extension table is ordered longest first, which the seam rule relies on.
    #[test]
    fn web_extensions_are_ordered_longest_first() {
        let mut sorted = WEB_EXTENSIONS.to_vec();
        sorted.sort_by_key(|ext| std::cmp::Reverse(ext.len()));
        assert_eq!(WEB_EXTENSIONS, sorted.as_slice());
    }

    /// A run is cut where the next literal in the blob begins.
    #[test]
    fn carve_url_bounds_a_run_against_the_next_literal() {
        // a second URL is an exact marker of where the first one ended
        let blob = b"https://a.example.com/https://b.example.com/x";
        assert_eq!(carve_url(blob, 0), Some("https://a.example.com/"));
        assert_eq!(carve_url(blob, 22), Some("https://b.example.com/x"));
        // a resource extension followed by more text is a seam
        assert_eq!(
            carve_url(b"https://a.example.com/css/style.csshz8l2fnpvp71", 0),
            Some("https://a.example.com/css/style.css")
        );
        assert_eq!(
            carve_url(b"https://a.example.com/icon.pnginsufficient security", 0),
            Some("https://a.example.com/icon.png")
        );
        // so is a path segment carrying a colon
        assert_eq!(
            carve_url(b"https://a.example.com/idna: invalid label", 0),
            Some("https://a.example.com/")
        );
        // and the run stops on its own at whitespace or a NUL
        assert_eq!(
            carve_url(b"https://a.example.com/p\x00rest", 0),
            Some("https://a.example.com/p")
        );
    }

    /// The seam rules leave a URL that is genuinely longer alone.
    #[test]
    fn carve_url_keeps_a_complete_url_whole() {
        // an extension followed by a query or nothing is not a seam
        for url in [
            "https://a.example.com/style.css?v=2",
            "https://a.example.com/assets/index.php",
            "https://a.example.com:8443/a/b/c",
            "http://a.example.com/download/setup",
            // a colon outside the path is a port or ordinary query text
            "https://a.example.com:8443/p?t=12:30",
        ] {
            assert_eq!(carve_url(url.as_bytes(), 0), Some(url), "{url}");
        }
        // the longest extension at a dot wins, so `.aspx` is not cut to `.asp`
        assert_eq!(
            carve_url(b"https://a.example.com/p/login.aspxmalformed", 0),
            Some("https://a.example.com/p/login.aspx")
        );
    }

    /// A carve run terminates at `#`: a fragment never goes on the wire, so carving
    /// past one only merges a documentation link with the next blob literal.
    #[test]
    fn carve_url_stops_at_a_fragment() {
        // the getrandom doc link must not swallow the adjacent '/dev/urandom' literal
        assert_eq!(
            carve_url(
                b"https://docs.rs/getrandom#nodejs-es-module-support/dev/urandom",
                0
            ),
            Some("https://docs.rs/getrandom")
        );
        // a '#' straight after a complete path bounds the URL there too
        assert_eq!(
            carve_url(b"https://a.example.com/style.css#top", 0),
            Some("https://a.example.com/style.css")
        );
    }

    /// A scheme with no dotted host behind it is a placeholder, not an indicator.
    #[test]
    fn carve_url_rejects_an_undotted_host() {
        assert_eq!(carve_url(b"http://invalidlookup nil key", 0), None);
        assert_eq!(carve_url(b"https://localhost/health", 0), None);
        // and a bare scheme that ran straight into the next literal
        assert_eq!(carve_url(b"http://", 0), None);
    }

    /// The sweep reads data-like sections only, and finds URLs the strings missed.
    #[test]
    fn carve_urls_sweeps_the_data_sections() {
        let blob = b"oo largehttps://cdn.example.net/idna: invalid labelhttps://a.example.com/s.js";
        let sections = vec![
            (".rdata".to_string(), 0x1000, blob.to_vec()),
            (
                ".text".to_string(),
                0x2000,
                b"https://code.example.com/".to_vec(),
            ),
        ];
        let found = carve_urls(&sections);
        assert_eq!(
            found,
            vec![
                (0x1008, "https://cdn.example.net/".to_string()),
                (0x1033, "https://a.example.com/s.js".to_string()),
            ]
        );
    }

    /// Only a recognized library or runtime networking symbol counts as runtime code.
    #[test]
    fn is_runtime_function_spots_the_linked_stack() {
        assert!(is_runtime_function(Some("net/http.(*Client).Do")));
        assert!(is_runtime_function(Some("crypto/tls.(*Conn).Handshake")));
        // an unnamed or program-named function is the sample's until shown otherwise
        assert!(!is_runtime_function(Some("FUN_00401000")));
        assert!(!is_runtime_function(None));
    }

    /// The artifact role states what was observed and nothing more.
    #[test]
    fn ioc_role_states_only_what_was_observed() {
        let ioc = classify_ioc("https://c2.example.com/panel").expect("URL classifies");
        // referenced from code, in the program's own function
        let referenced = IocRecord {
            ioc: ioc.clone(),
            string_addr: 0x4a_1000,
            sites: vec![0x40_1000],
            function: Some(0x40_0f00),
        };
        assert_eq!(
            ioc_role(&referenced, false).as_deref(),
            Some("references this url")
        );
        // attributed only through the surrounding buffer — say so, do not imply a use
        let by_buffer = IocRecord {
            sites: Vec::new(),
            ..referenced.clone()
        };
        assert!(
            ioc_role(&by_buffer, false)
                .expect("buffer attribution has a role")
                .contains("no direct code reference")
        );
        // runtime code is named as runtime code
        assert!(
            ioc_role(&referenced, true)
                .expect("runtime attribution has a role")
                .contains("runtime/library code")
        );
        // no function, no role — the detail carries the explanation instead
        let orphan = IocRecord {
            function: None,
            ..referenced
        };
        assert!(ioc_role(&orphan, false).is_none());
    }

    /// A Go/Rust networking symbol is recognized, and lookalikes are not.
    #[test]
    fn symbol_marker_matches_packages_not_lookalikes() {
        assert_eq!(
            symbol_marker("net/http.(*Client).Do").map(|m| m.proto),
            Some(Proto::Web)
        );
        assert_eq!(
            symbol_marker("crypto/tls.(*Conn).Handshake").map(|m| m.proto),
            Some(Proto::Tls)
        );
        assert_eq!(
            symbol_marker("net.DialTCP").map(|m| m.proto),
            Some(Proto::Socket)
        );
        assert_eq!(
            symbol_marker("_ZN5hyper6client6legacy6client").map(|m| m.proto),
            Some(Proto::Web)
        );
        // "hypervisor" must not match the `hyper` crate (al-khaser is full of these)
        assert!(symbol_marker("is_running_under_hypervisor").is_none());
        assert!(symbol_marker("FUN_00401000").is_none());
    }

    /// Build an import symbol at a given address for the Winsock-ordinal tests.
    ///
    /// # Arguments
    ///
    /// * `name` - The symbol name (e.g. `"Ordinal_23"`)
    /// * `address` - The import's raw address, matching an [`import_libraries`] key
    fn import(name: &str, address: &str) -> Symbol {
        Symbol {
            name: name.to_string(),
            address: address.to_string(),
            symbol_type: "Function".to_string(),
            namespace: None,
            source: Some("IMPORTED".to_string()),
        }
    }

    /// Ordinal names map back to their Winsock exports, and only the real ones do.
    #[test]
    fn winsock_ordinal_maps_the_export_block() {
        assert_eq!(winsock_ordinal("Ordinal_23"), Some("socket"));
        assert_eq!(winsock_ordinal("Ordinal_4"), Some("connect"));
        assert_eq!(winsock_ordinal("Ordinal_9"), Some("htons"));
        assert_eq!(winsock_ordinal("Ordinal_115"), Some("WSAStartup"));
        // a named import and an unmapped ordinal are both left alone
        assert!(winsock_ordinal("getaddrinfo").is_none());
        assert!(winsock_ordinal("Ordinal_9999").is_none());
    }

    /// Only the Winsock providers' ordinals are worth resolving.
    #[test]
    fn is_winsock_library_matches_both_providers() {
        assert!(is_winsock_library("WS2_32.DLL"));
        assert!(is_winsock_library("ws2_32.dll"));
        assert!(is_winsock_library("WSOCK32.DLL"));
        assert!(!is_winsock_library("OLEAUT32.DLL"));
        assert!(!is_winsock_library("KERNEL32.DLL"));
    }

    /// An ordinal import resolves through the address→library map, not its name.
    #[test]
    fn ws2_32_ordinal_resolves_only_winsock() {
        // two libraries each exporting an ordinal 2, distinguished only by address
        let libraries: HashMap<String, String> = [
            ("EXTERNAL:00000002".to_string(), "ws2_32.dll".to_string()),
            ("EXTERNAL:00000008".to_string(), "oleaut32.dll".to_string()),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            ws2_32_ordinal(&import("Ordinal_2", "EXTERNAL:00000002"), &libraries),
            Some("bind")
        );
        // another library numbers its own exports from 1 — never map those
        assert!(ws2_32_ordinal(&import("Ordinal_2", "EXTERNAL:00000008"), &libraries).is_none());
        // an import missing from the map (no library reported) is left alone
        assert!(ws2_32_ordinal(&import("Ordinal_2", "EXTERNAL:00000010"), &libraries).is_none());
    }

    /// A resolved ordinal picks up the catalog entry of the equivalent named import.
    #[test]
    fn ordinal_spec_reuses_the_catalogs() {
        let (technique, spec) = ordinal_spec("connect").expect("connect is catalogued");
        assert_eq!(technique.id, T1095.id);
        assert_eq!(spec.key, "connect");
        let (technique, spec) = ordinal_spec("gethostbyname").expect("resolver is catalogued");
        assert_eq!(technique.id, T1071_004.id);
        assert_eq!(spec.key, "gethostbyname");
        // ordinals with no catalog entry (htonl, shutdown, …) are not flagged
        assert!(ordinal_spec("htonl").is_none());
    }

    /// Each protocol family maps to its ATT&CK technique.
    #[test]
    fn proto_maps_to_technique() {
        assert_eq!(Proto::Web.technique().id, T1071_001.id);
        assert_eq!(Proto::Dns.technique().id, T1071_004.id);
        assert_eq!(Proto::Socket.technique().id, T1095.id);
        assert_eq!(Proto::Quic.technique().id, T1071.id);
        assert_eq!(Proto::Tls.technique().id, T1573.id);
    }

    /// A recovered immediate only becomes a port when it fits the 16-bit range.
    #[test]
    fn port_from_imm_enforces_the_port_range() {
        // an immediate must fit in 16 bits and be non-zero
        assert_eq!(port_from_imm(8080), Some(8080));
        assert_eq!(port_from_imm(0), None);
        assert_eq!(port_from_imm(0x1_0000), None);
    }

    /// Build a minimal instruction for the port-recovery tests.
    ///
    /// # Arguments
    ///
    /// * `mnemonic` - The instruction mnemonic
    /// * `operands` - The operand text
    fn insn(mnemonic: &str, operands: &str) -> Instruction {
        Instruction {
            address: "401000".to_string(),
            mnemonic: mnemonic.to_string(),
            operands: vec![operands.to_string()],
            bytes: None,
            length: None,
            flow_type: None,
        }
    }

    /// `port_arg` recovers the `htons()` argument from either ABI's argument slot.
    #[test]
    fn port_arg_recovers_call_argument() {
        // System V: the first argument is in edi
        assert_eq!(port_arg(&[insn("MOV", "EDI,0x1f90")]), Some(8080));
        // Windows x64: the first argument is in ecx
        assert_eq!(port_arg(&[insn("MOV", "ECX, 0x115c")]), Some(4444));
        // 32-bit: the argument is pushed
        assert_eq!(port_arg(&[insn("PUSH", "0x50")]), Some(80));
        // the closest write wins over an earlier, unrelated one
        assert_eq!(
            port_arg(&[insn("MOV", "EDI,0x50"), insn("MOV", "EDI,0x1f90")]),
            Some(8080)
        );
        // nothing recoverable: a register-to-register move
        assert_eq!(port_arg(&[insn("MOV", "EDI,EAX")]), None);
        assert_eq!(port_arg(&[]), None);
    }

    /// `folded_port` byte-swaps a `sin_port` store and rejects implausible values.
    #[test]
    fn folded_port_swaps_network_order() {
        // 0x5c11 stored little-endian is 0x115c big-endian == port 4444
        assert_eq!(
            folded_port(&insn("MOV", "word ptr [RBP + -0x1e],0x5c11")),
            Some(4444)
        );
        // 0x5000 -> 0x0050 == port 80 (a well-known port is still plausible)
        assert_eq!(folded_port(&insn("MOV", "word ptr [RAX],0x5000")), Some(80));
        // a swap yielding a low, non-standard port is rejected as noise
        assert_eq!(folded_port(&insn("MOV", "word ptr [RAX],0x0200")), None);
        // not a 16-bit immediate store into memory
        assert_eq!(folded_port(&insn("MOV", "dword ptr [RAX],0x5c11")), None);
        assert_eq!(folded_port(&insn("MOV", "EAX,0x5c11")), None);
    }

    /// The IOC content block carries the literal, the parsed fields, and its sites.
    #[test]
    fn ioc_content_renders_extracted_fields() {
        let ioc = classify_ioc("https://evil.example.com:8443/gate.php").expect("URL classifies");
        let content = ioc_content(&ioc, 0x4a_1000, &[0x40_1234]);
        assert!(content.contains("### Network IOC — URL"));
        assert!(content.contains("https://evil.example.com:8443/gate.php"));
        assert!(content.contains("- **Host:** evil.example.com"));
        assert!(content.contains("- **Port:** 8443 (explicit)"));
        assert!(content.contains("- **Path:** /gate.php"));
        assert!(content.contains("- **String address:** 0x4a1000"));
        assert!(content.contains("- **Referenced from:** 0x401234"));
    }

    /// Character-store immediates are rejected by the folded-port recovery.
    #[test]
    fn folded_port_rejects_character_store_artifacts() {
        // single-byte stores: the wide chars 'G'/'E'/'T' of a stack-built L"GET"
        // (previously misread as ports 18176/17664/21504), the 'd' of "udevd" (25600),
        // the '1' of " 2>&1" (12544), framing sizes 20/92 (5120/23552), and
        // STARTUPINFO.wShowWindow = 5 (1280)
        for imm in [
            "0x47", "0x45", "0x54", "0x64", "0x31", "0x14", "0x5c", "0x5",
        ] {
            assert_eq!(
                folded_port(&insn("MOV", &format!("word ptr [EBP + -0x18],{imm}"))),
                None,
                "{imm}"
            );
        }
        // two-letter string fragments: the "rs" tail of "LogonHours" (29299) and the
        // "nW"/"oW"/"le" API-name fragments (28247/28503/27749)
        for imm in ["0x7372", "0x576e", "0x576f", "0x656c"] {
            assert_eq!(
                folded_port(&insn("MOV", &format!("word ptr [EBP + -0x18],{imm}"))),
                None,
                "{imm}"
            );
        }
    }

    /// Real folded `sin_port` stores still recover through the character-store rules.
    #[test]
    fn folded_port_keeps_real_folded_ports() {
        // htons(443) folds to 0xbb01 and htons(8080) to 0x901f — neither is a
        // single-byte store, and neither byte pair is alphanumeric-only
        assert_eq!(
            folded_port(&insn("MOV", "word ptr [RAX],0xbb01")),
            Some(443)
        );
        assert_eq!(
            folded_port(&insn("MOV", "word ptr [RAX],0x901f")),
            Some(8080)
        );
    }

    /// Build an IOC record for the gate tests.
    ///
    /// # Arguments
    ///
    /// * `value` - The string the record was recovered from
    /// * `string_addr` - Where the string is stored
    /// * `sites` - The code sites referencing it
    fn ioc_record(value: &str, string_addr: u64, sites: Vec<u64>) -> IocRecord {
        IocRecord {
            ioc: classify_ioc(value).expect("value classifies"),
            string_addr,
            sites,
            function: None,
        }
    }

    /// Build a defined string at an address for the gate tests.
    ///
    /// # Arguments
    ///
    /// * `value` - The string's decoded value
    /// * `address` - The string's address, as hex text
    fn defined_string(value: &str, address: &str) -> StringData {
        StringData {
            address: address.to_string(),
            value: value.to_string(),
            length: value.len(),
            encoding: None,
            references: Vec::new(),
        }
    }

    /// Fragment URLs fold into the digest; the killswitch-shaped URL does not.
    #[test]
    fn ioc_gates_fold_fragment_urls_only() {
        let network_funcs: HashSet<u64> = HashSet::new();
        let gates = IocGates::new(&[], &[], &network_funcs);
        // a documentation link carrying a fragment is digest material
        let doc = ioc_record(
            "https://docs.rs/getrandom#nodejs-es-module-support",
            0x1e_6000,
            Vec::new(),
        );
        assert!(gates.reject(&doc, |_| None).is_some());
        // the WannaCry-killswitch URL shape (no fragment) still seeds a finding
        let killswitch = ioc_record(
            "http://www.iuqerfsodp9ifjaposdfjhgosurijfaewrwergwea.com",
            0x40_c000,
            vec![0x40_1000],
        );
        assert!(gates.reject(&killswitch, |_| Some(0x40_0f00)).is_none());
    }

    /// Bare IPs need a code reference from networking-capable code to seed.
    #[test]
    fn ioc_gates_require_networking_code_use_for_bare_ips() {
        let network_funcs: HashSet<u64> = [0x40_3b8b].into_iter().collect();
        let gates = IocGates::new(&[], &[], &network_funcs);
        // the VERSIONINFO FileVersion value '1.0.0.0' has no code reference at all
        let version = ioc_record("1.0.0.0", 0x40_4200, Vec::new());
        assert!(gates.reject(&version, |_| None).is_some());
        // a quad referenced only from a non-networking function folds as well
        let unrelated = ioc_record("3.51.124.11", 0x44_01b0, vec![0x40_2000]);
        assert!(gates.reject(&unrelated, |_| Some(0x40_1f00)).is_some());
        // '127.0.0.1' referenced from inside the function carrying a WSAStartup hit
        // (the confirmed pattern) keeps seeding
        let loopback = ioc_record("127.0.0.1", 0x40_8000, vec![0x40_3c56]);
        assert!(
            gates
                .reject(&loopback, |site| (site == 0x40_3c56).then_some(0x40_3b8b))
                .is_none()
        );
    }

    /// IPs stored in resource data or beside VERSIONINFO keys are version metadata.
    #[test]
    fn ioc_gates_skip_resource_section_and_versioninfo_ips() {
        let network_funcs: HashSet<u64> = [0x40_1000].into_iter().collect();
        // a .rsrc section holding the first quad, and a FileVersion key by the second
        let sections = vec![(".rsrc".to_string(), 0x40_4000, vec![0u8; 0x800])];
        let strings = vec![defined_string("FileVersion", "440190")];
        let gates = IocGates::new(&sections, &strings, &network_funcs);
        // '1.0.0.0' at 0x404200 sits inside .rsrc, wherever it is referenced from
        let in_rsrc = ioc_record("1.0.0.0", 0x40_4200, vec![0x40_5000]);
        assert!(gates.reject(&in_rsrc, |_| Some(0x40_1000)).is_some());
        // '3.51.124.11' at 0x4401b0 sits beside the FileVersion key at 0x440190
        let beside_key = ioc_record("3.51.124.11", 0x44_01b0, vec![0x40_5000]);
        assert!(gates.reject(&beside_key, |_| Some(0x40_1000)).is_some());
    }

    /// The digest lists every folded value verbatim with its address and reason.
    #[test]
    fn digest_hit_lists_folded_values_verbatim() {
        let doc = ioc_record(
            "https://docs.rs/getrandom#nodejs-es-module-support",
            0x1e_6000,
            Vec::new(),
        );
        let version = ioc_record("1.0.0.0", 0x40_4200, Vec::new());
        let entries = vec![
            (&doc, "carries a `#` fragment"),
            (
                &version,
                "not referenced from any function that does networking",
            ),
        ];
        let hit = digest_hit(&entries);
        // one file-scoped finding with no pivot artifact, at the low grade
        assert!(hit.function.is_none());
        assert!(hit.artifact.is_none());
        assert_eq!(
            severity_key((hit.confidence, hit.suspicion)),
            severity_key(low())
        );
        // every value appears verbatim with its address and the reason it folded
        assert!(
            hit.content
                .contains("https://docs.rs/getrandom#nodejs-es-module-support")
        );
        assert!(hit.content.contains("1.0.0.0"));
        assert!(hit.content.contains("0x404200"));
        assert!(hit.content.contains("carries a `#` fragment"));
        assert!(
            hit.name
                .as_deref()
                .is_some_and(|name| name.contains("2 unseeded"))
        );
    }

    /// The digest lists addresses ahead of request fabric, so the line cap never
    /// drops them.
    ///
    /// The confirmed C2 IP `220.181.87.80` folded past position 40 in insertion
    /// order and fell off the digest behind header fabric. Entries are ordered by
    /// class priority — IPs/URLs/endpoints first, URI paths next, fabric last —
    /// before the cap, with insertion order breaking ties, so the IP is always
    /// listed and only fabric overflows into the "and N more" tail.
    #[test]
    fn digest_lists_addresses_ahead_of_fabric_past_the_line_cap() {
        // fifty header-fabric records, then the confirmed C2 IP dead last
        let mut records: Vec<IocRecord> = (0u64..50)
            .map(|i| ioc_record(&format!("X-Pad-{i}: v{i}"), 0x40_0000 + i, Vec::new()))
            .collect();
        records.push(ioc_record("220.181.87.80", 0x41_2000, Vec::new()));
        let entries: Vec<(&IocRecord, &'static str)> = records
            .iter()
            .map(|record| (record, "its class's per-sample flag cap was reached"))
            .collect();
        let hit = digest_hit(&entries);
        // the IP survives the 40-line cap despite arriving past it
        assert!(hit.content.contains("220.181.87.80"), "{}", hit.content);
        // it lists ahead of every fabric line
        let ip_at = hit.content.find("220.181.87.80").expect("ip listed");
        let fabric_at = hit.content.find("X-Pad-0").expect("fabric listed");
        assert!(ip_at < fabric_at, "{}", hit.content);
        // ties keep insertion order (the sort is stable)
        let pad0 = hit.content.find("X-Pad-0:").expect("pad 0");
        let pad1 = hit.content.find("X-Pad-1:").expect("pad 1");
        assert!(pad0 < pad1);
        // the overflow is still counted honestly: 51 entries, 40 lines
        assert!(hit.content.contains("and 11 more"), "{}", hit.content);
    }

    /// The pivot-protected endpoint artifact needs an xref or a medium grade.
    #[test]
    fn endpoint_artifact_needs_an_xref_or_a_medium_grade() {
        // an unreferenced low-grade endpoint value keeps content only, no artifact
        assert!(!endpoint_artifact_justified(
            ArtifactClass::NetworkEndpoint,
            &[],
            low()
        ));
        // a referenced endpoint (the killswitch URL shape) keeps its artifact
        assert!(endpoint_artifact_justified(
            ArtifactClass::NetworkEndpoint,
            &[0x40_1000],
            low()
        ));
        // a medium-grade endpoint carries its own weight even unreferenced
        assert!(endpoint_artifact_justified(
            ArtifactClass::NetworkEndpoint,
            &[],
            medium()
        ));
        // fabric classes are never pivot-protected, so they are never gated
        assert!(endpoint_artifact_justified(
            ArtifactClass::HttpHeader,
            &[],
            low()
        ));
    }

    /// Build a low-severity API call hit for the plumbing-collapse tests.
    ///
    /// # Arguments
    ///
    /// * `api` - The API name leading the content
    /// * `technique` - The technique the hit evidences
    /// * `function` - The calling function's entry, when known
    /// * `site` - The call site address
    fn api_hit(
        api: &str,
        technique: AttackTechnique,
        function: Option<u64>,
        site: u64,
    ) -> DetectionHit {
        DetectionHit::new(
            technique,
            site,
            function,
            low(),
            format!("{api} call @ {}", fmt_addr(site)),
            "test reasoning".to_string(),
        )
        .kind(EvidenceKind::ContextOnly)
    }

    /// Only low-grade setup calls inside a known function are collapsible.
    #[test]
    fn plumbing_key_admits_only_low_setup_call_hits() {
        // a plumbing call attributed to a function collapses
        assert_eq!(
            plumbing_key(&api_hit("WSAStartup", T1095, Some(0x40_1000), 0x40_1010)),
            Some("WSAStartup")
        );
        // a traffic-moving (medium) call keeps its own finding
        let mut connect = api_hit("connect", T1095, Some(0x40_1000), 0x40_1020);
        connect.confidence = medium().0;
        connect.suspicion = medium().1;
        assert_eq!(plumbing_key(&connect), None);
        // an "imported but never called" hit has no function and stays as-is
        assert_eq!(plumbing_key(&api_hit("socket", T1095, None, 0)), None);
        // a non-plumbing API never collapses, whatever its grade
        assert_eq!(
            plumbing_key(&api_hit(
                "getnameinfo",
                T1071_004,
                Some(0x40_1000),
                0x40_1030
            )),
            None
        );
    }

    /// A function's plumbing hits merge into one capability finding that keeps every
    /// API name and call site.
    #[test]
    fn plumbing_hits_merge_into_one_capability_finding() {
        // the confirmed cheap anchors: WSAStartup + socket + setsockopt in one function
        let members = vec![
            (
                "WSAStartup",
                api_hit("WSAStartup", T1095, Some(0x40_1000), 0x40_1010),
            ),
            (
                "socket",
                api_hit("socket", T1095, Some(0x40_1000), 0x40_1020),
            ),
            (
                "setsockopt",
                api_hit("setsockopt", T1095, Some(0x40_1000), 0x40_1030),
            ),
        ];
        let hit = plumbing_hit(Some("reverse_shell"), 0x40_1000, &members);
        // one low-grade finding, anchored at the earliest site in the function
        assert_eq!(hit.function, Some(0x40_1000));
        assert_eq!(hit.address, 0x40_1010);
        assert_eq!(
            severity_key((hit.confidence, hit.suspicion)),
            severity_key(low())
        );
        // the anchors survive the merge: every API name, every site, the function
        for text in [
            "WSAStartup",
            "socket",
            "setsockopt",
            "0x401010",
            "0x401020",
            "0x401030",
            "reverse_shell",
        ] {
            assert!(hit.content.contains(text), "{text}");
        }
        // agreeing members keep their technique, and the reasoning stays capability-level
        assert_eq!(hit.technique.id, T1095.id);
        assert!(!hit.reasoning.contains("C2"));
    }

    /// Mixed-catalog plumbing falls back to the T1071 family parent.
    #[test]
    fn plumbing_hit_mixes_techniques_to_the_parent() {
        let members = vec![
            (
                "WSAStartup",
                api_hit("WSAStartup", T1095, Some(0x40_1000), 0x40_1010),
            ),
            (
                "InternetOpen",
                api_hit("InternetOpen", T1071_001, Some(0x40_1000), 0x40_1020),
            ),
        ];
        let hit = plumbing_hit(None, 0x40_1000, &members);
        assert_eq!(hit.technique.id, T1071.id);
    }
}
