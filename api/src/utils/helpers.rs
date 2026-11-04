//! Helper functions for the Thorium API and friends
use chrono::prelude::*;
use data_encoding::HEXLOWER;
use futures::Stream;
use sha2::{Digest, Sha256};

#[cfg(feature = "client")]
use tokio::io::AsyncReadExt;

/// Get the number of hours so far in this year
///
/// # Arguments
///
/// * `timestamp` - The timestamp to determine our partition for
/// * `year` - The year to determine our partition for
/// * `chunk` - The size of our partition
///
/// # Panics
///
/// This should never actually panic as its just unwraps on known good values.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_lossless)]
pub fn partition(timestamp: DateTime<Utc>, year: i32, chunk: u16) -> i32 {
    let duration = timestamp.naive_utc()
        - NaiveDate::from_ymd_opt(year, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 1)
            .unwrap();
    // get the correct chunk
    duration.num_seconds() as i32 / chunk as i32
}

/// Get the number of hours so far in this year
///
/// # Arguments
///
/// * `timestamp` - The timestamp to determine our partition for
/// * `year` - The year to determine our partition for
/// * `chunk` - The size of our partition
///
/// # Panics
///
/// This should never actually panic as its just unwraps on known good values.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_lossless)]
pub fn partition_i64(timestamp: i64, year: i32, chunk: u16) -> i32 {
    // build a naive timestamp
    let datetime = DateTime::from_timestamp_millis(timestamp).unwrap();
    // get a duration
    let duration = datetime.naive_utc()
        - NaiveDate::from_ymd_opt(year, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 1)
            .unwrap();
    // get the correct chunk
    duration.num_seconds() as i32 / chunk as i32
}

/// Resolves `FnOnce` errors by asserting an iterator is Send
///
/// See <https://users.rust-lang.org/t/implementation-of-fnonce-is-not-general-enough-with-async-block/83427/3>
///
/// # Arguments
///
/// * `it` - The iterator to assert
pub fn assert_send_stream<R>(it: impl Send + Stream<Item = R>) -> impl Send + Stream<Item = R> {
    it
}

/// Get the sha256 of an iterator of strings
///
/// # Arguments
///
/// * `iterator` - The iterator to build a sha256 from
pub fn sha256_iter<I, S>(iterator: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut hasher = Sha256::new();
    for s in iterator {
        hasher.update(s.as_ref().as_bytes());
    }
    // get this filesystems hash
    let sha256 = HEXLOWER.encode(&hasher.finalize());
    sha256
}

/// Convert a duration to a human readable string
///
/// humantime renders values like "1day 2h", so this inserts a space between
/// each amount and its unit so it reads "1 day 2 h".
///
/// # Arguments
///
/// * `duration` - The duration to convert to a human readable string
///
/// # Examples
///
/// ```
/// use thorium::utils::helpers;
///
/// // returns "1 day"
/// helpers::human_duration(std::time::Duration::from_secs(86400));
/// ```
#[cfg(any(feature = "api", feature = "client"))]
#[must_use]
pub fn human_duration(duration: std::time::Duration) -> String {
    // get this duration in a human readable format
    let raw = humantime::format_duration(duration).to_string();
    // build a human time formatted string with spaces
    let mut human = String::with_capacity(raw.len() + 1);
    // keep track the character before our current one
    let mut prev: Option<char> = None;
    // step over each character and add spaces when needed
    for chr in raw.chars() {
        // we will never add a space on the first digit
        if let Some(prev) = prev {
            // check if our last character was a digit and the current one is not
            if prev.is_ascii_digit() && chr.is_ascii_alphabetic() {
                // add a space to seperate the amount from the time visually
                human.push(' ');
            }
        }
        // add the next character
        human.push(chr);
        // keep track of our previous char
        prev = Some(chr);
    }
    human
}

/// Get the sha256 of a file on disk
#[cfg(feature = "client")]
pub async fn sha256_file(
    path: impl AsRef<std::path::Path>,
) -> Result<String, crate::client::Error> {
    // get a handle to the file to hash
    let file = tokio::fs::File::open(path).await?;
    // use a larger buffer size to optimize for nvme storage
    const BUF_SIZE: usize = 4 * 1024 * 1024; // 4 MiB
    // wrap our file handle in a reader
    let mut reader = tokio::io::BufReader::with_capacity(BUF_SIZE, file);
    // instance a buffer large enough to
    let mut buf = bytes::BytesMut::with_capacity(BUF_SIZE);
    // setup a sha256 hasher
    let mut hasher = Sha256::new();
    // keep hashing this file until complete
    loop {
        // load some data from our file into our buffer
        let n = reader.read_buf(&mut buf).await?;
        // check if there is no more data to read
        if n == 0 {
            break;
        }
        // read only the bytes we wrote to our buffer into the hasher
        hasher.update(&buf[..n]);
    }
    // get this files hash
    Ok(HEXLOWER.encode(&hasher.finalize()))
}

/// The text logged in place of a URL that can't be parsed
const UNPARSEABLE_URL: &str = "<unparseable url>";

/// Find where a URL's scheme ends, at the start of its `://`
///
/// A `://` only ends a scheme when no `@`, `/`, `?`, or `#` comes before it, so a URL in a query
/// isn't taken for the URL's own scheme.
///
/// # Arguments
///
/// * `url` - The URL to find the scheme of
fn scheme_end(url: &str) -> Option<usize> {
    url.find("://")
        .filter(|end| !url[..*end].contains(['@', '/', '?', '#']))
}

/// Remove the user info before the last `@` that comes before a URL's query or fragment
///
/// A URL in a query doesn't hide the user info before it (see [`scheme_end`]). Everything after
/// the scheme up to and including the last `@` before the query or fragment is removed, so
/// credentials are dropped even when a stray `/` comes before them
/// (`https://ftp//user:password@host/path`). A `?` or `#` only starts the query or fragment once
/// a `/` has come before it, so one inside an unencoded password doesn't end the search early.
///
/// # Arguments
///
/// * `url` - The URL to strip user info from
fn strip_user_info(url: &str) -> String {
    // find where the authority starts, after the scheme if there is one
    let start = scheme_end(url).map_or(0, |end| end + 3);
    // user info can only come before a query or fragment, which starts after a path's '/'
    let end = url[start..]
        .find('/')
        .and_then(|slash| {
            url[start + slash..]
                .find(['?', '#'])
                .map(|mark| start + slash + mark)
        })
        .unwrap_or(url.len());
    // drop everything up to and including the last '@' before that
    match url[start..end].rfind('@') {
        Some(at) => format!("{}{}", &url[..start], &url[start + at + 1..]),
        None => url.to_owned(),
    }
}

/// Remove any user info (a username and password) from a URL so it can be logged or shown
///
/// The user info is first stripped as text, which also catches user info a lenient parse would
/// read as part of the path (`https://ftp//user:password@host/path`). The rest is then parsed,
/// and the parsed URL is returned with any user info left in it cleared. A URL without a scheme
/// (`github.com/corn/harvest`) is parsed as an `https` URL and returned without the scheme, the
/// same way repo URLs are read. A URL that doesn't parse to one with a host, such as one whose
/// unencoded password holds a `/` before a `?`, `#`, or `://`, or an scp-like git URL
/// (`git@host:path`), is replaced by `<unparseable url>`, so no text split can let a credential
/// through.
///
/// # Arguments
///
/// * `url` - The URL to redact
#[must_use]
pub fn redact_url(url: &str) -> String {
    // strip the user info as text before parsing
    let stripped = strip_user_info(url);
    // parse a URL with a scheme as given and one without as an https URL
    let schemeless = scheme_end(&stripped).is_none();
    let parsed = if schemeless {
        url::Url::parse(&format!("https://{stripped}"))
    } else {
        url::Url::parse(&stripped)
    };
    // only a URL with a host can be shown without its user info
    let mut parsed = match parsed {
        Ok(parsed) if parsed.has_host() => parsed,
        _ => return UNPARSEABLE_URL.to_owned(),
    };
    // clear any user info the parser still found; a URL with a host always accepts this
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    // serialize the parsed URL, without the root path the parser adds to a bare host
    let mut redacted = String::from(parsed);
    if redacted.ends_with('/') && !stripped.ends_with('/') {
        redacted.pop();
    }
    // drop the scheme added to a URL that had none
    match redacted.strip_prefix("https://") {
        Some(rest) if schemeless => rest.to_owned(),
        _ => redacted,
    }
}

/// Redact the user info of every URL inside a piece of text
///
/// A URL is found by its `://` and runs from the start of its scheme to the next whitespace,
/// quote, `)`, or `>`, the characters error messages wrap URLs in.
///
/// # Arguments
///
/// * `text` - The text whose URLs to redact
fn redact_urls_in(text: &str) -> String {
    // copy the text, replacing each URL with its redacted form
    let mut redacted = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(marker) = rest.find("://") {
        // the scheme runs back from the marker over the characters a scheme may hold
        let start = rest[..marker]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
            .map_or(0, |before| before + 1);
        // the URL ends at the first character messages wrap URLs in
        let end = rest[marker..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | '>'))
            .map_or(rest.len(), |after| marker + after);
        redacted.push_str(&rest[..start]);
        redacted.push_str(&redact_url(&rest[start..end]));
        rest = &rest[end..];
    }
    redacted.push_str(rest);
    redacted
}

/// Describe an error along with every error that caused it, joined with `": "`
///
/// Many errors, such as reqwest's, leave their cause out of their own message, so "error sending
/// request" alone can't tell a refused connection from a TLS failure or a DNS miss. A cause whose
/// message repeats one already shown, as wrappers that print their inner error do, is skipped,
/// and the user info of any URL in a message is redacted.
///
/// # Arguments
///
/// * `error` - The error to describe
#[must_use]
pub fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    // walk the error and its sources, keeping each message not already shown
    let mut messages: Vec<String> = Vec::new();
    let mut current = Some(error);
    while let Some(link) = current {
        let message = redact_urls_in(&link.to_string());
        if !message.is_empty() && !messages.contains(&message) {
            messages.push(message);
        }
        current = link.source();
    }
    messages.join(": ")
}

#[cfg(test)]
mod tests {
    use super::{error_chain, redact_url};

    /// User info is stripped from https, ssh, and git URLs, with or without a password, and from
    /// malformed URLs that still carry it
    #[test]
    fn redact_url_strips_user_info() {
        // each URL and what it must look like once redacted
        let cases = [
            (
                "https://alice:SecretPw1@github.com/corn/harvest",
                "https://github.com/corn/harvest",
            ),
            ("https://user:password@host", "https://host"),
            (
                "http://token@gitlab.example.com:8443/a/b.git",
                "http://gitlab.example.com:8443/a/b.git",
            ),
            (
                "ssh://git:SecretPw1@github.com/corn/harvest.git",
                "ssh://github.com/corn/harvest.git",
            ),
            (
                "git://alice@example.com/corn.git",
                "git://example.com/corn.git",
            ),
            (
                "alice:SecretPw1@github.com/corn/harvest",
                "github.com/corn/harvest",
            ),
            (
                "https://alice:p@ss@github.com/corn",
                "https://github.com/corn",
            ),
            (
                "https://ftp//alice:SecretPw1@github.com/corn/harvest",
                "https://github.com/corn/harvest",
            ),
            (
                "https://user:pa?ss@github.com/corn/harvest",
                "https://github.com/corn/harvest",
            ),
            (
                "https://user:pa#ss@github.com/corn/harvest",
                "https://github.com/corn/harvest",
            ),
            (
                "alice:SecretPw1@github.com/corn/harvest?next=https://x",
                "github.com/corn/harvest?next=https://x",
            ),
            (
                "https://ftp//alice:SecretPw1@github.com/corn?next=a@b",
                "https://github.com/corn?next=a@b",
            ),
        ];
        for (url, expected) in cases {
            assert_eq!(redact_url(url), expected, "redacting {url}");
        }
    }

    /// URLs without user info are returned unchanged
    #[test]
    fn redact_url_keeps_plain_urls() {
        // URLs with no user info, including ones with an '@' in their query or fragment
        for url in [
            "https://github.com/corn/harvest",
            "https://host",
            "http://127.0.0.1:9200",
            "github.com/corn/harvest",
            "https://github.com/corn/harvest?ref=a@b#frag@c",
            "https://github.com/corn/harvest#frag@c",
            "github.com/corn/harvest?next=https://alice@x",
            "localhost:9200",
        ] {
            assert_eq!(redact_url(url), url, "redacting {url}");
        }
    }

    /// URLs that can't be parsed are replaced by a placeholder, so user info a text split would
    /// miss never reaches a log
    #[test]
    fn redact_url_hides_unparseable_urls() {
        // URLs whose password holds a '/' before a '?', '#', or '://', and other unparseable URLs
        for url in [
            "https://user:a/b?c@github.com/corn/harvest",
            "https://user:a/b#c@github.com/corn/harvest",
            "user:a/b?c@github.com/corn/harvest",
            "alice:p://w@github.com/corn/harvest",
            "git@github.com:corn/harvest.git",
            "",
        ] {
            assert_eq!(redact_url(url), "<unparseable url>", "redacting {url}");
        }
    }

    /// A test error with a message and an optional cause
    #[derive(Debug)]
    struct Link {
        /// The error's own message
        message: &'static str,
        /// The error that caused it
        cause: Option<Box<Link>>,
    }

    impl std::fmt::Display for Link {
        /// Write the error's own message
        ///
        /// # Arguments
        ///
        /// * `f` - The formatter to write to
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.message)
        }
    }

    impl std::error::Error for Link {
        /// The error that caused this one
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.cause
                .as_deref()
                .map(|cause| cause as &(dyn std::error::Error + 'static))
        }
    }

    /// Build a chain of test errors, outermost first
    ///
    /// # Arguments
    ///
    /// * `messages` - Each error's message, outermost first
    fn chain(messages: &[&'static str]) -> Link {
        // wrap each cause in the error before it
        let mut iter = messages.iter().rev();
        let mut link = Link {
            message: iter.next().expect("a message"),
            cause: None,
        };
        for message in iter {
            link = Link {
                message,
                cause: Some(Box::new(link)),
            };
        }
        link
    }

    /// The chain names every cause once, in order, and redacts URL user info
    #[test]
    fn error_chain_names_causes() {
        // a lone error is just its message
        assert_eq!(error_chain(&chain(&["boom"])), "boom");
        // causes follow in order, and a wrapper repeating its cause's message is shown once
        let error = chain(&[
            "error sending request for url (https://es:9200/_mapping)",
            "error sending request for url (https://es:9200/_mapping)",
            "client error (Connect)",
            "Connection refused (os error 111)",
        ]);
        assert_eq!(
            error_chain(&error),
            "error sending request for url (https://es:9200/_mapping): client error (Connect): \
             Connection refused (os error 111)"
        );
        // credentials in a URL inside a message are dropped
        let error = chain(&[
            "error for url (https://elastic:Secret1@es:9200/x)",
            "refused",
        ]);
        assert_eq!(
            error_chain(&error),
            "error for url (https://es:9200/x): refused"
        );
    }
}
