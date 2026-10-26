//! Handles scoped token commands

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Serialize;
use tabled::settings::Style;
use tabled::{Table, Tabled};
use thorium::client::conf::ActiveScopedToken;
use thorium::models::{RunTokenScope, ScopedToken, ScopedTokenRequest, ScopedTokenUpdate};
use thorium::utils::helpers::human_duration;
use thorium::{CtlConf, Error, Thorium};

use crate::args::Args;
use crate::args::tokens::{
    ActivateScopedToken, CreateScopedToken, CurrentScopedToken, DeleteScopedTokens, DescribeTokens,
    GetTokens, Tokens, UpdateScopedToken,
};
use crate::utils;
use crate::utils::output::{self, OutputFormat, REDACTED, View};
use crate::utils::watch;

/// Parse an optional expiration date with the given format
///
/// # Arguments
///
/// * `expires` - The raw expiration date to parse if one was given
/// * `date_fmt` - The chrono strftime format the date is in
fn parse_expires(expires: Option<&String>, date_fmt: &str) -> Result<Option<DateTime<Utc>>, Error> {
    // parse our expiration date if one was given
    match expires {
        Some(raw) => Ok(Some(
            NaiveDateTime::parse_from_str(raw, date_fmt)?.and_utc(),
        )),
        None => Ok(None),
    }
}

/// Get a human readable duration until a target timestamp
///
/// Returns `None` if the target timestamp has already passed. Only the two
/// most significant units are kept to keep output short (e.g. "2 months 29 days").
///
/// # Arguments
///
/// * `target` - The timestamp to get a human readable duration until
fn time_until(target: DateTime<Utc>) -> Option<String> {
    // get the time remaining until our target or bail if its in the past
    let remaining = (target - Utc::now()).to_std().ok()?;
    // truncate to whole seconds so we don't print ms/us/ns level detail
    let secs = remaining.as_secs();
    // convert our remaining time to a human readable string
    let human = human_duration(std::time::Duration::from_secs(secs));
    // keep only the two most significant units (each unit is 2 words)
    let short: Vec<&str> = human.split_whitespace().take(4).collect();
    Some(short.join(" "))
}

/// Get a human readable time until a scoped tokens value rotates
///
/// # Arguments
///
/// * `scoped` - The scoped token to get a refresh time for
fn refresh_time(scoped: &ScopedToken) -> String {
    // get the time until this scoped tokens value rotates
    time_until(scoped.token_expiration).unwrap_or_else(|| "now".to_owned())
}

/// Get a human readable time until a scoped token permanently expires
///
/// # Arguments
///
/// * `scoped` - The scoped token to get an expiration time for
fn expiration_time(scoped: &ScopedToken) -> String {
    // get the time until this scoped token permanently expires
    match scoped.expires {
        Some(expires) => time_until(expires).unwrap_or_else(|| "expired".to_owned()),
        None => "never".to_owned(),
    }
}

/// List the tool instances a run-scoped token may run
///
/// # Arguments
///
/// * `run` - The run the token is limited to
fn run_tools(run: &RunTokenScope) -> String {
    // join the tool instance ids
    run.tool_instances
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<String>>()
        .join(", ")
}

/// Print a scoped tokens info
///
/// The scoped tokens value is only printed when `show_token` is set.
///
/// # Arguments
///
/// * `scoped` - The scoped token to print
/// * `show_token` - Whether to show this scoped tokens value
fn print_token(scoped: &ScopedToken, show_token: bool) {
    // render and print this scoped token's info
    println!("{}", token_text(scoped, show_token));
}

/// Render a scoped token's info as text
///
/// The scoped token's value is only included when `show_token` is set.
///
/// # Arguments
///
/// * `scoped` - The scoped token to render
/// * `show_token` - Whether to include this scoped token's value
fn token_text(scoped: &ScopedToken, show_token: bool) -> String {
    // start with this scoped token's name
    let mut lines = vec![format!("{}:", scoped.name)];
    // add the groups this scoped token is limited to
    lines.push(format!("  groups: {}", scoped.groups.join(", ")));
    // add the time until this scoped token's value rotates
    lines.push(format!("  refresh: {}", refresh_time(scoped)));
    // add the time until this scoped token permanently expires
    lines.push(format!("  expires: {}", expiration_time(scoped)));
    // add the run a run-scoped token is limited to
    if let Some(run) = &scoped.run {
        lines.push(format!("  run: {}", run.reaction));
        lines.push(format!("  run tools: {}", run_tools(run)));
    }
    // only include this scoped token's value if it was requested
    if show_token {
        lines.push(format!("  token: {}", scoped.token));
    }
    lines.join("\n")
}

/// The scoped tokens one command shows, with every token value redacted
#[derive(Serialize)]
#[serde(transparent)]
struct TokensView(Vec<ScopedToken>);

impl TokensView {
    /// Build a view of scoped tokens, redacting each of their values
    ///
    /// # Arguments
    ///
    /// * `tokens` - The scoped tokens to show
    fn new(mut tokens: Vec<ScopedToken>) -> Self {
        // replace every token value so no format can print it
        for scoped in &mut tokens {
            REDACTED.clone_into(&mut scoped.token);
        }
        TokensView(tokens)
    }
}

impl View for TokensView {
    /// Render the scoped tokens as a table
    fn table(&self) -> String {
        output::table(self.0.iter().map(TokenRow::from))
    }

    /// Render the scoped tokens as text
    ///
    /// # Arguments
    ///
    /// * `_ansi` - Whether to style the text (scoped token text is never styled)
    fn human(&self, _ansi: bool) -> String {
        self.0
            .iter()
            .map(|scoped| token_text(scoped, false))
            .collect::<Vec<String>>()
            .join("\n")
    }
}

/// Limit scoped tokens to the requested names, erroring if any requested name is missing
///
/// All scoped tokens are kept when no names are requested.
///
/// # Arguments
///
/// * `tokens` - The scoped tokens to limit
/// * `names` - The names of the scoped tokens to keep
fn select_tokens(
    mut tokens: Vec<ScopedToken>,
    names: &[String],
) -> Result<Vec<ScopedToken>, Error> {
    // limit our tokens to the requested names if any were given
    if !names.is_empty() {
        // make sure all of the requested names exist
        for name in names {
            if !tokens.iter().any(|scoped| &scoped.name == name) {
                return Err(Error::new(format!("Scoped token {name} not found")));
            }
        }
        // drop any tokens that were not requested
        tokens.retain(|scoped| names.contains(&scoped.name));
    }
    Ok(tokens)
}

/// Write an updated Thorctl config to disk
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `config` - The config to write to disk
fn write_config(args: &Args, config: &CtlConf) -> Result<(), Error> {
    // open the config file for writing
    let conf_file = std::fs::File::create(&args.config)?;
    // write our updated config to disk
    serde_norway::to_writer(conf_file, config)?;
    Ok(())
}

/// Make sure this command was not run in `--keys` mode
///
/// Activation state lives in the Thorctl config file which does not exist
/// when authenticating with a raw keys file.
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
fn require_config(args: &Args) -> Result<(), Error> {
    // bail if a keys file is in use since we have no config file to update
    if args.keys.is_some() {
        return Err(Error::new(
            "Scoped token activation requires a Thorctl config file and cannot be used with --keys",
        ));
    }
    Ok(())
}

/// Create a new scoped token
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The create command to execute
async fn create(thorium: Thorium, cmd: &CreateScopedToken) -> Result<(), Error> {
    // parse our expiration date if one was given
    let expires = parse_expires(cmd.expires.as_ref(), &cmd.date_fmt)?;
    // build our scoped token request
    let mut req = ScopedTokenRequest::new(&cmd.name).groups(cmd.groups.clone());
    // set our expiration date if one was given
    if let Some(expires) = expires {
        req = req.expires(expires);
    }
    // create our scoped token
    let scoped = thorium.users.create_scoped_token(&req).await?;
    // print our new scoped tokens info
    print_token(&scoped, false);
    // tell the user how to see this scoped tokens value
    println!(
        "Run 'thorctl tokens describe {} --show-token' to see this scoped tokens value",
        scoped.name
    );
    Ok(())
}

/// A row in the scoped token table printed by the get command
#[derive(Tabled)]
struct TokenRow {
    /// The name of this scoped token
    #[tabled(rename = "NAME")]
    name: String,
    /// The human readable time until this scoped tokens value rotates
    #[tabled(rename = "REFRESH")]
    refresh: String,
    /// The human readable time until this scoped token permanently expires
    #[tabled(rename = "EXPIRATION")]
    expiration: String,
    /// The groups this scoped token is limited to
    #[tabled(rename = "GROUPS")]
    groups: String,
    /// The run a run-scoped token is limited to
    #[tabled(rename = "RUN")]
    run: String,
}

impl From<&ScopedToken> for TokenRow {
    /// Build a table row from a scoped token
    ///
    /// # Arguments
    ///
    /// * `scoped` - The scoped token to build a table row from
    fn from(scoped: &ScopedToken) -> Self {
        TokenRow {
            name: scoped.name.clone(),
            refresh: refresh_time(scoped),
            expiration: expiration_time(scoped),
            groups: scoped.groups.join(", "),
            run: scoped.run.as_ref().map_or_else(String::new, |run| {
                format!("{} ({})", run.reaction, run_tools(run))
            }),
        }
    }
}

/// Get all available scoped tokens, as a table unless another format, `--watch`, or `--wait-for`
/// is requested
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The get command to execute
async fn get(thorium: Thorium, cmd: &GetTokens) -> Result<(), Error> {
    // print through the shared output helpers if any of their flags were set
    if cmd.format.format.is_some() || cmd.watch.watch || cmd.wait.wait_for.is_some() {
        // default to a table
        let format = cmd.format.format.unwrap_or(OutputFormat::Table);
        return watch::show(format, &cmd.watch, Some(&cmd.wait), || async {
            Ok(TokensView::new(thorium.users.list_scoped_tokens().await?))
        })
        .await;
    }
    // list all of our scoped tokens
    let tokens = thorium.users.list_scoped_tokens().await?;
    // build a table row for each of our scoped tokens
    let rows: Vec<TokenRow> = tokens.iter().map(TokenRow::from).collect();
    // build and print our scoped token table
    println!("{}", Table::new(rows).with(Style::psql()));
    Ok(())
}

/// Describe scoped tokens by displaying all of their details, as JSON unless another format,
/// `--watch`, or `--wait-for` is requested
///
/// Token values are redacted unless `--show-token` is set.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The describe command to execute
async fn describe(thorium: Thorium, cmd: &DescribeTokens) -> Result<(), Error> {
    // print through the shared output helpers if any of their flags were set
    if cmd.format.format.is_some() || cmd.watch.watch || cmd.wait.wait_for.is_some() {
        // default to JSON
        let format = cmd.format.format.unwrap_or(OutputFormat::Json);
        return watch::show(format, &cmd.watch, Some(&cmd.wait), || async {
            let tokens = thorium.users.list_scoped_tokens().await?;
            Ok(TokensView::new(select_tokens(tokens, &cmd.names)?))
        })
        .await;
    }
    // list the requested scoped tokens
    let tokens = select_tokens(thorium.users.list_scoped_tokens().await?, &cmd.names)?;
    // serialize our scoped tokens redacting values if needed
    let mut details = Vec::with_capacity(tokens.len());
    for scoped in tokens {
        // serialize this scoped token
        let mut value = serde_json::to_value(&scoped)
            .map_err(|err| Error::new(format!("Failed to serialize scoped token: {err}")))?;
        // redact this scoped tokens value unless it was requested
        if !cmd.show_token {
            value["token"] = serde_json::Value::String(REDACTED.to_owned());
        }
        details.push(value);
    }
    // serialize our details in the requested format
    let raw = if cmd.condensed {
        serde_json::to_string(&details)
    } else {
        serde_json::to_string_pretty(&details)
    };
    // print our serialized details
    match raw {
        Ok(raw) => println!("{raw}"),
        Err(err) => {
            return Err(Error::new(format!(
                "Failed to serialize scoped tokens: {err}"
            )));
        }
    }
    Ok(())
}

/// Update a scoped token
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The update command to execute
async fn update(thorium: Thorium, cmd: &UpdateScopedToken) -> Result<(), Error> {
    // parse our expiration date if one was given
    let expires = parse_expires(cmd.opts.expires.as_ref(), &cmd.date_fmt)?;
    // build our scoped token update
    let mut update = ScopedTokenUpdate::default()
        .add_groups(cmd.opts.add_groups.clone())
        .remove_groups(cmd.opts.remove_groups.clone());
    // set our new expiration date if one was given
    if let Some(expires) = expires {
        update = update.expires(expires);
    }
    // clear our expiration date if requested
    if cmd.opts.clear_expires {
        update = update.clear_expires();
    }
    // update our scoped token
    let scoped = thorium
        .users
        .update_scoped_token(&cmd.name, &update)
        .await?;
    // print our updated scoped tokens info
    print_token(&scoped, false);
    Ok(())
}

/// Delete scoped tokens, asking first unless told not to
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The delete command to execute
async fn delete(thorium: Thorium, cmd: &DeleteScopedTokens) -> Result<(), Error> {
    // ask on the real terminal
    delete_asking(&thorium, cmd, &utils::Terminal).await
}

/// Delete scoped tokens, confirming with `asker` unless `--skip-confirm` was given
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The delete command to execute
/// * `asker` - Where the confirmation answer comes from
async fn delete_asking<A: utils::Asker>(
    thorium: &Thorium,
    cmd: &DeleteScopedTokens,
    asker: &A,
) -> Result<(), Error> {
    // deleting a scoped token can't be undone, so name exactly what goes before asking
    let question = match cmd.names.as_slice() {
        [name] => format!("Delete scoped token {name}?"),
        names => format!("Delete scoped tokens {}?", names.join(", ")),
    };
    if !utils::confirm_or_skip(asker, &question, "--skip-confirm", cmd.skip_confirm)? {
        return Ok(());
    }
    // track whether any deletes failed
    let mut failed = false;
    // try to delete each of the target scoped tokens
    for name in &cmd.names {
        match thorium.users.delete_scoped_token(name).await {
            // we deleted this scoped token
            Ok(_) => println!("Deleted scoped token {name}"),
            // we failed to delete this scoped token
            Err(error) => {
                // print this error and keep deleting the remaining tokens
                eprintln!("Failed to delete scoped token {name}: {error}");
                failed = true;
            }
        }
    }
    // error out if any deletes failed
    if failed {
        return Err(Error::new("Failed to delete one or more scoped tokens"));
    }
    Ok(())
}

/// Activate a scoped token making Thorctl authenticate with it
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The activate command to execute
pub async fn activate(args: &Args, cmd: &ActivateScopedToken) -> Result<(), Error> {
    // make sure we have a config file to store our activation in
    require_config(args)?;
    // load our config and instance a client that always uses our primary
    // credentials since scoped tokens cannot manage scoped tokens
    let (mut config, thorium) = utils::get_primary_client(args).await?;
    // warn about insecure connections if not set to skip
    if !config.skip_insecure_warning.unwrap_or_default() {
        utils::warn_insecure_conf(&config)?;
    }
    // get this scoped tokens info rotating its value if it has expired
    let scoped = thorium.users.get_scoped_token(&cmd.name).await?;
    // save this scoped token to our config
    config.scoped_token = Some(ActiveScopedToken {
        name: scoped.name.clone(),
        token: scoped.token.clone(),
    });
    // write our updated config to disk
    write_config(args, &config)?;
    // print this scoped tokens info
    print_token(&scoped, false);
    // tell the user this scoped token is now active
    println!(
        "Activated scoped token {}; Thorctl commands will now authenticate with it \
        until 'thorctl deactivate' is run",
        scoped.name
    );
    Ok(())
}

/// Deactivate the currently activated scoped token
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
pub fn deactivate(args: &Args) -> Result<(), Error> {
    // make sure we have a config file that could contain an activation
    require_config(args)?;
    // load our config from disk
    let mut config = CtlConf::from_path(&args.config)?;
    // check if a scoped token is currently active
    match config.scoped_token.take() {
        // a scoped token was active so clear it from our config
        Some(active) => {
            // write our updated config to disk
            write_config(args, &config)?;
            // tell the user this scoped token is no longer active
            println!(
                "Deactivated scoped token {}; Thorctl commands will now authenticate \
                with your primary credentials",
                active.name
            );
            Ok(())
        }
        // no scoped token is active so there is nothing to do
        None => {
            println!("No scoped token is currently active");
            Ok(())
        }
    }
}

/// Get info on the currently activated scoped token if one is active
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `config` - The Thorctl config to inspect
/// * `thorium` - The Thorium client
/// * `cmd` - The current command to execute
async fn current(
    args: &Args,
    config: CtlConf,
    thorium: Thorium,
    cmd: &CurrentScopedToken,
) -> Result<(), Error> {
    // make sure we have a config file that could contain an activation
    require_config(args)?;
    // check if a scoped token is currently active
    let Some(active) = &config.scoped_token else {
        println!("No scoped token is currently active");
        return Ok(());
    };
    // get this scoped tokens current info with our primary credentials
    match thorium.users.get_scoped_token(&active.name).await {
        Ok(scoped) => {
            // print this scoped tokens info
            print_token(&scoped, cmd.show_token);
            // warn if our stored value no longer matches the server side value
            if scoped.token != active.token {
                println!(
                    "WARNING: The activated value for {} is stale because this scoped token \
                    was rotated; rerun 'thorctl activate {}' to fix it",
                    active.name, active.name
                );
            }
            Ok(())
        }
        Err(error) => {
            // warn if this scoped token no longer exists server side
            if error.status() == Some(http::StatusCode::NOT_FOUND) {
                println!(
                    "The activated scoped token {} no longer exists; run \
                    'thorctl deactivate' to clear it",
                    active.name
                );
                return Ok(());
            }
            // some other error occurred so bubble it up
            Err(error)
        }
    }
}

/// Handle all tokens commands
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The tokens command to execute
pub async fn handle(args: &Args, cmd: &Tokens) -> Result<(), Error> {
    // load our config and instance a client that always uses our primary
    // credentials since scoped tokens cannot manage scoped tokens
    let (conf, thorium) = utils::get_primary_client(args).await?;
    // warn about insecure connections if not set to skip
    if !conf.skip_insecure_warning.unwrap_or_default() {
        utils::warn_insecure_conf(&conf)?;
    }
    // call the right tokens handler
    match cmd {
        Tokens::Create(cmd) => create(thorium, cmd).await,
        Tokens::Get(cmd) => get(thorium, cmd).await,
        Tokens::Update(cmd) => update(thorium, cmd).await,
        Tokens::Delete(cmd) => delete(thorium, cmd).await,
        Tokens::Describe(cmd) => describe(thorium, cmd).await,
        Tokens::Current(cmd) => current(args, conf, thorium, cmd).await,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use clap::Parser;
    use thorium::models::{RunTokenScope, ScopedToken, ScopedTokenRole};
    use uuid::Uuid;

    use super::{TokensView, delete_asking, select_tokens};
    use crate::args::tokens::DeleteScopedTokens;
    use crate::utils::output::{OutputFormat, REDACTED, render};
    use crate::utils::run_wait::testing::{client, mock};
    use crate::utils::testing::Scripted;

    /// Parse a `tokens delete` command
    ///
    /// # Arguments
    ///
    /// * `args` - The arguments after `tokens delete`
    fn delete_cmd(args: &[&str]) -> DeleteScopedTokens {
        // prepend the subcommand name and parse
        let mut full = vec!["delete"];
        full.extend_from_slice(args);
        DeleteScopedTokens::try_parse_from(full).unwrap()
    }

    /// Count the delete requests a mock API received
    ///
    /// # Arguments
    ///
    /// * `requests` - The mock's request log
    fn deletes(requests: &crate::utils::run_wait::testing::Requests) -> usize {
        // only the token deletes count
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(line, _)| line.starts_with("DELETE /api/users/tokens/"))
            .count()
    }

    /// Deleting asks first, naming every token, and an accepted question deletes them all
    #[tokio::test]
    async fn delete_asks_then_deletes_on_yes() {
        let (host, requests) = mock(|_, _| (200, String::new())).await;
        let thorium = client(&host).await;
        let asker = Scripted::answering(true);
        delete_asking(&thorium, &delete_cmd(&["one", "two"]), &asker)
            .await
            .unwrap();
        // one question that names both tokens, and both deleted
        let questions = asker.asked.borrow();
        assert_eq!(questions.len(), 1, "{questions:?}");
        assert!(
            questions[0].contains("one") && questions[0].contains("two"),
            "{questions:?}"
        );
        assert_eq!(deletes(&requests), 2);
    }

    /// A declined question deletes nothing and is not an error
    #[tokio::test]
    async fn delete_declined_changes_nothing() {
        let (host, requests) = mock(|_, _| (200, String::new())).await;
        let thorium = client(&host).await;
        let asker = Scripted::answering(false);
        delete_asking(&thorium, &delete_cmd(&["one"]), &asker)
            .await
            .unwrap();
        assert_eq!(asker.asked.borrow().len(), 1);
        assert_eq!(deletes(&requests), 0);
    }

    /// `--skip-confirm` and its `--yes` alias delete without asking
    #[tokio::test]
    async fn delete_skip_confirm_never_asks() {
        let (host, requests) = mock(|_, _| (200, String::new())).await;
        let thorium = client(&host).await;
        for flag in ["--skip-confirm", "--yes"] {
            let asker = Scripted::no_terminal();
            delete_asking(&thorium, &delete_cmd(&["one", flag]), &asker)
                .await
                .unwrap();
            assert!(asker.asked.borrow().is_empty());
        }
        assert_eq!(deletes(&requests), 2);
    }

    /// Without a terminal and without the flag the delete is refused, naming the flag
    #[tokio::test]
    async fn delete_without_a_terminal_is_refused() {
        let (host, requests) = mock(|_, _| (200, String::new())).await;
        let thorium = client(&host).await;
        let asker = Scripted::no_terminal();
        let err = delete_asking(&thorium, &delete_cmd(&["one"]), &asker)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("--skip-confirm"), "{err}");
        assert_eq!(deletes(&requests), 0);
    }

    /// Build a scoped token holding a known secret value
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the scoped token
    fn scoped(name: &str) -> ScopedToken {
        ScopedToken {
            name: name.to_owned(),
            owner: "owner".to_owned(),
            role: ScopedTokenRole::User,
            groups: vec!["corn".to_owned()],
            token: "super-secret-token-value".to_owned(),
            token_expiration: Utc::now() + Duration::days(1),
            expires: None,
            run: None,
        }
    }

    /// Every format shows a run token's run and tool instances and its expiry but never its
    /// value, and an ordinary scoped token shows no run
    #[test]
    fn run_tokens_show_their_run() {
        // a run token next to an ordinary scoped token
        let reaction = Uuid::new_v4();
        let tool = Uuid::new_v4();
        let mut run = scoped(&format!("run-{reaction}"));
        run.expires = Some(run.token_expiration);
        run.run = Some(RunTokenScope {
            reaction,
            tool_instances: vec![tool],
        });
        let view = TokensView::new(vec![run, scoped("plain")]);
        for format in [
            OutputFormat::Table,
            OutputFormat::Human,
            OutputFormat::Json,
            OutputFormat::Yaml,
        ] {
            // render our view in this format
            let rendered = render(&view, format, false).unwrap();
            // the run and its tool show but the token value never does
            assert!(
                rendered.contains(&tool.to_string()),
                "{format:?}: {rendered}"
            );
            assert!(
                rendered.matches(&reaction.to_string()).count() >= 2,
                "{format:?} showed no run: {rendered}"
            );
            assert!(!rendered.contains("super-secret-token-value"));
        }
        // the JSON document has a run section only for the run token
        let json: serde_json::Value =
            serde_json::from_str(&render(&view, OutputFormat::Json, false).unwrap()).unwrap();
        assert_eq!(json[0]["run"]["reaction"], reaction.to_string());
        assert!(json[0]["expires"].is_string());
        assert!(json[1].get("run").is_none());
        // the text for the plain token has no run line
        let plain = render(
            &TokensView::new(vec![scoped("plain")]),
            OutputFormat::Human,
            false,
        )
        .unwrap();
        assert!(!plain.contains("run"), "{plain}");
    }

    /// No format of a scoped token view contains a token value
    #[test]
    fn no_format_contains_a_token() {
        // build a view of two scoped tokens
        let view = TokensView::new(vec![scoped("one"), scoped("two")]);
        for format in [
            OutputFormat::Table,
            OutputFormat::Human,
            OutputFormat::Json,
            OutputFormat::Yaml,
        ] {
            // render our view in this format
            let rendered = render(&view, format, false).unwrap();
            // the token value never appears but the token names do
            assert!(
                !rendered.contains("super-secret-token-value"),
                "{format:?} printed a token"
            );
            assert!(rendered.contains("one") && rendered.contains("two"));
        }
        // the JSON document keeps the response's fields with the value redacted
        let json: serde_json::Value =
            serde_json::from_str(&render(&view, OutputFormat::Json, false).unwrap()).unwrap();
        assert_eq!(json[0]["token"], REDACTED);
        assert_eq!(json[0]["owner"], "owner");
    }

    /// Selecting names keeps only those tokens and rejects missing names
    #[test]
    fn select_names() {
        // build three scoped tokens
        let tokens = vec![scoped("a"), scoped("b"), scoped("c")];
        // no names keeps them all
        assert_eq!(select_tokens(tokens.clone(), &[]).unwrap().len(), 3);
        // names keep only those tokens
        let picked = select_tokens(tokens.clone(), &["b".to_owned()]).unwrap();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].name, "b");
        // a missing name is an error
        assert!(select_tokens(tokens, &["z".to_owned()]).is_err());
    }
}
