//! Shared indicator classifiers: turning a raw literal into a typed value.
//!
//! These are the value-level primitives — "is this text a URL / an endpoint / an
//! IP / an HTTP artifact / a MAC address / a GUID?" — with no ATT&CK or scoring
//! coupling. They were factored out of the network detection so that every
//! consumer can share one implementation: the network detection classifies
//! defined strings, the decode engine classifies *recovered* buffers, and the
//! host-binding detection classifies machine-identity literals.
//!
//! # The blob problem
//!
//! Every classifier here that matches on *shape alone* must be guarded by
//! [`word_bounded`]. Statically linked runtimes (and any toolchain that
//! deduplicates string constants) concatenate their literals into one
//! unseparated blob, so a shape-only pattern happily slices across two unrelated
//! strings: a run of numeric constants yields dotted quads and dash-form MAC
//! addresses that were never addresses at all. Requiring a non-token character
//! on both sides of a match is what keeps that noise out.

use std::net::{Ipv4Addr, Ipv6Addr};

/// The URL schemes worth classifying (the ones that actually carry traffic).
pub const SCHEMES: &[&str] = &["http", "https", "ws", "wss", "ftp", "ftps"];

/// Well-known service ports: a connection to one of these is unremarkable.
///
/// Everything else counts as non-standard — a deliberate choice by whoever built
/// the program, and one of the more durable indicators it has.
pub const STANDARD_PORTS: &[u16] = &[
    20, 21, 22, 23, 25, 53, 67, 68, 69, 80, 110, 119, 123, 135, 137, 138, 139, 143, 161, 162, 389,
    443, 445, 465, 514, 587, 636, 993, 995, 1433, 1521, 3306, 3389, 5432, 5900, 8080, 8443,
];

/// Whether a port is a well-known service port.
///
/// # Arguments
///
/// * `port` - The recovered port number
pub fn is_standard_port(port: u16) -> bool {
    // a simple membership test over the curated list above
    STANDARD_PORTS.contains(&port)
}

/// Whether a match at `at..at + len` in `hay` is delimited on both sides.
///
/// A "token" character here is anything a shape-only pattern could have run
/// through — alphanumerics plus the separators those patterns use (`.`, `:`,
/// `-`, `_`). A match is word-bounded when neither neighbour is one, i.e. the
/// match is not a slice out of a longer run of the same character class.
///
/// This is the guard that keeps a runtime's concatenated string blob from
/// manufacturing indicators: a numeric-constant run like `…01-02-03-04-05-06-08…`
/// contains a perfectly-shaped dash-form MAC address that is not one.
///
/// # Arguments
///
/// * `hay` - The full text the match was found in
/// * `at` - The byte offset the match starts at
/// * `len` - The byte length of the match
pub fn word_bounded(hay: &str, at: usize, len: usize) -> bool {
    /// Whether a byte could be part of a shape-only indicator token.
    fn is_token(b: u8) -> bool {
        // the character classes every shape pattern in this module runs through
        b.is_ascii_alphanumeric() || matches!(b, b'.' | b':' | b'-' | b'_')
    }
    let bytes = hay.as_bytes();
    // reject an out-of-range span rather than panicking on a bad caller
    let Some(end) = at.checked_add(len).filter(|end| *end <= bytes.len()) else {
        return false;
    };
    // the character before the match must not continue the token
    if at > 0 && is_token(bytes[at - 1]) {
        return false;
    }
    // ...and neither must the character after it
    if end < bytes.len() && is_token(bytes[end]) {
        return false;
    }
    true
}

/// The default port for a URL scheme, when it has one.
///
/// # Arguments
///
/// * `scheme` - The lower-case URL scheme
pub fn default_port(scheme: &str) -> Option<u16> {
    // the IANA-registered default for each scheme we classify
    match scheme {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        "ftp" => Some(21),
        "ftps" => Some(990),
        _ => None,
    }
}

/// A URL recovered from a literal, split into its pivot-worthy parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlIoc {
    /// The URL exactly as it appears in the binary (trimmed to the URL itself).
    pub raw: String,
    /// The lower-case URL scheme (e.g. `https`).
    pub scheme: String,
    /// The lower-case host (domain or IP literal; no brackets for IPv6).
    pub host: String,
    /// The explicit port, when the URL carries one.
    pub port: Option<u16>,
    /// The path (plus query/fragment), defaulting to `/`.
    pub path: String,
}

impl UrlIoc {
    /// The port the URL actually talks to: the explicit one, else the scheme default.
    pub fn effective_port(&self) -> Option<u16> {
        // an explicit port always wins; otherwise fall back to the scheme's default
        self.port.or_else(|| default_port(&self.scheme))
    }

    /// Whether the host is a Tor hidden service (`.onion`).
    pub fn is_onion(&self) -> bool {
        // a .onion host only resolves inside Tor — a deliberate anonymity choice
        self.host.ends_with(".onion")
    }

    /// Whether the URL addresses a section of a document meant for a person to read.
    ///
    /// A fragment is resolved by the client and is never put on the wire (RFC 3986
    /// §3.5), so pointing one at a server gains a program nothing — the only reason to
    /// carry a URL with a fragment is that someone is expected to open it in a browser.
    /// Library documentation links and error messages that cite a manual page are the
    /// usual sources, and both arrive in quantity in a statically linked image.
    pub fn has_fragment(&self) -> bool {
        // the fragment is kept on the path, which is where the `#` survives
        self.path.contains('#')
    }
}

/// Whether a host is syntactically usable as a URL host.
///
/// Deliberately lenient (a URL's host may be a bare name like `intranet`), just
/// strict enough to reject punctuation soup that happened to follow a `://`.
///
/// # Arguments
///
/// * `host` - The candidate host text
pub fn valid_url_host(host: &str) -> bool {
    // a DNS name is at most 253 characters, and only these characters are legal
    !host.is_empty()
        && host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Whether a host is a plausible *bare* endpoint host (for a `host:port` literal).
///
/// Stricter than [`valid_url_host`]: with no scheme to anchor on, only an IPv4
/// literal or a dotted DNS name with an alphabetic TLD is accepted, so text like
/// `error:42` or `Content-Length:0` is not mistaken for an endpoint.
///
/// # Arguments
///
/// * `host` - The candidate host text
pub fn plausible_endpoint_host(host: &str) -> bool {
    // an IP literal is unambiguous
    if parse_ipv4_literal(host).is_some() {
        return true;
    }
    // otherwise require a dotted name whose labels are legal and whose TLD is alphabetic
    if !valid_url_host(host) || !host.contains('.') {
        return false;
    }
    let labels: Vec<&str> = host.split('.').collect();
    // every label must be non-empty and free of leading/trailing hyphens
    if labels
        .iter()
        .any(|l| l.is_empty() || l.starts_with('-') || l.ends_with('-'))
    {
        return false;
    }
    // the TLD must look like a TLD (>= 2 letters), which rules out "1.2:80"-style noise
    labels
        .last()
        .is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()))
}

/// Split a URL authority into its host and explicit port.
///
/// Handles the bracketed IPv6 form (`[2001:db8::1]:8443`) as well as the ordinary
/// `host[:port]` form.
///
/// # Arguments
///
/// * `authority` - The authority component (userinfo already stripped)
pub fn split_host_port(authority: &str) -> Option<(String, Option<u16>)> {
    // bracketed IPv6 literal: the brackets delimit the host, a trailing :N is the port
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(text) => Some(parse_port(text)?),
            None => None,
        };
        return host
            .parse::<Ipv6Addr>()
            .ok()
            .map(|ip| (ip.to_string(), port));
    }
    // otherwise a trailing :digits is the port and everything before it is the host
    match authority.rsplit_once(':') {
        Some((host, text)) if !text.is_empty() && text.chars().all(|c| c.is_ascii_digit()) => {
            let port = parse_port(text)?;
            valid_url_host(host).then(|| (host.to_ascii_lowercase(), Some(port)))
        }
        _ => valid_url_host(authority).then(|| (authority.to_ascii_lowercase(), None)),
    }
}

/// Parse a port number, rejecting 0 and anything outside the 16-bit range.
///
/// # Arguments
///
/// * `text` - The decimal port text
pub fn parse_port(text: &str) -> Option<u16> {
    // port 0 is a wildcard, never a real endpoint
    text.parse::<u16>().ok().filter(|port| *port > 0)
}

/// Parse a URL out of a literal, splitting it into its parts.
///
/// The URL need not be the whole string (a request template like
/// `GET http://host/path HTTP/1.1` is common), so the scheme is located by its
/// `://` separator and the URL runs to the first whitespace/quote delimiter.
///
/// # Arguments
///
/// * `value` - The literal's value
pub fn parse_url(value: &str) -> Option<UrlIoc> {
    // locate the scheme separator; the scheme is the run of legal characters before it
    let sep = value.find("://")?;
    let head = &value[..sep];
    let start = head
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        .last()
        .map_or(head.len(), |(idx, _)| idx);
    let scheme = head[start..].to_ascii_lowercase();
    // only classify schemes that actually carry traffic
    if !SCHEMES.contains(&scheme.as_str()) {
        return None;
    }
    // the rest of the URL runs to the first delimiter (whitespace, quote, or bracket)
    let rest: String = value[sep + 3..]
        .chars()
        .take_while(|c| {
            !c.is_whitespace() && !matches!(c, '"' | '\'' | '<' | '>' | '`' | '\\' | '|')
        })
        .collect();
    if rest.is_empty() {
        return None;
    }
    // split the authority (host[:port]) from the path/query/fragment
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let path = if auth_end < rest.len() {
        rest[auth_end..].to_string()
    } else {
        "/".to_string()
    };
    // drop any userinfo (`user:pass@`) prefix before parsing the host
    let authority = rest[..auth_end].rsplit('@').next().unwrap_or_default();
    let (host, port) = split_host_port(authority)?;
    Some(UrlIoc {
        raw: format!("{scheme}://{rest}"),
        scheme,
        host,
        port,
        path,
    })
}

/// Parse a bare `host:port` / `ip:port` endpoint literal.
///
/// Requires the *whole* string to be the endpoint (no surrounding text) and the host
/// to be an IPv4 literal or a dotted DNS name, so ordinary `key:value` text is not
/// mistaken for an address.
///
/// # Arguments
///
/// * `value` - The literal's value
pub fn parse_endpoint(value: &str) -> Option<(String, u16)> {
    let trimmed = value.trim();
    // an endpoint literal is a single token, and never a URL (handled by `parse_url`)
    if trimmed.is_empty() || trimmed.contains(char::is_whitespace) || trimmed.contains("://") {
        return None;
    }
    // the trailing :digits is the port; everything before it is the host
    let (host, text) = trimmed.rsplit_once(':')?;
    let port = parse_port(text)?;
    // reject anything whose host half is not a plausible address/name
    plausible_endpoint_host(host).then(|| (host.to_ascii_lowercase(), port))
}

/// Parse a bare IPv4 literal, rejecting the shapes that are usually version numbers.
///
/// Requires the whole (trimmed) string to be a dotted quad with no leading zeros,
/// and rejects `0.x.y.z` and the broadcast address. A quad like `1.2.3.4` is still
/// ambiguous with a version number — which is why IP hits are reported weakly.
///
/// # Arguments
///
/// * `value` - The candidate address text
pub fn parse_ipv4_literal(value: &str) -> Option<Ipv4Addr> {
    let trimmed = value.trim();
    // exactly four dotted parts, or it is not an address
    let parts: Vec<&str> = trimmed.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut octets = [0u8; 4];
    for (slot, part) in octets.iter_mut().zip(parts) {
        // each octet is 1-3 digits with no leading zero ("01.2.3.4" is not an address)
        if part.is_empty() || part.len() > 3 || !part.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        if part.len() > 1 && part.starts_with('0') {
            return None;
        }
        *slot = part.parse::<u8>().ok()?;
    }
    let addr = Ipv4Addr::from(octets);
    // 0.x.y.z is not a routable host address (and reads like a version); so is broadcast
    if octets[0] == 0 || addr.is_broadcast() {
        return None;
    }
    Some(addr)
}

/// Parse a bare IPv6 literal, rejecting the unspecified address.
///
/// # Arguments
///
/// * `value` - The candidate address text
pub fn parse_ipv6_literal(value: &str) -> Option<Ipv6Addr> {
    let trimmed = value.trim();
    // an IPv6 literal always has at least two colons, which also rules out "12:34"
    if trimmed.matches(':').count() < 2 {
        return None;
    }
    let addr = trimmed.parse::<Ipv6Addr>().ok()?;
    // "::" carries no endpoint information
    (!addr.is_unspecified()).then_some(addr)
}

// ===========================================================================
// HTTP request fabric
// ===========================================================================

/// The HTTP request methods defined by RFC 9110 and RFC 5789.
///
/// A platform fact, not an indicator: the list changes when an RFC ships.
const HTTP_METHODS: &[&str] = &[
    "GET", "POST", "PUT", "DELETE", "HEAD", "OPTIONS", "PATCH", "CONNECT", "TRACE",
];

/// The HTTP version tokens a request line or hand-rolled template carries,
/// lower-cased for matching.
const HTTP_VERSION_TOKENS: &[&str] = &["http/1.0", "http/1.1", "http/2"];

/// The standard HTTP header names, lower-cased (RFC 9110 and its companions).
///
/// Used to tell a hand-built request header from a colon that happens to appear in
/// prose. A header the program invented (`X-Session-Id:`) is recognized by the `x-`
/// convention instead, so this list does not need to grow per sample.
const STANDARD_HEADERS: &[&str] = &[
    "accept",
    "accept-charset",
    "accept-encoding",
    "accept-language",
    "authorization",
    "cache-control",
    "connection",
    "content-disposition",
    "content-encoding",
    "content-length",
    "content-type",
    "cookie",
    "date",
    "expect",
    "from",
    "host",
    "if-modified-since",
    "if-none-match",
    "origin",
    "pragma",
    "proxy-authorization",
    "range",
    "referer",
    "set-cookie",
    "te",
    "transfer-encoding",
    "upgrade",
    "user-agent",
    "via",
    "x-requested-with",
];

/// The IANA top-level media types, for recognizing a `type/subtype` content type.
const MEDIA_TYPES: &[&str] = &[
    "application/",
    "audio/",
    "font/",
    "image/",
    "message/",
    "model/",
    "multipart/",
    "text/",
    "video/",
];

/// Filesystem path prefixes that a leading-slash literal is far more likely to be
/// than a URI path — a platform fact about Unix and macOS layout, not an indicator.
const FILESYSTEM_PREFIXES: &[&str] = &[
    "/applications/",
    "/bin/",
    "/boot/",
    "/dev/",
    "/etc/",
    "/home/",
    "/lib/",
    "/lib64/",
    "/library/",
    "/media/",
    "/mnt/",
    "/opt/",
    "/proc/",
    "/root/",
    "/run/",
    "/sbin/",
    "/srv/",
    "/sys/",
    "/system/",
    "/tmp/",
    "/usr/",
    "/var/",
    "/volumes/",
];

/// File extensions that name a compilation input rather than a request target.
///
/// Compilers of every language embed the absolute source path of a statement into the
/// binary so that an assertion, panic, or trace can say where it came from, and in a
/// statically linked image those paths arrive by the hundred. They are leading-slash,
/// segmented, and made entirely of path characters, so nothing about their *shape*
/// separates them from a URI — only the fact that no server is asked for a translation
/// unit. A build-path extension is therefore disqualifying on its own.
const SOURCE_FILE_EXTS: &[&str] = &[
    ".rs", ".c", ".cc", ".cxx", ".cpp", ".h", ".hh", ".hpp", ".hxx", ".inl", ".m", ".mm", ".swift",
    ".kt", ".scala", ".zig", ".rlib", ".o", ".obj", ".pdb", ".s", ".asm",
];

/// Path fragments a toolchain writes when it records where code was built from.
///
/// These carry the cases the extension test cannot reach: a directory-only path with no
/// file at the end, and — the reason this exists at all — a source path that the linker
/// concatenated into the next literal in the blob, leaving the extension buried mid-word
/// where no boundary test can find it. Like the extension list this describes build
/// tooling, so it changes when a toolchain ships, not when a sample appears.
const TOOLCHAIN_PATH_MARKERS: &[&str] = &[
    "/rustc/",
    "/rust/deps/",
    "/cargo/registry/",
    "/go/pkg/mod/",
    "/goroot/",
    "/usr/src/",
    "/usr/include/",
    "/usr/local/include/",
    "/checkout/",
    "/.conan/",
    "/vcpkg/",
];

/// A piece of the HTTP request fabric a program builds its traffic out of.
///
/// These are the literals that survive into every request the program makes, so they
/// fingerprint it in proxy and IDS logs even when the endpoint rotates. They are also
/// exactly what a statically linked HTTP stack ships by the dozen, which is why the
/// caller must suppress or weaken them when the referencing function is runtime code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HttpArtifact {
    /// A User-Agent the program presents.
    UserAgent,
    /// A hand-built request header line.
    HeaderName,
    /// A request header carrying a format specifier or placeholder.
    HeaderTemplate,
    /// A cookie header or a `k=v;` cookie template.
    CookieName,
    /// A URI path used against an endpoint.
    UriPath,
    /// A URI path carrying a format specifier or placeholder.
    UriTemplate,
    /// A media type the program sends or accepts.
    ContentType,
    /// A whole request line or hand-rolled request/response template.
    RequestTemplate,
}

impl HttpArtifact {
    /// The label this artifact is reported under.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::UserAgent => "User-Agent",
            Self::HeaderName => "HTTP Header",
            Self::HeaderTemplate => "HTTP Header Template",
            Self::CookieName => "Cookie",
            Self::UriPath => "URI Path",
            Self::UriTemplate => "URI Template",
            Self::ContentType => "Content Type",
            Self::RequestTemplate => "HTTP Request Template",
        }
    }

    /// Whether the literal carries a format specifier or placeholder, i.e. the program
    /// fills it in at request time rather than sending it verbatim.
    #[must_use]
    pub fn is_template(self) -> bool {
        matches!(
            self,
            Self::HeaderTemplate | Self::UriTemplate | Self::RequestTemplate
        )
    }
}

/// Whether a literal carries a runtime-filled placeholder.
///
/// Covers the `printf` family, Rust/Python brace formatting, and the `<name>` and
/// `$name` conventions — enough to tell a template apart from a fixed literal without
/// knowing which language built it.
///
/// # Arguments
///
/// * `value` - The literal's value
#[must_use]
pub fn has_placeholder(value: &str) -> bool {
    // a printf conversion: '%' followed by flags/width and a conversion letter
    let bytes = value.as_bytes();
    for (idx, &byte) in bytes.iter().enumerate() {
        if byte != b'%' {
            continue;
        }
        // Walk the flags/width/precision, then test the conversion character. The
        // space flag (`% d`) is deliberately not accepted: it only means anything for a
        // signed numeric conversion, while prose runs `100% complete` past it into a
        // spurious `%c` on every percentage a program ever prints.
        let mut at = idx + 1;
        while at < bytes.len()
            && (bytes[at].is_ascii_digit()
                || matches!(
                    bytes[at],
                    b'-' | b'+' | b'.' | b'#' | b'$' | b'l' | b'h' | b'z'
                ))
        {
            at += 1;
        }
        if bytes.get(at).is_some_and(|c| {
            matches!(
                c,
                b's' | b'd' | b'u' | b'x' | b'X' | b'i' | b'f' | b'p' | b'c'
            )
        }) {
            return true;
        }
    }
    // brace, angle-bracket, and dollar placeholders
    value.contains("{}")
        || value.contains("{0}")
        || value.contains("{1}")
        || has_angle_placeholder(value)
        || value.contains("${")
}

/// Whether `value` contains an `<name>` fill point.
///
/// Written as a bounded span rather than "contains a `<` and contains a `>`",
/// because the loose form calls anything holding both characters templated — and a
/// data table that ramps through the printable ASCII range holds every character
/// there is. The name must also look like a name: a short run with no whitespace,
/// so a comparison written out in prose is not mistaken for a placeholder.
///
/// # Arguments
///
/// * `value` - The literal to test
fn has_angle_placeholder(value: &str) -> bool {
    // the longest a placeholder name is worth entertaining
    const MAX_NAME: usize = 32;
    let bytes = value.as_bytes();
    for (idx, &byte) in bytes.iter().enumerate() {
        if byte != b'<' {
            continue;
        }
        // scan for the closing bracket, giving up once the span is implausibly long
        let end = (idx + 1 + MAX_NAME).min(bytes.len());
        let Some(close) = bytes[idx + 1..end].iter().position(|b| *b == b'>') else {
            continue;
        };
        // the name itself: non-empty, and made of the characters a name is made of
        let name = &bytes[idx + 1..idx + 1 + close];
        if !name.is_empty()
            && name
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
        {
            return true;
        }
    }
    false
}

/// Whether a path ends in the name of a compilation input.
///
/// Only the end of the path is tested, never a middle segment: a build path records the
/// file a statement was compiled from, so the source file is always the last thing on
/// it, and honouring `/` as a boundary too would condemn any URI routed under a
/// directory that happens to be named like one. The second boundary is an uppercase
/// letter, because a statically linked image stores its literals unseparated and the
/// seam where the next one begins reads as `escape.rsEmpty` — a real end-of-path that
/// the string table simply does not mark.
///
/// Some of these extensions are a single letter after the dot, so the boundary test is
/// what carries the whole guard; `/api/list.something` shares a prefix with `.s` and
/// must survive.
///
/// # Arguments
///
/// * `value` - The path to test, which must be ASCII
fn carries_source_extension(value: &str) -> bool {
    // match case-insensitively; indices stay aligned because the input is ASCII
    let lower = value.to_ascii_lowercase();
    let bytes = value.as_bytes();
    for ext in SOURCE_FILE_EXTS {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(ext) {
            let at = from + rel;
            let after = at + ext.len();
            // a bare `/.rs` is a dotfile, not a named source file
            let named = at > 0 && bytes[at - 1] != b'/';
            // ...and the extension has to be the last thing on the path
            let bounded = bytes.get(after).is_none_or(u8::is_ascii_uppercase);
            if named && bounded {
                return true;
            }
            from = after;
        }
    }
    false
}

/// Whether a leading-slash literal is a URI path rather than a filesystem path.
///
/// The two are genuinely ambiguous in isolation; this rules out the Unix/macOS
/// layout prefixes, the build paths a compiler embeds, dot-directory segments, and
/// lone base64-shaped blobs, and the caller is expected to report the remaining
/// ambiguity rather than assert a URI.
///
/// # Arguments
///
/// * `value` - The literal's value
#[must_use]
pub fn is_uri_path(value: &str) -> bool {
    // a path starts with a slash and has something after it
    if !value.starts_with('/') || value.len() < 2 || value.len() > 512 {
        return false;
    }
    // a Windows-style or protocol-relative form is not a URI path
    if value.contains('\\') || value.starts_with("//") {
        return false;
    }
    // every character has to be legal in a path, query, or format specifier
    if !value.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '/' | '.'
                    | '-'
                    | '_'
                    | '~'
                    | '%'
                    | '?'
                    | '='
                    | '&'
                    | '+'
                    | ':'
                    | '@'
                    | '{'
                    | '}'
                    | '$'
                    | '*'
                    | ','
                    | ';'
                    | '('
                    | ')'
                    | '\''
                    | '!'
            )
    }) {
        return false;
    }
    // at least one segment has to carry a letter, or it is a number list
    if !value.chars().any(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    // a dot-directory segment is dotfile/config layout, not a request target
    if value.contains("/.") {
        return false;
    }
    // a lone segment with no query that ends in '=' or is base64-shaped is an
    // encoded blob (a key, a token) that merely starts with a slash
    let segment = &value[1..];
    if !segment.contains('/') && !segment.contains('?') {
        // base64 padding glued to a single segment gives the encoding away
        if segment.ends_with('=') {
            return false;
        }
        // a mixed-case alphanumeric run long enough to be an encoded value
        let base64_shaped = segment.len() >= 12
            && segment.chars().all(|c| c.is_ascii_alphanumeric())
            && segment.chars().any(|c| c.is_ascii_uppercase())
            && segment.chars().any(|c| c.is_ascii_lowercase());
        if base64_shaped {
            return false;
        }
    }
    // the Unix/macOS layout prefixes are filesystem paths far more often than URI paths
    let lower = value.to_ascii_lowercase();
    if FILESYSTEM_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return false;
    }
    // a build path a compiler recorded is not something the program ever requests
    if TOOLCHAIN_PATH_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return false;
    }
    !carries_source_extension(value)
}

/// Whether a literal matches the RFC 9110 product-token grammar (`name/version`).
///
/// This is the shape of every User-Agent, but also of the version banners a linked
/// library leaves in its data, so a match on its own is weak evidence — see
/// [`classify_http`] for how the two are separated.
///
/// # Arguments
///
/// * `value` - The literal's value
fn is_product_token(value: &str) -> bool {
    // the token is everything up to the first slash; the version follows it
    let Some((name, rest)) = value.split_once('/') else {
        return false;
    };
    if name.is_empty() || name.len() > 40 {
        return false;
    }
    // RFC 9110 token characters, minus the ones no real product name uses
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
    {
        return false;
    }
    // the version has to start with a digit
    let version: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    version.chars().next().is_some_and(|c| c.is_ascii_digit())
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
}

/// Whether `needle` occurs in `haystack` as a word of its own (no identifier
/// character on either side).
///
/// # Arguments
///
/// * `haystack` - The lower-cased text to search
/// * `needle` - The ASCII token to find
fn contains_word_bounded(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    // walk every occurrence until one has non-identifier neighbours
    let mut from = 0;
    while let Some(at) = haystack[from..].find(needle) {
        let start = from + at;
        let end = start + needle.len();
        // an identifier character on either side extends the token past the needle
        let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let after_ok = bytes.get(end).is_none_or(|byte| !is_word_byte(*byte));
        if before_ok && after_ok {
            return true;
        }
        // resume just past this occurrence's start (the needle is ASCII, so the
        // next byte is a character boundary)
        from = start + 1;
    }
    false
}

/// Whether a byte extends an identifier/word token.
///
/// # Arguments
///
/// * `byte` - The byte to test
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Split a `Name: value` header line into its name and value.
///
/// # Arguments
///
/// * `value` - The literal's value
fn split_header(value: &str) -> Option<(String, &str)> {
    let (name, rest) = value.split_once(':')?;
    // a header name is a token: no spaces, no empty, and bounded in length
    if name.is_empty() || name.len() > 40 {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return None;
    }
    Some((name.to_ascii_lowercase(), rest))
}

/// The longest rest a glued known-header line still reads as a value, in bytes.
///
/// Real header values the fabric carries (media types, encodings, hand-built token
/// strings) sit well under this; prose that happens to open with a header word runs
/// long past it.
const MAX_GLUED_VALUE_LEN: usize = 128;

/// Whether the text after a known header's colon looks like a header value.
///
/// Accepts a short, printable, single-line rest with no `::` (a scoped code path's
/// tell, wherever it appears in the rest). Used only for *known* header names: a
/// glued real-header template (`Content-Type:application/x-www-form-urlencoded`) is
/// exactly what malware builds when it assembles requests by concatenation, while
/// the gluing FPs this grammar rejects — Rust paths, ODBC/`k=v` strings — never
/// hang off a known header name with a value-shaped rest.
///
/// # Arguments
///
/// * `rest` - The text after the header name's colon
fn looks_like_header_value(rest: &str) -> bool {
    // a value is non-empty, bounded, and free of the scoped-path tell
    if rest.is_empty() || rest.len() > MAX_GLUED_VALUE_LEN || rest.contains("::") {
        return false;
    }
    // one line of printable text (tabs allowed; header values are single-line)
    rest.chars()
        .all(|c| c == '\t' || (!c.is_control() && c.is_ascii()))
}

/// Fixed byte sequences the HTTP specifications require a conforming implementation to
/// emit verbatim, with their whitespace squashed out.
///
/// These are not request fabric. Every one of them is a constant the protocol itself
/// mandates, so any program speaking the protocol at all carries it — an HTTP/2 client
/// that omits the preface is not an HTTP/2 client. Finding one says only that the
/// binary links an HTTP stack, which the networking-API and runtime-symbol passes
/// already report on much better evidence. Reported as request fabric they read as a
/// hand-built request the author wrote, which is exactly backwards.
///
/// Stored squashed because the literals are mostly delimiter: the preface is
/// `PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n`, and an implementation may hold it whole, split
/// across the blank line, or with the line endings written separately.
const HTTP_PROTOCOL_CONSTANTS: &[&str] = &[
    // RFC 9113 s3.4 — the mandatory client connection preface
    "pri*http/2.0",
    // RFC 9110 s15.2 — the interim responses a server must be able to send
    "http/1.1100continue",
    "http/1.1101switchingprotocols",
];

/// The number of squashed characters compared against [`HTTP_PROTOCOL_CONSTANTS`].
///
/// Comfortably longer than the longest entry, so a match is never cut short, while
/// keeping the scratch allocation fixed no matter how long the literal is.
const PROTOCOL_CONSTANT_PREFIX: usize = 64;

/// Whether a literal is a protocol-mandated constant rather than anything the program's
/// author chose.
///
/// Compares the literal's leading characters — lowercased, with all ASCII whitespace
/// and control bytes removed — against [`HTTP_PROTOCOL_CONSTANTS`]. Squashing is what
/// makes the test hold up: the constants are defined in terms of CRLFs that an
/// implementation is free to store separately from the text around them, so matching
/// them literally would only catch whichever spelling happened to be in front of us.
///
/// A prefix test rather than equality, because a stack that holds the preface and the
/// first frame it sends in one buffer still carries the preface.
///
/// # Arguments
///
/// * `value` - The literal's value
#[must_use]
fn is_protocol_constant(value: &str) -> bool {
    // squash the literal down to its significant leading characters
    let squashed: String = value
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .take(PROTOCOL_CONSTANT_PREFIX)
        .flat_map(char::to_lowercase)
        .collect();
    // a mandated constant is a prefix of whatever buffer it was stored in
    HTTP_PROTOCOL_CONSTANTS
        .iter()
        .any(|constant| squashed.starts_with(constant))
}

/// Classify a literal as a piece of the HTTP request fabric, if it is one.
///
/// Tried most-specific first, so a `User-Agent:` header is a User-Agent rather than a
/// generic header, and a header carrying a placeholder is a template rather than a
/// fixed line. Returns `None` for anything that does not match a grammar — prose with
/// a colon in it, a bare word (including a bare method token like `GET`, which every
/// HTTP client alive carries), a filesystem path, or a constant the protocol required
/// the program to carry.
///
/// # Arguments
///
/// * `value` - The literal's value
#[must_use]
pub fn classify_http(value: &str) -> Option<HttpArtifact> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 2048 {
        return None;
    }
    // a constant the protocol mandates is not something the author wrote
    if is_protocol_constant(trimmed) {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let templated = has_placeholder(trimmed);

    // 1) a whole request line or a hand-rolled request/response template
    let first_word = trimmed.split([' ', '\t']).next().unwrap_or_default();
    let starts_with_method = HTTP_METHODS.contains(&first_word);
    if starts_with_method && trimmed.contains(' ') {
        return Some(HttpArtifact::RequestTemplate);
    }
    // a version token claims request fabric only when it stands as a word of its
    // own AND the literal is shaped like protocol text — a request line, a trailing
    // `HTTP/x` tail, or raw CRLF framing. Prose, symbol names, and library banners
    // mention the version without being any of those
    if HTTP_VERSION_TOKENS
        .iter()
        .any(|token| contains_word_bounded(&lower, token))
    {
        let ends_with_version = HTTP_VERSION_TOKENS
            .iter()
            .any(|token| lower.ends_with(token));
        if starts_with_method || ends_with_version || trimmed.contains("\r\n") {
            return Some(HttpArtifact::RequestTemplate);
        }
    }

    // 2) a header line, typed by which header it is
    if let Some((name, rest)) = split_header(trimmed) {
        // a double colon is a scoped code path (`Connection::poll`), never a header
        let scoped = rest.starts_with(':');
        let known = STANDARD_HEADERS.contains(&name.as_str());
        // a real header's value is empty, set off from the name by whitespace, or
        // filled in at request time; a *known* header also accepts a value glued to
        // the colon (`Content-Type:application/x-www-form-urlencoded` — the shape a
        // hand-concatenated request template takes), provided the rest reads as a
        // header value. Anything else glued to the colon is prose/code
        let header_shaped = rest.is_empty()
            || rest.starts_with(|c: char| c.is_whitespace())
            || has_placeholder(rest)
            || (known && looks_like_header_value(rest));
        // a program's own header follows the `x-` convention; anything else with a
        // colon is prose until proven otherwise
        let custom = name.starts_with("x-") && name.len() > 2;
        if !scoped && header_shaped && (known || custom) {
            return Some(match name.as_str() {
                "user-agent" => HttpArtifact::UserAgent,
                "cookie" | "set-cookie" => HttpArtifact::CookieName,
                "content-type" if !templated => HttpArtifact::ContentType,
                _ if templated || has_placeholder(rest) => HttpArtifact::HeaderTemplate,
                _ => HttpArtifact::HeaderName,
            });
        }
    }

    // 3) a bare media type
    if MEDIA_TYPES.iter().any(|prefix| lower.starts_with(prefix)) && !lower.contains(' ') {
        return Some(HttpArtifact::ContentType);
    }

    // 4) a URI path or the template one is built from
    if is_uri_path(trimmed) {
        return Some(if templated || trimmed.contains('?') {
            HttpArtifact::UriTemplate
        } else {
            HttpArtifact::UriPath
        });
    }

    // 5) a bare product token. Every User-Agent has this shape and so does every
    // linked library's version banner, so it is only claimed as a User-Agent when the
    // literal also carries the parenthesized platform comment a real UA has.
    if is_product_token(trimmed) && trimmed.contains('(') && trimmed.contains(')') {
        return Some(HttpArtifact::UserAgent);
    }
    // a bare method token ("GET", "POST") is shared by every HTTP client alive, and
    // a `k=v;` template matches ODBC connection strings as readily as cookies, so
    // neither is a classification of its own
    None
}

// ===========================================================================
// Runtime symbol markers
// ===========================================================================

/// Return the first entry of a runtime-symbol marker table that `name` matches.
///
/// A statically linked Go or Rust binary has no import table, so the only
/// evidence that a package is compiled in is the symbol names Ghidra recovers.
/// Matching is a lower-case substring test in table order, so the first entry
/// wins and a caller controls precedence by ordering its own table.
///
/// The marker string is read through a closure rather than a trait so each
/// caller's table can carry whatever payload it needs alongside the marker —
/// a protocol family for the network detection, an identity class for the
/// host-binding one.
///
/// # Arguments
///
/// * `name` - The Ghidra function name to classify
/// * `markers` - The caller's marker table, in precedence order
/// * `marker_of` - Reads the lower-case marker substring out of a table entry
pub fn match_runtime_marker<'a, T>(
    name: &str,
    markers: &'a [T],
    marker_of: impl Fn(&T) -> &'static str,
) -> Option<&'a T> {
    // lower-case once, then test each marker in table order
    let lower = name.to_ascii_lowercase();
    markers
        .iter()
        .find(|entry| lower.contains(marker_of(entry)))
}

// ===========================================================================
// Host-identity literals
// ===========================================================================

/// What a recovered literal identifies about the machine it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HostIdKind {
    /// A network adapter's hardware address.
    MacAddress,
    /// A GUID/UUID — a machine, volume, installation, or product identifier.
    Guid,
    /// A volume/disk serial number in the `XXXX-XXXX` form Windows renders.
    VolumeSerial,
}

impl HostIdKind {
    /// A short noun naming what the value identifies, for the finding text.
    pub fn label(self) -> &'static str {
        match self {
            Self::MacAddress => "MAC address",
            Self::Guid => "GUID",
            Self::VolumeSerial => "volume serial number",
        }
    }
}

/// One host-identity literal found inside a larger piece of text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostIdMatch {
    /// What the value identifies.
    pub kind: HostIdKind,
    /// The byte offset the match starts at within the searched text.
    pub at: usize,
    /// The matched text, exactly as it appeared.
    pub value: String,
}

/// Find every host-identity literal in `text`.
///
/// All three shapes are matched on appearance alone, so every candidate is put
/// through [`word_bounded`] — without it a run of numeric constants in a
/// statically linked binary's string blob manufactures dash-form MAC addresses
/// by the dozen. Matches are found in decreasing order of specificity (GUID,
/// then MAC, then serial) and overlapping spans are discarded, so the four
/// hex digits at the front of a GUID are never also reported as a serial.
///
/// Placeholder values — the all-zero and all-ones forms every networking stack
/// carries as a sentinel — are rejected: they identify nothing.
///
/// # Arguments
///
/// * `text` - The string to search
pub fn find_host_identifiers(text: &str) -> Vec<HostIdMatch> {
    let mut found: Vec<HostIdMatch> = Vec::new();
    // most specific first, so a longer shape claims its bytes before a shorter
    // one can carve a match out of the middle of it
    collect_guids(text, &mut found);
    collect_macs(text, &mut found);
    collect_volume_serials(text, &mut found);
    // report in the order they appear in the text
    found.sort_by_key(|hit| hit.at);
    found
}

/// Find every host-identity literal in a run of raw bytes from a literal pool.
///
/// The case [`find_host_identifiers`] cannot see. Several toolchains concatenate all
/// of a program's string literals into one blob with no terminators between them,
/// carrying the length of each at its use site instead; the result has no delimiters
/// for a string-table pass to find, so the analysis back end defines either nothing
/// there or one enormous string, and either way a value stored in it is invisible to
/// a scan of discrete strings. This reads the section bytes directly.
///
/// Two rules have to give for that to work, and both are replaced rather than dropped:
///
/// * The run may end on a hex digit ([`Termination::Open`]), because the byte after a
///   value is the first byte of the next literal.
/// * [`word_bounded`] is replaced by [`separator_bounded`]. Every byte in a blob has a
///   token character on either side, so the word-boundary rule rejects everything;
///   what still means something is whether the *shape's own* grammar continues, which
///   is a separator sitting where the next group would start.
///
/// The volume-serial shape is not searched here. It is four hex digits, a dash, and
/// four more, which without a terminator to bound it also describes the tail of every
/// version string a build stamps into its blob — measured against 514 MB of program
/// data, opening that shape's terminator turned 10 matches into 517, all of the new
/// ones slices of module pseudo-versions. GUID and MAC gained 14 and 0 respectively
/// over the same corpus, so those two are searched.
///
/// Offsets in the returned matches are byte offsets into `bytes`.
///
/// # Arguments
///
/// * `bytes` - The raw section bytes to search
pub fn find_blob_host_identifiers(bytes: &[u8]) -> Vec<HostIdMatch> {
    let mut found: Vec<HostIdMatch> = Vec::new();
    // most specific first, so a GUID claims its bytes before a MAC can be carved
    // out of the middle of one — the same ordering `find_host_identifiers` uses
    collect_blob(bytes, HostIdKind::Guid, GUID_GROUPS, b"-", &mut found);
    collect_blob(bytes, HostIdKind::MacAddress, MAC_GROUPS, b":-", &mut found);
    // report in the order they appear in the blob
    found.sort_by_key(|hit| hit.at);
    found
}

/// Whether `at..at + len` overlaps any span already claimed.
///
/// # Arguments
///
/// * `found` - The matches collected so far
/// * `at` - The candidate's start offset
/// * `len` - The candidate's byte length
fn overlaps(found: &[HostIdMatch], at: usize, len: usize) -> bool {
    // a candidate is discarded if any byte of it is already spoken for
    found
        .iter()
        .any(|hit| at < hit.at + hit.value.len() && hit.at < at + len)
}

/// The hex-digit-group lengths of the canonical GUID rendering.
const GUID_GROUPS: &[usize] = &[8, 4, 4, 4, 12];

/// The hex-digit-group lengths of the six-octet MAC rendering.
const MAC_GROUPS: &[usize] = &[2, 2, 2, 2, 2, 2];

/// Collect `8-4-4-4-12` hex GUIDs, with or without enclosing braces.
///
/// # Arguments
///
/// * `text` - The string to search
/// * `found` - The match list to append to
fn collect_guids(text: &str, found: &mut Vec<HostIdMatch>) {
    /// The hex-digit-group lengths of the canonical GUID rendering.
    const GROUPS: &[usize] = GUID_GROUPS;
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        // try to read the grouped shape starting here
        match read_groups(bytes, at, GROUPS, b'-', Termination::Sealed) {
            // a GUID of all zeros is the "no identifier" sentinel, not an identity
            Some(len) if word_bounded(text, at, len) && !is_placeholder(&text[at..at + len]) => {
                found.push(HostIdMatch {
                    kind: HostIdKind::Guid,
                    at,
                    value: text[at..at + len].to_string(),
                });
                at += len;
            }
            _ => at += 1,
        }
    }
}

/// Collect MAC addresses in colon and dash notation.
///
/// # Arguments
///
/// * `text` - The string to search
/// * `found` - The match list to append to
fn collect_macs(text: &str, found: &mut Vec<HostIdMatch>) {
    /// The hex-digit-group lengths of the six-octet rendering.
    const GROUPS: &[usize] = MAC_GROUPS;
    let bytes = text.as_bytes();
    // both separators are equally conventional, so both are searched
    for separator in *b":-" {
        let mut at = 0;
        while at < bytes.len() {
            match read_groups(bytes, at, GROUPS, separator, Termination::Sealed) {
                Some(len)
                    if word_bounded(text, at, len)
                        && !is_placeholder(&text[at..at + len])
                        && !overlaps(found, at, len) =>
                {
                    found.push(HostIdMatch {
                        kind: HostIdKind::MacAddress,
                        at,
                        value: text[at..at + len].to_string(),
                    });
                    at += len;
                }
                _ => at += 1,
            }
        }
    }
}

/// Collect `XXXX-XXXX` volume serial numbers.
///
/// The weakest of the three shapes — eight hex digits and a dash also describe a
/// part number, a build identifier, and half a dozen other things — so this one
/// additionally requires a hex letter somewhere in it, which rules out the purely
/// decimal runs (dates, version numbers, ticket ids) that dominate the false
/// positives. Callers are expected to corroborate it further before scoring it.
///
/// # Arguments
///
/// * `text` - The string to search
/// * `found` - The match list to append to
fn collect_volume_serials(text: &str, found: &mut Vec<HostIdMatch>) {
    /// The hex-digit-group lengths of the Windows serial rendering.
    const GROUPS: &[usize] = &[4, 4];
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match read_groups(bytes, at, GROUPS, b'-', Termination::Sealed) {
            Some(len)
                if word_bounded(text, at, len)
                    && !is_placeholder(&text[at..at + len])
                    && !overlaps(found, at, len)
                    // a purely decimal run is far more likely to be anything else
                    && text[at..at + len].bytes().any(|b| b.is_ascii_hexdigit() && !b.is_ascii_digit()) =>
            {
                found.push(HostIdMatch {
                    kind: HostIdKind::VolumeSerial,
                    at,
                    value: text[at..at + len].to_string(),
                });
                at += len;
            }
            _ => at += 1,
        }
    }
}

/// Collect one shape out of a concatenated literal pool.
///
/// The blob counterpart to the `collect_*` functions above: same shapes, read with
/// [`Termination::Open`] and bounded by [`separator_bounded`] instead of
/// [`word_bounded`]. A MAC candidate is additionally put through
/// [`is_degenerate_mac`], which is where most of the rejection happens — over 514 MB
/// of program data the shape alone matched 8 times, the boundary rule took that to 7
/// and the placeholder test to 5, and the octet tests took it to 1. The four they
/// removed were a Bluetooth `BDADDR_LOCAL` constant and a manual page's worked
/// example, both of which spell out an all-zero manufacturer prefix.
///
/// # Arguments
///
/// * `bytes` - The raw section bytes to search
/// * `kind` - The shape being collected
/// * `groups` - The required hex-digit count of each group, in order
/// * `separators` - The separator bytes to try, each searched in full
/// * `found` - The match list to append to
fn collect_blob(
    bytes: &[u8],
    kind: HostIdKind,
    groups: &[usize],
    separators: &[u8],
    found: &mut Vec<HostIdMatch>,
) {
    for separator in separators {
        let mut at = 0;
        while at < bytes.len() {
            match read_groups(bytes, at, groups, *separator, Termination::Open) {
                Some(len) if separator_bounded(bytes, at, len, *separator) => {
                    // the run is hex digits and separators, so it is always ASCII
                    let value = String::from_utf8_lossy(&bytes[at..at + len]).into_owned();
                    // the sentinels, and the degenerate runs that are not addresses
                    let rejected = is_placeholder(&value)
                        || (kind == HostIdKind::MacAddress && is_degenerate_mac_text(&value))
                        || overlaps(found, at, len);
                    if rejected {
                        at += 1;
                        continue;
                    }
                    found.push(HostIdMatch { kind, at, value });
                    at += len;
                }
                _ => at += 1,
            }
        }
    }
}

/// Whether a match at `at..at + len` is the whole of its shape, not a slice of a longer one.
///
/// The blob replacement for [`word_bounded`], which cannot be used on a concatenated
/// literal pool: everything in one has a token character on both sides, so that rule
/// rejects every candidate including the real ones. What still discriminates is the
/// shape's own grammar. A group-and-separator run continues past the match exactly
/// when a separator sits where the next group would begin, so
/// `…-01-02-03-04-05-06-08-09-…` is refused at every offset it could be read from
/// while `…TinySizeClassc8:27:cc:c2:37:5adecryption failed…` is not: the letters
/// glued to either end are the neighbouring literals, and neither is a separator.
///
/// # Arguments
///
/// * `bytes` - The blob being searched
/// * `at` - The byte offset the match starts at
/// * `len` - The byte length of the match
/// * `separator` - The separator the shape was read with
fn separator_bounded(bytes: &[u8], at: usize, len: usize, separator: u8) -> bool {
    // a separator before the match means a group precedes it
    if at > 0 && bytes[at - 1] == separator {
        return false;
    }
    // ...and one after it means a group follows
    match at.checked_add(len) {
        Some(end) => bytes.get(end) != Some(&separator),
        // an overflowing span is a caller error, not a match
        None => false,
    }
}

/// Whether a MAC address written out with separators is a degenerate run.
///
/// A thin wrapper over [`is_degenerate_mac`] for callers holding the text form.
///
/// # Arguments
///
/// * `value` - The matched literal, separators included
fn is_degenerate_mac_text(value: &str) -> bool {
    // parse the six octets back out of the rendering
    let octets: Vec<u8> = value
        .split([':', '-'])
        .filter_map(|group| u8::from_str_radix(group, 16).ok())
        .collect();
    // a rendering that does not parse into six octets is not judged here
    octets.len() == 6 && is_degenerate_mac(&octets)
}

/// Whether six octets are a degenerate run rather than an adapter's own address.
///
/// The tests that hold however the rendering was found — a terminated literal in the
/// string table, or an unterminated one in a literal pool. Each rejects a shape that
/// occurs for a reason other than identifying an adapter.
///
/// # Arguments
///
/// * `octets` - The six octets, in wire order
pub fn is_degenerate_mac(octets: &[u8]) -> bool {
    // six octets or it is not an address at all
    if octets.len() != 6 {
        return true;
    }
    // the sentinels every networking stack carries
    if octets.iter().all(|b| *b == 0) || octets.iter().all(|b| *b == 0xff) {
        return true;
    }
    // a repeated byte is padding or a fill pattern, not an address
    if octets.iter().all(|b| *b == octets[0]) {
        return true;
    }
    // a monotone run is a counter, an index table, or a test vector
    let ascending = octets
        .windows(2)
        .all(|pair| pair[1] == pair[0].wrapping_add(1));
    let descending = octets
        .windows(2)
        .all(|pair| pair[1] == pair[0].wrapping_sub(1));
    if ascending || descending {
        return true;
    }
    // the low bit of the first octet marks a group address, not an adapter's own
    if octets[0] & 0x01 != 0 {
        return true;
    }
    // an all-zero OUI is not an assigned manufacturer prefix. This is the test that
    // does the work on values written out as text: a documented example and a
    // protocol's "any address" constant are both spelled with a null prefix
    if octets[..3].iter().all(|b| *b == 0) {
        return true;
    }
    // half the octets being zero is the signature of a table of small little-endian
    // numbers — pointers, RVAs, offsets, lengths — whose high bytes are always zero,
    // and a six-byte window over one of those is the single richest source of
    // fabricated addresses in any binary. A real adapter address spends three bytes
    // on an assigned OUI and three on a device identifier, so it is not mostly zero.
    // This does give up the genuinely sparse address (`00:50:56:00:00:01` is a real
    // VMware one), which is the right trade: that address is indistinguishable from
    // padding by any means available here
    octets.iter().filter(|b| **b == 0).count() >= 3
}

/// How the byte after a shape's last group is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Termination {
    /// The run must end where the hex digits do.
    ///
    /// What a value stored as its own string looks like: something delimits it, so a
    /// seventh hex digit after a six-digit group means the group is really seven
    /// digits long and the shape was never there.
    Sealed,
    /// The run may butt straight up against another hex digit.
    ///
    /// What a value stored in a concatenated literal pool looks like. The runtimes
    /// that build one emit every literal end to end with no terminators, so the byte
    /// after a value is the first byte of the *next* value, and roughly a quarter of
    /// English words begin with a letter that is also a hex digit.
    Open,
}

/// Read a separated run of fixed-length hex-digit groups starting at `at`.
///
/// Returns the total byte length of the run, or `None` if the text at `at` does
/// not match the shape exactly — a group that is too short, too long, or not hex
/// all fail, so `1234-5678-9` never passes as `4-4`.
///
/// # Arguments
///
/// * `bytes` - The text being searched
/// * `at` - The offset to start reading at
/// * `groups` - The required hex-digit count of each group, in order
/// * `separator` - The byte that must appear between groups
/// * `termination` - Whether the run has to end where its digits do
fn read_groups(
    bytes: &[u8],
    at: usize,
    groups: &[usize],
    separator: u8,
    termination: Termination,
) -> Option<usize> {
    let mut pos = at;
    for (index, want) in groups.iter().enumerate() {
        // every group after the first is preceded by the separator
        if index > 0 {
            if bytes.get(pos) != Some(&separator) {
                return None;
            }
            pos += 1;
        }
        // exactly `want` hex digits, no more and no fewer
        let start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_hexdigit() && pos - start < *want {
            pos += 1;
        }
        if pos - start != *want {
            return None;
        }
        // a further hex digit means the group is longer than the shape allows. Only
        // the last group can be spared this, and only for an `Open` run: an interior
        // group is followed by the separator either way, so nothing is loosened
        let last = index + 1 == groups.len();
        if (!last || termination == Termination::Sealed)
            && bytes.get(pos).is_some_and(u8::is_ascii_hexdigit)
        {
            return None;
        }
    }
    Some(pos - at)
}

/// Whether a hex literal is the all-zero or all-ones sentinel.
///
/// # Arguments
///
/// * `value` - The matched literal, separators included
fn is_placeholder(value: &str) -> bool {
    // strip the separators and look at the digits themselves
    let digits: Vec<u8> = value
        .bytes()
        .filter(u8::is_ascii_hexdigit)
        .map(|b| b.to_ascii_lowercase())
        .collect();
    // every networking stack ships both of these as "unset" and "broadcast"
    digits.iter().all(|b| *b == b'0') || digits.iter().all(|b| *b == b'f')
}

/// Whether a GUID literal is one of the ubiquitous platform identifiers.
///
/// COM/OLE interface ids, shell and media class ids, and the `DirectShow`
/// baseclasses sample pair recur across a large share of all Windows binaries: a
/// program carries them because it links a platform API, not because anything is
/// keyed to a machine, so reporting each one floods a session with values no
/// analyst can pivot on. Three cheap family markers cover nearly all of them —
/// the OLE automation `data4` `C000-000000000046` shared by the base COM
/// interface ids (`IUnknown`, `IDispatch`, ...), and the `E260-11CF`
/// `data2`/`data3` pair and `00AA004A34D5` node, each shared by one of the big
/// mid-nineties platform interface-id blocks — plus an exact list for the pair
/// of `DirectShow` baseclasses GUIDs every binary built on that SDK embeds.
///
/// Deliberately *not* a validity test: a unique, author-chosen GUID (the kind a
/// campaign is actually keyed or tracked by) matches none of these families and
/// is unaffected.
///
/// # Arguments
///
/// * `value` - The GUID rendering, with or without braces, any case
pub fn is_ubiquitous_guid(value: &str) -> bool {
    /// The exact ubiquitous GUIDs the family rules do not cover.
    const DENYLIST: &[&str] = &[
        // the DirectShow baseclasses pair, compiled into every binary built on
        // that SDK's sample code
        "42e65c96-15e8-40c0-b04a-90d5ccf6e2eb",
        "3cfffc43-d199-46fb-bfee-8342b653a491",
    ];
    // normalize: strip braces and compare in lower case
    let trimmed = value.trim().trim_start_matches('{').trim_end_matches('}');
    let lower = trimmed.to_ascii_lowercase();
    // split the five groups of the canonical rendering; anything else is not a
    // GUID rendering and is judged by its own matcher, not here
    let groups: Vec<&str> = lower.split('-').collect();
    if groups.len() != 5 {
        return false;
    }
    // the OLE automation node every base COM interface id ends with
    if groups[3] == "c000" && groups[4] == "000000000046" {
        return true;
    }
    // the shared node of one mid-nineties platform interface-id block
    if groups[4] == "00aa004a34d5" {
        return true;
    }
    // ...and the shared timestamp fields of another
    if groups[1] == "e260" && groups[2] == "11cf" {
        return true;
    }
    // the handful of exact values the families do not cover
    DENYLIST.contains(&lower.as_str())
}

/// Whether a literal could be a computer, domain, or account name.
///
/// Shape only — these values have no distinguishing structure, so this is a
/// filter for the *candidate* set a caller has already narrowed by other means
/// (typically "referenced by the function that enumerates the name"). On its own
/// it proves nothing: it accepts most short identifiers.
///
/// # Arguments
///
/// * `value` - The literal to test
pub fn plausible_host_name(value: &str) -> bool {
    /// The longest a NetBIOS/DNS label or account name runs.
    const MAX_NAME: usize = 63;
    /// Suffixes that make a literal a filename, not a machine or account name.
    const FILE_SUFFIXES: &[&str] = &[
        ".dll", ".exe", ".sys", ".so", ".dylib", ".txt", ".log", ".dat", ".bin", ".tmp", ".ini",
        ".xml", ".json", ".cfg", ".db",
    ];
    let value = value.trim();
    // an empty or over-long literal is not a name
    if value.is_empty() || value.len() > MAX_NAME {
        return false;
    }
    // names use the identifier character set; anything else (spaces, slashes,
    // format specifiers, punctuation) means this is prose, a path, or a template
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'$'))
    {
        return false;
    }
    // a bare number is a count, an offset, or a version — not a name
    if value.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return false;
    }
    // a filename that happens to satisfy the character set is still a filename
    let lower = value.to_ascii_lowercase();
    !FILE_SUFFIXES.iter().any(|suffix| lower.ends_with(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `word_bounded` accepts a delimited match and rejects a slice out of a longer run.
    #[test]
    fn word_bounded_rejects_slices_of_longer_runs() {
        // a standalone match, and one delimited by non-token characters
        assert!(word_bounded("192.168.1.10", 0, 12));
        assert!(word_bounded("host=192.168.1.10;", 5, 12));
        // the same shape sliced out of a longer numeric run is not an indicator
        assert!(!word_bounded("1.4.14.2.9.1.1", 0, 8));
        assert!(!word_bounded("01-02-03-04-05-06-08-09", 0, 17));
        assert!(!word_bounded("v1.2.3.4", 1, 7));
        // an out-of-range span is rejected rather than panicking
        assert!(!word_bounded("abc", 1, 99));
    }

    /// `parse_url` splits a full URL into scheme/host/port/path.
    #[test]
    fn parse_url_splits_components() {
        let url = parse_url("https://evil.example.com:8443/gate.php?id=1").expect("URL parses");
        assert_eq!(url.scheme, "https");
        assert_eq!(url.host, "evil.example.com");
        assert_eq!(url.port, Some(8443));
        assert_eq!(url.path, "/gate.php?id=1");
        assert_eq!(url.effective_port(), Some(8443));
    }

    /// A URL with no explicit port falls back to the scheme default and a `/` path.
    #[test]
    fn parse_url_defaults_port_and_path() {
        let url = parse_url("http://cdn.example.org").expect("URL parses");
        assert_eq!(url.port, None);
        assert_eq!(url.effective_port(), Some(80));
        assert_eq!(url.path, "/");
    }

    /// A URL embedded in a request template is still recovered.
    #[test]
    fn parse_url_extracts_from_surrounding_text() {
        let url = parse_url("GET http://1.2.3.4/beacon HTTP/1.1").expect("URL parses");
        assert_eq!(url.host, "1.2.3.4");
        assert_eq!(url.path, "/beacon");
        assert_eq!(url.raw, "http://1.2.3.4/beacon");
    }

    /// Userinfo is stripped and a `.onion` host is recognized.
    #[test]
    fn parse_url_recognizes_onion_and_strips_userinfo() {
        let url = parse_url("http://bob:pw@xyzabc234.onion/cmd").expect("URL parses");
        assert_eq!(url.host, "xyzabc234.onion");
        assert!(url.is_onion());
    }

    /// Non-network schemes and malformed text are rejected.
    #[test]
    fn parse_url_rejects_non_network_schemes() {
        assert!(parse_url("file:///c:/windows/system32").is_none());
        assert!(parse_url("://nohost").is_none());
        assert!(parse_url("just some prose").is_none());
    }

    /// `parse_endpoint` accepts `ip:port` / `host:port` and rejects `key:value` text.
    #[test]
    fn parse_endpoint_requires_plausible_host() {
        assert_eq!(
            parse_endpoint("192.168.1.10:4444"),
            Some(("192.168.1.10".to_string(), 4444))
        );
        assert_eq!(
            parse_endpoint("c2.example.com:8080"),
            Some(("c2.example.com".to_string(), 8080))
        );
        assert!(parse_endpoint("error:42").is_none());
        assert!(parse_endpoint("Content-Length:0").is_none());
        assert!(parse_endpoint("example.com:70000").is_none());
        assert!(parse_endpoint("host.com:0").is_none());
        assert!(parse_endpoint("http://example.com:80").is_none());
        assert!(parse_endpoint("two words:80").is_none());
    }

    /// `parse_ipv4_literal` accepts real dotted quads and rejects malformed ones.
    #[test]
    fn parse_ipv4_literal_validates_octets() {
        assert!(parse_ipv4_literal("192.168.1.10").is_some());
        assert!(parse_ipv4_literal("8.8.8.8").is_some());
        assert!(parse_ipv4_literal("1.2.3.999").is_none());
        assert!(parse_ipv4_literal("01.2.3.4").is_none());
        assert!(parse_ipv4_literal("1.2.3").is_none());
        assert!(parse_ipv4_literal("1.2.3.4.5").is_none());
        assert!(parse_ipv4_literal("0.0.0.0").is_none());
        assert!(parse_ipv4_literal("255.255.255.255").is_none());
        assert!(parse_ipv4_literal("v1.2.3.4").is_none());
        assert!(parse_ipv4_literal("glibc 2.3.4").is_none());
    }

    /// `parse_ipv6_literal` accepts real addresses and rejects colon-separated noise.
    #[test]
    fn parse_ipv6_literal_validates_addresses() {
        assert!(parse_ipv6_literal("2001:db8::1").is_some());
        assert!(parse_ipv6_literal("fe80::1ff:fe23:4567:890a").is_some());
        assert!(parse_ipv6_literal("12:34:56").is_none());
        assert!(parse_ipv6_literal("::").is_none());
        assert!(parse_ipv6_literal("1.2.3.4").is_none());
    }

    /// `classify_http` types each piece of the request fabric distinctly.
    #[test]
    fn classify_http_types_the_request_fabric() {
        assert_eq!(
            classify_http("User-Agent: Mozilla/5.0 (Windows NT 10.0)"),
            Some(HttpArtifact::UserAgent)
        );
        assert_eq!(
            classify_http("Mozilla/4.0 (compatible; MSIE 8.0)"),
            Some(HttpArtifact::UserAgent)
        );
        assert_eq!(
            classify_http("POST /submit.php"),
            Some(HttpArtifact::RequestTemplate)
        );
        assert_eq!(
            classify_http("%s /%s HTTP/1.1\r\nHost: %s\r\n"),
            Some(HttpArtifact::RequestTemplate)
        );
        assert_eq!(
            classify_http("X-Session-Id: %s"),
            Some(HttpArtifact::HeaderTemplate)
        );
        assert_eq!(
            classify_http("Accept-Encoding: gzip, deflate"),
            Some(HttpArtifact::HeaderName)
        );
        assert_eq!(
            classify_http("Cookie: sid=%s"),
            Some(HttpArtifact::CookieName)
        );
        assert_eq!(
            classify_http("Content-Type: application/json"),
            Some(HttpArtifact::ContentType)
        );
        assert_eq!(
            classify_http("application/octet-stream"),
            Some(HttpArtifact::ContentType)
        );
        assert_eq!(classify_http("/api/v2/status"), Some(HttpArtifact::UriPath));
        assert_eq!(
            classify_http("/upload?id=%d"),
            Some(HttpArtifact::UriTemplate)
        );
    }

    /// Bare method tokens are every client's fabric, never a seeded finding.
    #[test]
    fn classify_http_demotes_bare_method_tokens() {
        // a lone verb carries no information about the sample's own requests
        assert_eq!(classify_http("POST"), None);
        assert_eq!(classify_http("GET"), None);
        // a verb opening a request line is still request fabric
        assert_eq!(
            classify_http("POST /submit.php"),
            Some(HttpArtifact::RequestTemplate)
        );
    }

    /// A version token classifies only inside protocol-shaped text.
    #[test]
    fn classify_http_gates_version_tokens_on_protocol_shape() {
        // a request line ending in the version token classifies
        assert_eq!(
            classify_http("GET /gate.php HTTP/1.1"),
            Some(HttpArtifact::RequestTemplate)
        );
        // so does a template ending in the version token with no method
        assert_eq!(
            classify_http("%s %s HTTP/1.1"),
            Some(HttpArtifact::RequestTemplate)
        );
        // and raw CRLF framing mid-string marks a hand-rolled response/request
        assert_eq!(
            classify_http("HTTP/1.1 200 OK\r\nContent-Length: %d\r\n\r\n"),
            Some(HttpArtifact::RequestTemplate)
        );
        // prose mentioning a version is not request fabric
        assert_eq!(classify_http("supports http/1.1 and spdy"), None);
        // a version glued into a longer identifier is not word-bounded
        assert_eq!(classify_http("winhttp/1.1transport"), None);
    }

    /// A scoped code path never reads as a header, but a known header glued to a
    /// plausible value does.
    ///
    /// The glued-rest rule exists for Rust paths (`Connection::poll`) and ODBC/`k=v`
    /// connection strings, not for real headers: a glued real-header template
    /// (`Content-Type:application/x-www-form-urlencoded` — a confirmed C2 lead) is
    /// exactly what malware builds when it assembles requests by concatenation, so a
    /// known header name followed by a value-shaped rest classifies. `Accept:gzip`
    /// rides the same rule — Accept is a known header and gzip a plausible value.
    #[test]
    fn classify_http_rejects_scoped_paths_but_keeps_glued_known_headers() {
        // the Rust path shape (`Type::method`) a panicking binary embeds by the dozen
        assert_eq!(classify_http("Connection::poll"), None);
        // the confirmed glued hand-built header template classifies by its known name
        assert_eq!(
            classify_http("Content-Type:application/x-www-form-urlencoded"),
            Some(HttpArtifact::ContentType)
        );
        // a known header glued to a short plausible value is the same template shape
        assert_eq!(classify_http("Accept:gzip"), Some(HttpArtifact::HeaderName));
        // a glued placeholder is request-time fill-in, so it stays a template
        assert_eq!(
            classify_http("X-Key:%s"),
            Some(HttpArtifact::HeaderTemplate)
        );
        // an unknown name glued to a value is still prose/code, not a header
        assert_eq!(classify_http("note:this is not a header"), None);
        // a known name glued to a scoped path is code, not a value
        assert_eq!(classify_http("Host:std::net::TcpStream"), None);
        // a known name glued to an over-long rest is prose, not a header value
        let long = format!("Accept:{}", "a".repeat(200));
        assert_eq!(classify_http(&long), None);
    }

    /// `classify_http` does not report the constants the protocol mandates.
    #[test]
    fn classify_http_rejects_protocol_constants() {
        // the HTTP/2 preface, in the spellings a stack actually stores it in
        assert!(classify_http("PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n").is_none());
        assert!(classify_http("PRI * HTTP/2.0").is_none());
        // held in one buffer with the frame that follows it
        assert!(classify_http("PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x12\x04\x00").is_none());
        // the interim status lines every server has to be able to send
        assert!(classify_http("HTTP/1.1 100 Continue\r\n\r\n").is_none());
        assert!(classify_http("HTTP/1.1 101 Switching Protocols\r\n").is_none());
        // a request the author built is still request fabric, version string and all
        assert_eq!(
            classify_http("GET /assets/index.php HTTP/1.1"),
            Some(HttpArtifact::RequestTemplate)
        );
        assert_eq!(
            classify_http("HTTP/1.1 200 OK\r\nContent-Length: %d\r\n\r\n"),
            Some(HttpArtifact::RequestTemplate)
        );
    }

    /// `classify_http` leaves prose, library banners, and filesystem paths alone.
    #[test]
    fn classify_http_rejects_lookalikes() {
        // a bare product token is every linked library's version banner too, so it is
        // only a User-Agent when it carries the parenthesized platform comment
        assert!(classify_http("libcurl/7.68.0").is_none());
        // a colon in prose is not a header line
        assert!(classify_http("Note: this file was generated").is_none());
        // a Unix layout path is a filesystem path, not a URI path
        assert!(classify_http("/usr/lib/locale").is_none());
        assert!(classify_http("/etc/resolv.conf").is_none());
        assert!(classify_http("hello").is_none());
        assert!(classify_http("the quick brown fox").is_none());
    }

    /// `has_placeholder` separates runtime-filled templates from fixed literals.
    #[test]
    fn has_placeholder_finds_runtime_fill_points() {
        assert!(has_placeholder("/api/%s/status"));
        assert!(has_placeholder("Host: %-16s"));
        assert!(has_placeholder("/user/{}/profile"));
        assert!(has_placeholder("${HOME}/config"));
        assert!(has_placeholder("/id/<token>"));
        assert!(!has_placeholder("/api/v2/status"));
        assert!(!has_placeholder("100% complete"));
        // both brackets present, but nothing between them is a placeholder: a data
        // table that ramps through printable ASCII holds every character there is
        assert!(!has_placeholder("!\"#$%&'()*+,-./0123456789:;<=>?@ABC"));
        assert!(!has_placeholder("if a < b then c > d"));
        // ...and a name that never closes, or closes far too late, is not one either
        assert!(!has_placeholder("<unterminated"));
        assert!(!has_placeholder(
            "<a very long stretch of prose that eventually closes>"
        ));
    }

    /// `k=v;` cookie-template shapes do not classify: ODBC strings share the shape.
    #[test]
    fn classify_http_rejects_cookie_template_shapes() {
        // the ODBC connection-string shape (a measured-corpus junk value)
        assert_eq!(classify_http("DRIVER={SQL Server};UID=sa;PWD=%s"), None);
        // a bare k=v; template reads the same way the ODBC string does
        assert_eq!(classify_http("sid=%s;path=/"), None);
        assert_eq!(classify_http("session_id=%s; path=/"), None);
        // a punctuation ramp carries `=` and `;` without carrying a cookie
        assert_eq!(classify_http("!\"#$%&'()*+,-./0123456789:;<=>?@"), None);
        // a real Cookie header classifies through the header grammar instead
        assert_eq!(
            classify_http("Cookie: sid=%s"),
            Some(HttpArtifact::CookieName)
        );
    }

    /// `is_uri_path` rules out the Windows and Unix filesystem forms.
    #[test]
    fn is_uri_path_rules_out_filesystem_forms() {
        assert!(is_uri_path("/index.php"));
        assert!(is_uri_path("/a/b/c-d_e~f"));
        assert!(!is_uri_path("/"));
        assert!(!is_uri_path("//cdn.example.com/x"));
        assert!(!is_uri_path("/tmp\\file"));
        assert!(!is_uri_path("/var/log/messages"));
        assert!(!is_uri_path("/1/2/3"));
        assert!(!is_uri_path("api/v2"));
    }

    /// A compiler-embedded build path is not a URI path, including when the linker has
    /// run it into the literal that follows it.
    #[test]
    fn is_uri_path_rules_out_compiler_build_paths() {
        // the plain form: a source file at the end of the path
        assert!(!is_uri_path("/rust/deps/hashbrown-0.14.5/src/raw/mod.rs"));
        assert!(!is_uri_path("/build/glibc-x/stdlib/qsort.c"));
        // the concatenated form: the extension is buried, but the marker still shows
        assert!(!is_uri_path(
            "/rustc/eeb90cda1969383f56a2637cbd3037bdf598841c/library/core/src/escape.rsEmptyInvalidDigitPos"
        ));
        // ...and the extension test finds the seam on its own, without a marker
        assert!(!is_uri_path("/project/src/parser.cppFailedToOpen"));
        // a directory that merely looks like a source file does not condemn the path
        assert!(is_uri_path("/dev.c/status"));
        // nor does a segment that only happens to contain the letters
        assert!(is_uri_path("/api/v2/orders.chart"));
        assert!(is_uri_path("/download/report.pdf"));
    }

    /// macOS layout prefixes and dotfile/encoded-blob shapes are not URI paths.
    #[test]
    fn is_uri_path_rejects_macos_and_blob_shapes() {
        // the macOS layout prefixes are filesystem facts, like the Unix ones
        assert!(!is_uri_path("/System/Library/Frameworks"));
        assert!(!is_uri_path("/Library/Preferences/com.apple.x"));
        assert!(!is_uri_path("/Applications/Safari.app"));
        assert!(!is_uri_path("/Volumes/Install"));
        // a dot-directory segment is dotfile/config layout, not a request target
        assert!(!is_uri_path("/.config"));
        assert!(!is_uri_path("/data/.hidden/x"));
        // a lone base64-shaped segment is an encoded blob, not a path
        assert!(!is_uri_path("/dGVzdHN0cmluZw=="));
        assert!(!is_uri_path("/aGVsbG93b3JsZHh5"));
        // real request targets keep classifying
        assert!(is_uri_path("/panel/gate.php?id="));
        assert!(is_uri_path("/gate.php"));
    }

    /// `is_standard_port` separates well-known service ports from bespoke ones.
    #[test]
    fn is_standard_port_matches_well_known_services() {
        assert!(is_standard_port(443));
        assert!(is_standard_port(53));
        assert!(!is_standard_port(4444));
        assert!(!is_standard_port(1337));
    }

    /// Both MAC notations are recognized, in isolation and inside surrounding text.
    #[test]
    fn find_host_identifiers_reads_both_mac_notations() {
        let colon = find_host_identifiers("00:1a:2b:3c:4d:5e");
        assert_eq!(colon.len(), 1);
        assert_eq!(colon[0].kind, HostIdKind::MacAddress);
        assert_eq!(colon[0].value, "00:1a:2b:3c:4d:5e");
        // the dash form, delimited by non-token characters
        let dash = find_host_identifiers("adapter=A0-B1-C2-D3-E4-F5;");
        assert_eq!(dash.len(), 1);
        assert_eq!(dash[0].value, "A0-B1-C2-D3-E4-F5");
        assert_eq!(dash[0].at, 8);
    }

    /// The blob problem: a shaped run sliced out of a longer one is not an address.
    #[test]
    fn find_host_identifiers_rejects_slices_of_longer_runs() {
        // a concatenated numeric-constant blob contains a perfectly-shaped MAC
        assert!(find_host_identifiers("01-02-03-04-05-06-07-08").is_empty());
        // ...and so does an IPv6 literal, once the colon groups line up
        assert!(find_host_identifiers("fe80::0011:2233:4455:6677:8899").is_empty());
        // seven octets is not six
        assert!(find_host_identifiers("00:11:22:33:44:55:66").is_empty());
        // and a group with the wrong digit count fails the shape outright
        assert!(find_host_identifiers("00:11:22:33:44:5").is_empty());
    }

    /// The all-zero and all-ones forms are sentinels, not identities.
    #[test]
    fn find_host_identifiers_rejects_placeholders() {
        assert!(find_host_identifiers("00:00:00:00:00:00").is_empty());
        assert!(find_host_identifiers("ff:ff:ff:ff:ff:ff").is_empty());
        assert!(find_host_identifiers("00000000-0000-0000-0000-000000000000").is_empty());
    }

    /// A GUID claims its own bytes, so its groups are not re-reported as a serial.
    #[test]
    fn find_host_identifiers_prefers_the_longer_shape() {
        let found = find_host_identifiers("{6F9619FF-8B86-D011-B42D-00CF4FC964FF}");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, HostIdKind::Guid);
        assert_eq!(found[0].value, "6F9619FF-8B86-D011-B42D-00CF4FC964FF");
    }

    /// The serial shape needs a hex letter, which keeps dates and versions out.
    #[test]
    fn volume_serial_requires_a_hex_letter() {
        let found = find_host_identifiers("Volume Serial Number is 1A2B-3C4D");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, HostIdKind::VolumeSerial);
        assert_eq!(found[0].value, "1A2B-3C4D");
        // a purely decimal run of the same shape is almost never a serial
        assert!(find_host_identifiers("2024-1231").is_empty());
    }

    /// A pooled literal has no terminator, so the shape must survive being glued to
    /// the literals on either side of it.
    #[test]
    fn find_blob_host_identifiers_reads_an_unterminated_literal() {
        // the shape of a real pool: three unrelated literals with nothing between them,
        // the one after the address starting on a hex digit
        let found = find_blob_host_identifiers(b"bad TinySizeClassc8:27:cc:c2:37:5adecryption");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, HostIdKind::MacAddress);
        assert_eq!(found[0].value, "c8:27:cc:c2:37:5a");
        assert_eq!(found[0].at, 17);
        // the text pass cannot see the same value, which is why this path exists
        assert!(find_host_identifiers("bad TinySizeClassc8:27:cc:c2:37:5adecryption").is_empty());
    }

    /// Dropping the terminator does not drop the boundary rule: the shape's own
    /// separator continuing past the match still means the match was never there.
    #[test]
    fn find_blob_host_identifiers_rejects_slices_of_longer_runs() {
        // the case the relaxed termination could have re-admitted: a dash-separated
        // run of small numbers, of which any six consecutive pairs read as an address
        assert!(find_blob_host_identifiers(b"h1-01-02-03-04-05-06-08-09-11-12.js").is_empty());
        // and the same in the colon notation
        assert!(find_blob_host_identifiers(b"\0fe80::0011:2233:4455:6677:8899\0").is_empty());
    }

    /// The octet tests are what a pooled MAC is judged on, since nothing delimits it.
    #[test]
    fn find_blob_host_identifiers_rejects_degenerate_addresses() {
        // an all-zero manufacturer prefix: a constant or a worked example, not a host
        assert!(find_blob_host_identifiers(b"BDADDR_ANY00:00:00:00:00:00BDADDR_ALL").is_empty());
        assert!(find_blob_host_identifiers(b"local00:00:00:ff:ff:ffany").is_empty());
    }

    /// GUIDs are searched in a pool; volume serials deliberately are not.
    #[test]
    fn find_blob_host_identifiers_searches_only_the_bounded_shapes() {
        let found = find_blob_host_identifiers(b"unsupported6ba7b810-9dad-11d1-80b4-00c04fd430c8x");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, HostIdKind::Guid);
        assert_eq!(found[0].value, "6ba7b810-9dad-11d1-80b4-00c04fd430c8");
        // four hex digits, a dash, and four more also describes the tail of every
        // version string a build stamps into its pool, so the shape stays out
        assert!(find_blob_host_identifiers(b"Volume Serial Number is 1A2B-3C4D").is_empty());
        assert_eq!(
            find_host_identifiers("Volume Serial Number is 1A2B-3C4D").len(),
            1
        );
    }

    /// `plausible_host_name` keeps identifiers and drops prose, paths, and files.
    #[test]
    fn plausible_host_name_filters_the_candidate_set() {
        assert!(plausible_host_name("WIN-DESKTOP-01"));
        assert!(plausible_host_name("corp.example.local"));
        assert!(plausible_host_name("svc_account$"));
        // prose, templates, and paths are not names
        assert!(!plausible_host_name("failed to open %s"));
        assert!(!plausible_host_name("C:\\Windows\\System32"));
        assert!(!plausible_host_name(""));
        // a bare number is a count or a version
        assert!(!plausible_host_name("1.2.3"));
        // a filename that satisfies the character set is still a filename
        assert!(!plausible_host_name("kernel32.dll"));
    }

    /// Ubiquitous platform GUID families are recognized; unique values are not.
    #[test]
    fn is_ubiquitous_guid_matches_platform_families_only() {
        // the OLE automation node family: IDispatch, braces and case ignored
        assert!(is_ubiquitous_guid("00020400-0000-0000-c000-000000000046"));
        assert!(is_ubiquitous_guid("{0000031A-0000-0000-C000-000000000046}"));
        // the two mid-nineties interface-id blocks, matched by their shared fields
        assert!(is_ubiquitous_guid("deadbeef-e260-11cf-9ae1-00805f14d305"));
        assert!(is_ubiquitous_guid("f0b7a1a3-9847-11cf-8f20-00aa004a34d5"));
        // the DirectShow baseclasses pair, exactly as binaries carry them
        assert!(is_ubiquitous_guid("42E65C96-15E8-40c0-B04A-90D5CCF6E2EB"));
        assert!(is_ubiquitous_guid("3CFFFC43-D199-46fb-BFEE-8342B653A491"));
        // the confirmed campaign GUIDs stay reportable: unique non-COM values
        // (full tails are representative; the fleet pack records the prefixes)
        assert!(!is_ubiquitous_guid("ac634028-9bf2-4876-8a52-6b43e4b1f2d7"));
        assert!(!is_ubiquitous_guid("67bde5d7-c2fc-4d2e-9a51-0c3e8f7b1a94"));
        assert!(!is_ubiquitous_guid("916d809c-5949-4ca3-b1d4-7e2f0a68c315"));
        // a non-GUID rendering is not judged here at all
        assert!(!is_ubiquitous_guid("not-a-guid"));
    }

    /// The shared marker matcher is a lower-case substring test in table order.
    #[test]
    fn match_runtime_marker_is_ordered_and_case_insensitive() {
        let table = [("net/http", "web"), ("net.dial", "socket")];
        assert_eq!(
            match_runtime_marker("net/http.(*Client).Do", &table, |entry| entry.0),
            Some(&("net/http", "web"))
        );
        assert_eq!(
            match_runtime_marker("net.DialTCP", &table, |entry| entry.0),
            Some(&("net.dial", "socket"))
        );
        assert!(match_runtime_marker("FUN_00401000", &table, |entry| entry.0).is_none());
    }
}
