//! Re-rendering views until interrupted and polling until a condition holds

use std::future::Future;
use std::io::Write;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;
use thorium::Error;
use tokio::time::{Instant, sleep};

use super::output::{self, OutputFormat, View};
use crate::args::helpers::{WaitForArgs, WatchArgs};

/// The exit code Thorctl uses when a `--wait-for` condition did not hold before its timeout
pub const TIMEOUT_EXIT_CODE: i32 = 3;

/// Whether a `--wait-for` condition timed out in this process
static TIMED_OUT: AtomicBool = AtomicBool::new(false);

/// The exit code Thorctl should use for a failed command
///
/// This is [`TIMEOUT_EXIT_CODE`] when a `--wait-for` condition timed out, and 1 otherwise.
#[must_use]
pub fn exit_code() -> i32 {
    if TIMED_OUT.load(Ordering::Relaxed) {
        TIMEOUT_EXIT_CODE
    } else {
        1
    }
}

/// A condition that `--wait-for` polls a view until it holds
///
/// A condition is either `<field>=<value>`, which holds when the field equals the value
/// (case-insensitively for strings, or when any element of a list field matches), or a bare
/// `<field>`, which holds when the field is set to a non-empty, non-false value. Fields can be
/// nested with `.`, and a `-` in a field name also matches a `_`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitFor {
    /// The condition as the user wrote it
    raw: String,
    /// The path of field names to the field to check
    pub path: Vec<String>,
    /// The value the field must have, or `None` if it only needs to be set
    pub expected: Option<String>,
}

impl FromStr for WaitFor {
    /// A malformed condition fails with a message saying why
    type Err = String;

    /// Parse a condition from `<field>=<value>` or `<field>`
    ///
    /// # Arguments
    ///
    /// * `raw` - The condition to parse
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        // ignore any surrounding whitespace
        let trimmed = raw.trim();
        // split our condition into its field and its expected value if it has one
        let (field, expected) = match trimmed.split_once('=') {
            Some((field, value)) => (field.trim(), Some(value.trim())),
            None => (trimmed, None),
        };
        // a condition needs a field to check
        if field.is_empty() {
            return Err(format!(
                "'{raw}' is not a condition; use <field>=<value> or <field>"
            ));
        }
        // a condition with an '=' needs a value after it
        if expected.is_some_and(str::is_empty) {
            return Err(format!("'{raw}' has no value after '='"));
        }
        // split nested field names on '.'
        let path: Vec<String> = field.split('.').map(ToOwned::to_owned).collect();
        // every nested field needs a name
        if path.iter().any(String::is_empty) {
            return Err(format!("'{raw}' has an empty field name"));
        }
        Ok(WaitFor {
            raw: trimmed.to_owned(),
            path,
            expected: expected.map(ToOwned::to_owned),
        })
    }
}

impl std::fmt::Display for WaitFor {
    /// Display this condition as the user wrote it
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.raw)
    }
}

/// Get a field from a JSON object, also trying the name with each `-` replaced by `_`
///
/// # Arguments
///
/// * `value` - The JSON value to get a field from
/// * `name` - The name of the field
fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    // only objects have fields
    let object = value.as_object()?;
    // try the exact name first, then the name with dashes as underscores
    object
        .get(name)
        .or_else(|| object.get(name.replace('-', "_").as_str()))
}

/// Whether a JSON value is set to something other than null, false, or an empty value
///
/// # Arguments
///
/// * `value` - The value to check
fn is_set(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(set) => *set,
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
        Value::Number(_) => true,
    }
}

/// Whether a JSON value matches the value a condition expects
///
/// # Arguments
///
/// * `value` - The value to check
/// * `expected` - The value the condition expects
fn matches(value: &Value, expected: &str) -> bool {
    match value {
        // strings compare without case so `status=completed` matches `Completed`
        Value::String(text) => text.eq_ignore_ascii_case(expected),
        // numbers and bools compare by their JSON text
        Value::Number(number) => number.to_string() == expected,
        Value::Bool(set) => expected == if *set { "true" } else { "false" },
        // a list matches when any of its elements does
        Value::Array(items) => items.iter().any(|item| matches(item, expected)),
        // null only matches the word null
        Value::Null => expected.eq_ignore_ascii_case("null"),
        // objects never match a single value
        Value::Object(_) => false,
    }
}

impl WaitFor {
    /// Whether this condition holds for one item
    ///
    /// # Arguments
    ///
    /// * `item` - The item to check
    #[must_use]
    pub fn holds(&self, item: &Value) -> bool {
        // walk down to the field this condition checks
        let mut current = item;
        for name in &self.path {
            match field(current, name) {
                Some(next) => current = next,
                None => return false,
            }
        }
        // check the field against our expected value, or that it is set at all
        match &self.expected {
            Some(expected) => matches(current, expected),
            None => is_set(current),
        }
    }

    /// Whether this condition holds for every item, and there is at least one item
    ///
    /// # Arguments
    ///
    /// * `items` - The items to check
    #[must_use]
    pub fn holds_all(&self, items: &[Value]) -> bool {
        !items.is_empty() && items.iter().all(|item| self.holds(item))
    }
}

/// Whether an error is worth retrying, like a server error, a timeout, or a dropped connection
///
/// Client errors such as a missing or forbidden item won't change by asking again, so they are
/// not retried.
///
/// # Arguments
///
/// * `error` - The error to check
fn is_transient(error: &Error) -> bool {
    match error {
        // the API answered with a server error
        Error::Thorium { code, .. } => code.is_server_error(),
        // the request failed on the way or the server answered with a server error
        Error::Reqwest(err) => err
            .status()
            .map_or(err.is_timeout() || err.is_connect(), |status| {
                status.is_server_error()
            }),
        // anything else is a problem with the command itself
        _ => false,
    }
}

/// Poll a value until a check holds or a timeout passes
///
/// The value is fetched once more at the timeout before giving up. Transient fetch errors are
/// printed to stderr and polling goes on; any other error stops it. Returns the value the check
/// held for, or `None` if the timeout passed first.
///
/// # Arguments
///
/// * `fetch` - Fetches the current value
/// * `check` - Checks whether the value is the one we are waiting for
/// * `timeout` - How long to poll for, or `None` to poll forever
/// * `interval` - How long to wait between polls
pub async fn poll_until<T, F, Fut, C>(
    mut fetch: F,
    mut check: C,
    timeout: Option<Duration>,
    interval: Duration,
) -> Result<Option<T>, Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Error>>,
    C: FnMut(&T) -> Result<bool, Error>,
{
    // get the time we stop polling at if we have a timeout
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    loop {
        // fetch the current value and return it if it is the one we want
        match fetch().await {
            Ok(value) => {
                if check(&value)? {
                    return Ok(Some(value));
                }
            }
            // report a transient error and poll again
            Err(error) if is_transient(&error) => {
                eprintln!("Error: {error}; retrying");
            }
            Err(error) => return Err(error),
        }
        // never wait past our deadline, and give up once it has passed
        let mut delay = interval;
        if let Some(deadline) = deadline {
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            delay = delay.min(deadline - now);
        }
        // wait before polling again
        sleep(delay).await;
    }
}

/// Poll a value until a condition holds for its items, then return it
///
/// When the timeout passes first, Thorctl will exit with [`TIMEOUT_EXIT_CODE`].
///
/// # Arguments
///
/// * `fetch` - Fetches the current value
/// * `items` - Gets the items the condition must hold for from a value
/// * `condition` - The condition to wait for
/// * `timeout` - How long to wait before giving up
/// * `interval` - How long to wait between polls
pub async fn wait_until<T, F, Fut, I>(
    fetch: F,
    mut items: I,
    condition: &WaitFor,
    timeout: Duration,
    interval: Duration,
) -> Result<T, Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Error>>,
    I: FnMut(&T) -> Result<Vec<Value>, Error>,
{
    // poll every interval until our condition holds for every item
    let check = |value: &T| Ok(condition.holds_all(&items(value)?));
    match poll_until(fetch, check, Some(timeout), interval).await? {
        Some(value) => Ok(value),
        None => {
            // remember that we timed out so Thorctl exits with the timeout code
            TIMED_OUT.store(true, Ordering::Relaxed);
            Err(Error::new(format!(
                "Timed out after {timeout:?} waiting for '{condition}'"
            )))
        }
    }
}

/// Run a render step again and again until interrupted
///
/// On a terminal the screen is cleared before each render. A transient error is printed to
/// stderr and the next render goes on; any other error stops.
///
/// # Arguments
///
/// * `interval` - How long to wait between renders
/// * `render` - Fetches and prints the view once
pub async fn repeat<F, Fut>(interval: Duration, mut render: F) -> Result<(), Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), Error>>,
{
    // only clear the screen when a person is watching a terminal
    let clear = output::stdout_ansi();
    loop {
        // clear the screen and move to its top left corner
        if clear {
            print!("\x1B[2J\x1B[H");
        }
        // fetch and print our view, riding out transient errors
        match render().await {
            Ok(()) => (),
            Err(error) if is_transient(&error) => eprintln!("Error: {error}; retrying"),
            Err(error) => return Err(error),
        }
        // make sure this render is visible before we wait
        std::io::stdout()
            .flush()
            .map_err(|err| Error::new(format!("Failed to write output: {err}")))?;
        // wait before rendering again
        sleep(interval).await;
    }
}

/// Fetch and print a view once, until interrupted with `--watch`, or once a `--wait-for`
/// condition holds
///
/// # Arguments
///
/// * `format` - The format to print the view in
/// * `watch` - The `--watch` arguments
/// * `wait` - The `--wait-for` arguments if this command takes them
/// * `fetch` - Fetches the current view
pub async fn show<V, F, Fut>(
    format: OutputFormat,
    watch: &WatchArgs,
    wait: Option<&WaitForArgs>,
    mut fetch: F,
) -> Result<(), Error>
where
    V: View,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<V, Error>>,
{
    // wait for our condition and then print the view once
    if let Some(WaitForArgs {
        wait_for: Some(condition),
        timeout,
    }) = wait
    {
        let view = wait_until(
            &mut fetch,
            View::wait_items,
            condition,
            *timeout,
            watch.interval,
        )
        .await?;
        return output::print(&view, format);
    }
    // re-render our view until interrupted
    if watch.watch {
        return repeat(watch.interval, || {
            // start fetching our view now so the future doesn't borrow our closure
            let view = fetch();
            async move { output::print(&view.await?, format) }
        })
        .await;
    }
    // fetch and print our view once
    output::print(&fetch().await?, format)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::Duration;

    use serde::Serialize;
    use serde_json::json;
    use tokio::time::Instant;

    use thorium::Error;

    use super::{TIMEOUT_EXIT_CODE, WaitFor, exit_code, poll_until, repeat, wait_until};

    /// Conditions parse into a field path and an optional expected value
    #[test]
    fn parse_conditions() {
        // a field and value
        let cond: WaitFor = "status=Completed".parse().unwrap();
        assert_eq!(cond.path, vec!["status"]);
        assert_eq!(cond.expected.as_deref(), Some("Completed"));
        // a nested field
        let cond: WaitFor = "results.my-tool".parse().unwrap();
        assert_eq!(cond.path, vec!["results", "my-tool"]);
        assert_eq!(cond.expected, None);
        // the condition displays as written
        assert_eq!(cond.to_string(), "results.my-tool");
        // malformed conditions are rejected
        for bad in ["", "=Completed", "status=", "a..b"] {
            assert!(bad.parse::<WaitFor>().is_err(), "'{bad}' should not parse");
        }
    }

    /// Conditions check values, list membership, set fields, and dashed names
    #[test]
    fn conditions_hold() {
        // build an item like a reaction
        let item = json!({
            "status": "Completed",
            "instance_revision": 2,
            "tags": ["a", "b"],
            "ban": null,
            "results": {"tool": [1]},
        });
        // values compare without case for strings and by text for numbers
        assert!("status=completed".parse::<WaitFor>().unwrap().holds(&item));
        assert!(!"status=Failed".parse::<WaitFor>().unwrap().holds(&item));
        assert!(
            "instance-revision=2"
                .parse::<WaitFor>()
                .unwrap()
                .holds(&item)
        );
        // lists match on any element
        assert!("tags=b".parse::<WaitFor>().unwrap().holds(&item));
        // bare fields must be set
        assert!("results.tool".parse::<WaitFor>().unwrap().holds(&item));
        assert!(!"ban".parse::<WaitFor>().unwrap().holds(&item));
        assert!(!"missing".parse::<WaitFor>().unwrap().holds(&item));
        // every item must match, and there must be one
        let cond: WaitFor = "status=Completed".parse().unwrap();
        assert!(cond.holds_all(&[item.clone(), item.clone()]));
        assert!(!cond.holds_all(&[item, json!({"status": "Started"})]));
        assert!(!cond.holds_all(&[]));
    }

    /// Polling returns the value once the check holds
    #[tokio::test]
    async fn poll_until_holds() {
        // count our fetches and hold on the third
        let count = Cell::new(0);
        let fetch = || {
            count.set(count.get() + 1);
            let value = count.get();
            async move { Ok(value) }
        };
        let found = poll_until(
            fetch,
            |value: &i32| Ok(*value == 3),
            None,
            Duration::from_millis(1),
        )
        .await
        .unwrap();
        assert_eq!(found, Some(3));
    }

    /// Polling gives up at the timeout without waiting past it
    #[tokio::test]
    async fn poll_until_times_out() {
        // never hold and time out after 200 milliseconds
        let start = Instant::now();
        let found = poll_until(
            || async { Ok(0) },
            |_: &i32| Ok(false),
            Some(Duration::from_millis(200)),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert_eq!(found, None);
        // the long interval must not push us far past the timeout
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    /// An item the tests wait on
    #[derive(Serialize)]
    struct Status {
        /// The item's status
        status: &'static str,
    }

    /// Waiting returns once the condition holds, and a timeout sets the timeout exit code
    #[tokio::test]
    async fn wait_until_sets_the_timeout_exit_code() {
        // a condition that already holds returns the value
        let cond: WaitFor = "status=Completed".parse().unwrap();
        let items = |value: &Vec<Status>| {
            Ok(value
                .iter()
                .map(|item| serde_json::to_value(item).unwrap())
                .collect())
        };
        let done = wait_until(
            || async {
                Ok(vec![Status {
                    status: "Completed",
                }])
            },
            items,
            &cond,
            Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .await
        .unwrap();
        assert_eq!(done.len(), 1);
        // no timeout has happened yet
        assert_eq!(exit_code(), 1);
        // a condition that never holds times out
        let timed_out = wait_until(
            || async { Ok(vec![Status { status: "Started" }]) },
            items,
            &cond,
            Duration::from_millis(100),
            Duration::from_millis(10),
        )
        .await;
        assert!(timed_out.is_err());
        // the timeout makes Thorctl exit with the timeout code
        assert_eq!(exit_code(), TIMEOUT_EXIT_CODE);
    }

    /// Build an API error with a status code
    ///
    /// # Arguments
    ///
    /// * `code` - The status code of the error
    fn api_error(code: http::StatusCode) -> Error {
        Error::Thorium { code, msg: None }
    }

    /// Waiting re-reads at the given interval
    #[tokio::test]
    async fn wait_until_polls_at_the_interval() {
        // count the reads of a condition that never holds within half a second
        let count = Cell::new(0);
        let cond: WaitFor = "status=Completed".parse().unwrap();
        let fetch = || {
            count.set(count.get() + 1);
            async { Ok(vec![serde_json::json!({"status": "Started"})]) }
        };
        let timed_out = wait_until(
            fetch,
            |value: &Vec<serde_json::Value>| Ok(value.clone()),
            &cond,
            Duration::from_millis(500),
            Duration::from_millis(50),
        )
        .await;
        assert!(timed_out.is_err());
        // a 50 millisecond interval reads about ten times in half a second
        assert!(
            (7..=13).contains(&count.get()),
            "read {} times in 500ms at a 50ms interval",
            count.get()
        );
    }

    /// Polling retries transient errors and stops on client errors
    #[tokio::test]
    async fn poll_until_retries_transient_errors() {
        // fail with a server error once and then hold
        let count = Cell::new(0);
        let fetch = || {
            count.set(count.get() + 1);
            let first = count.get() == 1;
            async move {
                if first {
                    Err(api_error(http::StatusCode::SERVICE_UNAVAILABLE))
                } else {
                    Ok(count_value())
                }
            }
        };
        let found = poll_until(fetch, |_: &i32| Ok(true), None, Duration::from_millis(1))
            .await
            .unwrap();
        assert_eq!(found, Some(7));
        // a missing item stops polling right away
        let missing = poll_until(
            || async { Err::<i32, Error>(api_error(http::StatusCode::NOT_FOUND)) },
            |_: &i32| Ok(true),
            None,
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(
            missing.unwrap_err().status(),
            Some(http::StatusCode::NOT_FOUND)
        );
    }

    /// The value the retried fetch returns once it succeeds
    fn count_value() -> i32 {
        7
    }

    /// Watching keeps rendering after a transient error and stops on a client error
    #[tokio::test]
    async fn repeat_rides_out_transient_errors() {
        // fail the first render with a server error and count the renders after it
        let count = Cell::new(0);
        let render = || {
            count.set(count.get() + 1);
            let first = count.get() == 1;
            async move {
                if first {
                    Err(api_error(http::StatusCode::BAD_GATEWAY))
                } else {
                    Ok(())
                }
            }
        };
        let watching = tokio::time::timeout(
            Duration::from_millis(200),
            repeat(Duration::from_millis(10), render),
        )
        .await;
        // the watch was still running when we stopped it and rendered again after the error
        assert!(watching.is_err(), "the watch stopped on its own");
        assert!(count.get() >= 3, "rendered {} times", count.get());
        // a forbidden view stops the watch
        let stopped = repeat(Duration::from_millis(10), || async {
            Err(api_error(http::StatusCode::UNAUTHORIZED))
        })
        .await;
        assert_eq!(
            stopped.unwrap_err().status(),
            Some(http::StatusCode::UNAUTHORIZED)
        );
    }
}
