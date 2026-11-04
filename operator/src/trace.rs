//! Sets up the operator's structured logs with the same tracing setup the other Thorium
//! components use

use clap::Parser;
use std::path::Path;
use thorium::Error;
use thorium::conf::{LogLevel, Tracing};
use tracing::instrument::WithSubscriber;
use tracing::subscriber::NoSubscriber;

/// The name the operator's traces are sent under
pub const TRACE_NAME: &str = "ThoriumOperator";

/// The arguments controlling the operator's logs
#[derive(Parser, Debug, Clone, Default)]
pub struct TraceArgs {
    /// The level to log at on stdout (off, error, warn, setup, info, debug, or trace), overriding
    /// the local level in the --trace file
    #[clap(long, global = true, env = "THORIUM_OPERATOR_LOG_LEVEL", value_parser = parse_level)]
    pub log_level: Option<LogLevel>,
    /// A tracing config in the format the other Thorium components use, which can also send
    /// traces to an external gRPC (OTLP) collector
    #[clap(long, global = true, env = "THORIUM_OPERATOR_TRACE")]
    pub trace: Option<String>,
}

/// Parse a log level, ignoring case
///
/// # Arguments
///
/// * `raw` - The level to parse
pub fn parse_level(raw: &str) -> Result<LogLevel, String> {
    // match the level's name in any case
    match raw.trim().to_ascii_lowercase().as_str() {
        "off" => Ok(LogLevel::Off),
        "error" => Ok(LogLevel::Error),
        "warn" | "warning" => Ok(LogLevel::Warn),
        "setup" => Ok(LogLevel::Setup),
        "info" => Ok(LogLevel::Info),
        "debug" => Ok(LogLevel::Debug),
        "trace" => Ok(LogLevel::Trace),
        _ => Err(format!(
            "unknown log level '{raw}' (expected off, error, warn, setup, info, debug, or trace)"
        )),
    }
}

/// Build the tracing config the operator's arguments describe
///
/// # Arguments
///
/// * `args` - The operator's log arguments
pub fn config(args: &TraceArgs) -> Result<Tracing, Error> {
    // load the tracing file like the other components do, using the defaults without one
    let mut conf = match &args.trace {
        // a named file that is missing is refused so a typo can't quietly turn export off
        Some(path) if !Path::new(path).exists() => {
            return Err(Error::new(format!(
                "Failed to load the tracing config {path}: the file does not exist"
            )));
        }
        Some(path) => Tracing::from_file(path).map_err(|error| {
            Error::new(format!("Failed to load the tracing config {path}: {error}"))
        })?,
        None => Tracing::default(),
    };
    // an explicit level overrides the file's local level
    if let Some(level) = args.log_level {
        conf.local.level = level;
    }
    Ok(conf)
}

/// Describe an error with the causes of the kube, HTTP, IO, or Elastic error it wraps
///
/// # Arguments
///
/// * `error` - The error to describe
pub fn describe(error: &Error) -> String {
    // get the error the Thorium error wraps, if it keeps one with causes of its own
    let inner: Option<&dyn std::error::Error> = match error {
        Error::K8s(inner) => Some(inner),
        Error::K8sConfig(inner) => Some(inner),
        Error::Reqwest(inner) => Some(inner),
        Error::IO(inner) => Some(inner),
        Error::Elastic(inner) => Some(inner),
        _ => None,
    };
    // follow the description with each cause it doesn't already contain
    crate::app::helpers::append_causes(error.to_string(), inner.and_then(std::error::Error::source))
}

/// Run a future that sends a credential with tracing switched off for it
///
/// Dependencies record their requests in their own spans at the debug and trace levels (the
/// Scylla driver puts each statement's text in its `Request` span), so a statement or request
/// carrying a password would otherwise reach stdout and any external collector. Events and
/// spans created while this future is polled go nowhere, whatever level is configured.
///
/// # Arguments
///
/// * `future` - The future sending a credential
pub async fn unrecorded<F: Future>(future: F) -> F::Output {
    // poll the future with a subscriber that drops everything
    future.with_subscriber(NoSubscriber::default()).await
}

/// Captures the operator's log lines in tests
#[cfg(test)]
pub(crate) mod capture {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing::subscriber::DefaultGuard;

    /// A writer appending log lines to a shared buffer
    #[derive(Clone, Default)]
    pub(crate) struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Captured {
        /// Get the lines captured so far
        pub(crate) fn text(&self) -> String {
            // copy the buffer out as text
            let buffer = self.0.lock().expect("capture lock");
            String::from_utf8_lossy(&buffer).into_owned()
        }
    }

    impl Write for Captured {
        /// Append bytes to the shared buffer
        ///
        /// # Arguments
        ///
        /// * `bytes` - The bytes to append
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            // append everything written
            self.0
                .lock()
                .expect("capture lock")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        /// Nothing is buffered beyond the shared buffer
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Capture this thread's log lines at the info level in the operator's local format, until
    /// the guard is dropped
    pub(crate) fn capture() -> (Captured, DefaultGuard) {
        // capture at the operator's default level
        capture_at(tracing::Level::INFO)
    }

    /// Capture this thread's log lines at a level in the operator's local format, until the
    /// guard is dropped
    ///
    /// # Arguments
    ///
    /// * `level` - The most verbose level to capture
    pub(crate) fn capture_at(level: tracing::Level) -> (Captured, DefaultGuard) {
        // format lines like the operator's stdout layer, without colors or timestamps
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .without_time()
            .with_max_level(level)
            .finish();
        // only this thread's events are captured
        let guard = tracing::subscriber::set_default(subscriber);
        (captured, guard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::{Level, event, info_span};

    /// An error with a cause, like the kube and HTTP errors the operator wraps
    #[derive(Debug)]
    struct Outer(std::io::Error);

    impl std::fmt::Display for Outer {
        /// Name the outer error
        ///
        /// # Arguments
        ///
        /// * `f` - The formatter to write to
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "request failed")
        }
    }

    impl std::error::Error for Outer {
        /// The cause of the outer error
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    /// Levels parse in any case and an unknown level is refused
    #[test]
    fn levels_parse() {
        // every level parses whatever its case
        assert_eq!(parse_level("debug"), Ok(LogLevel::Debug));
        assert_eq!(parse_level("Info"), Ok(LogLevel::Info));
        assert_eq!(parse_level("WARN"), Ok(LogLevel::Warn));
        assert_eq!(parse_level("warning"), Ok(LogLevel::Warn));
        assert_eq!(parse_level(" trace "), Ok(LogLevel::Trace));
        assert_eq!(parse_level("off"), Ok(LogLevel::Off));
        assert_eq!(parse_level("setup"), Ok(LogLevel::Setup));
        assert_eq!(parse_level("error"), Ok(LogLevel::Error));
        // an unknown level names the levels it takes
        let error = parse_level("loud").expect_err("unknown level");
        assert!(error.contains("'loud'") && error.contains("debug"));
    }

    /// The level defaults to info, a tracing file is loaded, and an explicit level overrides it
    #[test]
    fn config_layers_file_and_level() {
        // no arguments log at info locally with nothing external
        let conf = config(&TraceArgs::default()).expect("default config");
        assert_eq!(conf.local.level, LogLevel::Info);
        assert!(conf.external.is_none());
        // an explicit level replaces the local level
        let args = TraceArgs {
            log_level: Some(LogLevel::Debug),
            trace: None,
        };
        assert_eq!(config(&args).expect("config").local.level, LogLevel::Debug);
        // a tracing file sets the local level and an external collector
        let dir = std::env::temp_dir().join(format!("operator-trace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("tracing.yml");
        std::fs::write(
            &path,
            "local:\n  level: Warn\nexternal:\n  Grpc:\n    endpoint: http://collector:4317\n    level: Info\n",
        )
        .expect("write tracing.yml");
        let path = path.to_string_lossy().into_owned();
        let args = TraceArgs {
            log_level: None,
            trace: Some(path.clone()),
        };
        let conf = config(&args).expect("file config");
        assert_eq!(conf.local.level, LogLevel::Warn);
        assert!(conf.external.is_some());
        // an explicit level overrides only the file's local level
        let args = TraceArgs {
            log_level: Some(LogLevel::Trace),
            trace: Some(path),
        };
        let conf = config(&args).expect("file config with a level");
        assert_eq!(conf.local.level, LogLevel::Trace);
        assert!(conf.external.is_some());
        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    /// A tracing file that is missing or doesn't parse is refused with its path
    #[test]
    fn config_refuses_bad_files() {
        // a missing file is refused rather than replaced by the defaults
        let dir = std::env::temp_dir().join(format!("operator-trace-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let missing = dir.join("missing.yml").to_string_lossy().into_owned();
        let args = TraceArgs {
            log_level: Some(LogLevel::Debug),
            trace: Some(missing.clone()),
        };
        let error = config(&args).expect_err("missing file").to_string();
        assert!(
            error.contains(&missing) && error.contains("does not exist"),
            "{error}"
        );
        // a file that isn't a tracing config is refused with the parse error
        let path = dir.join("bad.yml");
        std::fs::write(&path, "local: [\n").expect("write bad.yml");
        let bad = path.to_string_lossy().into_owned();
        let args = TraceArgs {
            log_level: None,
            trace: Some(bad.clone()),
        };
        let error = config(&args).expect_err("bad file").to_string();
        assert!(
            error.contains(&format!("Failed to load the tracing config {bad}")),
            "{error}"
        );
        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    /// A described error carries the causes of the error it wraps, without repeating any
    #[test]
    fn describe_appends_causes() {
        // an IO error wrapping an error with a cause lists both
        let cause =
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
        let error = Error::IO(std::io::Error::other(Outer(cause)));
        assert_eq!(
            describe(&error),
            "Error: request failed: connection refused"
        );
        // an error without a wrapped cause is described as is
        assert_eq!(describe(&Error::new("bad config")), "Error: bad config");
    }

    /// Events are written with their fields under the spans they happen in
    #[test]
    fn events_carry_their_spans() {
        // log an event inside a reconcile span
        let (captured, _guard) = capture::capture();
        let span = info_span!(
            "ThoriumCluster::reconcile",
            cluster = "thorium",
            generation = 3
        );
        span.in_scope(|| event!(Level::INFO, msg = "Patched secret", secret = "thorium"));
        // the line names the span, its fields, and the event's fields
        let text = captured.text();
        assert!(
            text.contains("ThoriumCluster::reconcile{cluster=\"thorium\" generation=3}"),
            "{text}"
        );
        assert!(
            text.contains("msg=\"Patched secret\" secret=\"thorium\""),
            "{text}"
        );
    }
}
