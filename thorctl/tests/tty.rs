//! The interactive behavior of the real thorctl binary on a pseudo-terminal
//!
//! None of these need a real API: each runs a prompt or an editor that opens before any request is
//! sent, or against the small [`MockApi`] below, and checks what the terminal and the temporary
//! directory look like when thorctl is interrupted, refused a terminal, or given end of input.

#![cfg(unix)]

#[path = "common/editor.rs"]
mod editor;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use editor::Pty;

/// How long to wait for a prompt or for thorctl to exit
const WAIT: Duration = Duration::from_secs(30);

/// The thorctl binary under test
fn thorctl() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_thorctl"))
}

/// A scratch directory standing in for the home and the temp dir of one run
struct Scratch {
    /// The directory
    dir: std::path::PathBuf,
}

impl Scratch {
    /// Make a fresh scratch directory
    fn new() -> Self {
        // a unique directory under the system temp dir
        let dir = std::env::temp_dir().join(format!("thorctl-tty-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("tmp")).expect("create the scratch directory");
        Scratch { dir }
    }

    /// The path of the config file the run uses
    fn config(&self) -> String {
        self.dir.join("config.yml").display().to_string()
    }

    /// The temp dir thorctl is told to use
    fn tmp(&self) -> String {
        self.dir.join("tmp").display().to_string()
    }

    /// The environment that keeps a run inside this directory
    fn envs(&self) -> Vec<(&'static str, String)> {
        vec![
            ("HOME", self.dir.display().to_string()),
            ("TMPDIR", self.tmp()),
        ]
    }

    /// The edit directories thorctl left in the temp dir
    fn edit_dirs(&self) -> Vec<String> {
        // list the entries thorctl names for an edit session
        std::fs::read_dir(self.dir.join("tmp"))
            .expect("read the temp dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("thorium-edit-"))
            .collect()
    }
}

impl Drop for Scratch {
    /// Remove the directory
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Start thorctl on a pseudo-terminal inside a scratch directory
///
/// # Arguments
///
/// * `scratch` - The directory the run lives in
/// * `args` - The thorctl arguments after the global ones
fn spawn(scratch: &Scratch, args: &[&str]) -> Pty {
    // the global flags keep the run away from the real config and the update check
    let config = scratch.config();
    let mut all = vec!["--config", &config, "--skip-update"];
    all.extend_from_slice(args);
    let envs = scratch.envs();
    let envs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    Pty::spawn(thorctl(), &all, &envs).expect("start thorctl on a terminal")
}

/// Run thorctl without a terminal, with stdin from `/dev/null`, and return its exit code and text
///
/// # Arguments
///
/// * `scratch` - The directory the run lives in
/// * `args` - The thorctl arguments after the global ones
fn run_without_terminal(scratch: &Scratch, args: &[&str]) -> (Option<i32>, String) {
    // a session of its own, so there is no controlling terminal to fall back to
    let mut command = Command::new(thorctl());
    command
        .args(["--config", &scratch.config(), "--skip-update"])
        .args(args)
        .env("HOME", &scratch.dir)
        .env("TMPDIR", scratch.tmp())
        .stdin(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut command, || {
            libc::setsid();
            Ok(())
        });
    }
    let output = command.output().expect("run thorctl");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.code(), text)
}

/// Whether the cursor is visible at the end of the output (the last cursor sequence shows it)
///
/// # Arguments
///
/// * `output` - Everything the program wrote to the terminal
fn cursor_visible(output: &str) -> bool {
    // a prompt hides the cursor and draws it again when it is done
    output.rfind("\x1b[?25h") > output.rfind("\x1b[?25l")
}

/// Whether a process ended the way Ctrl-C ends it: by `SIGINT`, or with the interrupted status 130
///
/// The password prompt reads Ctrl-C as a key press when it has switched the terminal to raw mode
/// and as a signal when it has not yet, so either ending is the interrupt.
///
/// # Arguments
///
/// * `status` - How the process ended
fn interrupted(status: std::process::ExitStatus) -> bool {
    // a signal death or the shell convention for one
    status.signal() == Some(libc::SIGINT) || status.code() == Some(130)
}

/// Ctrl-C at the password prompt ends thorctl as interrupted and leaves the terminal echoing
#[test]
fn ctrl_c_at_the_password_prompt_restores_echo() {
    let scratch = Scratch::new();
    let mut pty = spawn(
        &scratch,
        &["login", "http://127.0.0.1:1", "--user", "nobody"],
    );
    pty.read_until("Password: ", WAIT).unwrap();
    // the prompt turns echo off while the password is typed (give it a moment to switch)
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !pty.echo_enabled(),
        "echo should be off at the password prompt"
    );
    pty.send("\x03").unwrap();
    let status = pty.finish(WAIT).unwrap();
    assert!(interrupted(status), "{status:?}\n{}", pty.output());
    assert!(pty.echo_enabled(), "echo left off after Ctrl-C");
}

/// Ctrl-C at a confirmation prompt ends thorctl as interrupted and shows the cursor again
#[test]
fn ctrl_c_at_a_confirmation_prompt_shows_the_cursor() {
    let scratch = Scratch::new();
    // trusting invalid certificates makes login ask `Continue?` before it connects
    let mut pty = spawn(
        &scratch,
        &[
            "login",
            "http://127.0.0.1:1",
            "-u",
            "u",
            "-p",
            "p",
            "--invalid-certs",
            "true",
        ],
    );
    pty.read_until("Continue?", WAIT).unwrap();
    pty.send("\x03").unwrap();
    let status = pty.finish(WAIT).unwrap();
    assert!(interrupted(status), "{status:?}\n{}", pty.output());
    assert!(
        cursor_visible(pty.output()),
        "cursor left hidden: {:?}",
        pty.output()
    );
}

/// Ctrl-C at an input prompt of `toolbox init` ends thorctl as interrupted, shows the cursor and writes nothing
#[test]
fn ctrl_c_at_an_init_prompt_shows_the_cursor() {
    let scratch = Scratch::new();
    let dir = scratch.dir.join("toolbox");
    let dir = dir.display().to_string();
    let mut pty = spawn(
        &scratch,
        &[
            "toolbox",
            "init",
            "toolbox",
            "-i",
            "one",
            "--toolbox-dir",
            &dir,
            "--editor",
            "true",
        ],
    );
    pty.read_until("Group name: ", WAIT).unwrap();
    pty.send("\x03").unwrap();
    let status = pty.finish(WAIT).unwrap();
    assert!(interrupted(status), "{status:?}\n{}", pty.output());
    assert!(
        cursor_visible(pty.output()),
        "cursor left hidden: {:?}",
        pty.output()
    );
    assert!(!Path::new(&dir).join("config.toml").exists());
}

/// Without a terminal, login asks for `--password` instead of dumping an operating system error
#[test]
fn login_without_a_terminal_asks_for_the_password_flag() {
    let scratch = Scratch::new();
    let (code, text) =
        run_without_terminal(&scratch, &["login", "http://127.0.0.1:1", "-u", "nobody"]);
    assert_eq!(code, Some(1), "{text}");
    assert!(text.contains("--password"), "{text}");
    assert!(!text.contains("Os {"), "raw error shown: {text}");
}

/// End of input at the username prompt is an error, not an empty username
#[test]
fn end_of_input_at_the_username_prompt_is_an_error() {
    let scratch = Scratch::new();
    let (code, text) =
        run_without_terminal(&scratch, &["login", "http://127.0.0.1:1", "-p", "secret"]);
    assert_eq!(code, Some(1), "{text}");
    assert!(text.contains("No username given"), "{text}");
}

/// End of input at the password prompt is an error with a message, not a debug dump
#[test]
fn ctrl_d_at_the_password_prompt_is_an_error() {
    let scratch = Scratch::new();
    let mut pty = spawn(
        &scratch,
        &["login", "http://127.0.0.1:1", "--user", "nobody"],
    );
    pty.read_until("Password: ", WAIT).unwrap();
    // the prompt flushes input typed before it switches the terminal mode
    std::thread::sleep(Duration::from_millis(300));
    pty.send("\x04").unwrap();
    let status = pty.finish(WAIT).unwrap();
    assert_eq!(status.code(), Some(1), "{}", pty.output());
    assert!(
        pty.output().contains("No password given"),
        "{}",
        pty.output()
    );
    assert!(pty.echo_enabled());
}

/// Without a terminal, the insecure-connection question is refused with a clear message
#[test]
fn insecure_warning_without_a_terminal_is_refused_clearly() {
    let scratch = Scratch::new();
    let (code, text) = run_without_terminal(
        &scratch,
        &[
            "login",
            "http://127.0.0.1:1",
            "-u",
            "u",
            "-p",
            "p",
            "--invalid-certs",
            "true",
        ],
    );
    assert_eq!(code, Some(1), "{text}");
    assert!(text.contains("No terminal available"), "{text}");
    assert!(!text.contains("Dialoguer"), "raw error shown: {text}");
}

/// A script that stands in for an editor: it records that it started and then waits
///
/// # Arguments
///
/// * `scratch` - The directory the run lives in
/// * `body` - What the script does after recording its start
fn waiting_editor(scratch: &Scratch, body: &str) -> String {
    // the marker file tells the test the editor is open (and the edit directory exists)
    let marker = scratch.dir.join("editor-started");
    let path = scratch.dir.join("editor.sh");
    std::fs::write(
        &path,
        format!("#!/bin/sh\ntouch {}\n{body}\n", marker.display()),
    )
    .expect("write the editor script");
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("make the editor script executable");
    path.display().to_string()
}

/// Wait for the editor script to report that it is open
///
/// # Arguments
///
/// * `scratch` - The directory the run lives in
fn wait_for_editor(scratch: &Scratch) {
    // poll for the marker the script writes first
    let marker = scratch.dir.join("editor-started");
    for _ in 0..300 {
        if marker.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the editor never started");
}

/// SIGTERM while an editor is open removes the edit directory and ends thorctl by that signal
#[test]
fn sigterm_while_editing_removes_the_edit_directory() {
    let scratch = Scratch::new();
    let editor = waiting_editor(&scratch, "sleep 30");
    let dir = scratch.dir.join("img").display().to_string();
    let mut pty = spawn(
        &scratch,
        &[
            "toolbox", "init", "image", &dir, "-g", "grp", "--editor", &editor,
        ],
    );
    wait_for_editor(&scratch);
    assert_eq!(
        scratch.edit_dirs().len(),
        1,
        "the edit directory should exist while editing"
    );
    pty.signal(libc::SIGTERM).unwrap();
    let status = pty.finish(WAIT).unwrap();
    assert_eq!(status.signal(), Some(libc::SIGTERM), "{}", pty.output());
    assert!(
        scratch.edit_dirs().is_empty(),
        "edit directory left behind: {:?}",
        scratch.edit_dirs()
    );
}

/// SIGINT sent to thorctl alone is left to the editor: thorctl keeps waiting and finishes normally
#[test]
fn sigint_while_editing_is_left_to_the_editor() {
    let scratch = Scratch::new();
    // the editor finishes by itself after the signal has had time to arrive
    let editor = waiting_editor(&scratch, "sleep 2");
    let dir = scratch.dir.join("img").display().to_string();
    let mut pty = spawn(
        &scratch,
        &[
            "toolbox", "init", "image", &dir, "-g", "grp", "--editor", &editor,
        ],
    );
    wait_for_editor(&scratch);
    pty.signal(libc::SIGINT).unwrap();
    let status = pty.finish(WAIT).unwrap();
    assert!(
        status.success(),
        "thorctl died: {status:?}\n{}",
        pty.output()
    );
    assert!(
        scratch.edit_dirs().is_empty(),
        "the edit directory was left behind"
    );
}

/// A signal the parent ignored stays ignored, as for a job started with `nohup` or `&`
#[test]
fn an_ignored_signal_stays_ignored() {
    let scratch = Scratch::new();
    let editor = waiting_editor(&scratch, "sleep 2");
    let dir = scratch.dir.join("img").display().to_string();
    let config = scratch.config();
    // the shell ignores SIGTERM and execs thorctl, which inherits the setting
    let script = "trap '' TERM; exec \"$0\" \"$@\"";
    let args = [
        "-c",
        script,
        thorctl().to_str().unwrap(),
        "--config",
        &config,
        "--skip-update",
        "toolbox",
        "init",
        "image",
        &dir,
        "-g",
        "grp",
        "--editor",
        &editor,
    ];
    let envs = scratch.envs();
    let envs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut pty = Pty::spawn(Path::new("/bin/sh"), &args, &envs).unwrap();
    wait_for_editor(&scratch);
    pty.signal(libc::SIGTERM).unwrap();
    let status = pty.finish(WAIT).unwrap();
    assert!(
        status.success(),
        "thorctl died from an ignored signal: {status:?}\n{}",
        pty.output()
    );
}

/// A tiny HTTP server that answers each request from a router and records the request lines
struct MockApi {
    /// The url the server listens at
    url: String,
    /// The request line of every request received, in order
    requests: Arc<Mutex<Vec<String>>>,
}

impl MockApi {
    /// Start the server on a free local port
    ///
    /// # Arguments
    ///
    /// * `router` - Picks the status and body of each answer from the request line
    fn start<F>(router: F) -> Self
    where
        F: Fn(&str) -> (u16, String) + Send + 'static,
    {
        // bind to a free port and answer one connection at a time on a background thread
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the mock API");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                // read the request head; none of the routes used here has a body
                let mut head = Vec::new();
                let mut chunk = [0_u8; 4096];
                while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => head.extend_from_slice(&chunk[..read]),
                    }
                }
                let text = String::from_utf8_lossy(&head).to_string();
                let line = text.lines().next().unwrap_or_default().to_owned();
                log.lock().unwrap().push(line.clone());
                // answer from the router and close the connection
                let (code, body) = router(&line);
                let reply = format!(
                    "HTTP/1.1 {code} Mock\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        MockApi { url, requests }
    }

    /// How many requests started with `prefix`
    ///
    /// # Arguments
    ///
    /// * `prefix` - What the request lines start with (for example `DELETE /api/users`)
    fn count(&self, prefix: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with(prefix))
            .count()
    }
}

impl Scratch {
    /// Write a config that points thorctl at a mock API with a token
    ///
    /// # Arguments
    ///
    /// * `api` - The mock API to point at
    fn point_at(&self, api: &MockApi) {
        std::fs::write(
            self.dir.join("config.yml"),
            format!("keys:\n  api: {}\n  token: token\n", api.url),
        )
        .expect("write the config");
    }
}

/// A network policy shared by the groups corn and wheat
fn shared_policy() -> String {
    // the policy the API returns for `allow-dns`
    let policy = thorium::models::NetworkPolicy {
        name: "allow-dns".to_owned(),
        id: uuid::Uuid::nil(),
        k8s_name: format!("allow-dns-{}", uuid::Uuid::nil()),
        groups: vec!["corn".to_owned(), "wheat".to_owned()],
        created: chrono::Utc::now(),
        ingress: None,
        egress: None,
        forced_policy: false,
        default_policy: false,
        used_by: std::collections::HashMap::new(),
    };
    serde_json::to_string(&policy).unwrap()
}

/// The reaction the API returns for a started run
///
/// # Arguments
///
/// * `id` - The reaction's id
fn started_run(id: uuid::Uuid) -> String {
    use thorium::models::{
        PlannedStage, Reaction, ReactionPlan, ReactionSource, ReactionStatus, RunProvenance,
    };
    // a started tool run in the corn group
    let tool_instance = uuid::Uuid::new_v4();
    let run = Reaction {
        id,
        group: "corn".to_owned(),
        creator: "mcarson".to_owned(),
        pipeline: "harvester".to_owned(),
        status: ReactionStatus::Started,
        current_stage: 0,
        current_stage_progress: 0,
        current_stage_length: 1,
        args: std::collections::HashMap::default(),
        sla: chrono::Utc::now(),
        jobs: Vec::new(),
        tags: Vec::new(),
        parent: None,
        sub_reactions: 0,
        completed_sub_reactions: 0,
        generators: Vec::new(),
        samples: vec!["63b9c2cbd4f4f1f6".to_owned()],
        ephemeral: Vec::new(),
        parent_ephemeral: std::collections::HashMap::default(),
        repos: Vec::new(),
        trigger_depth: None,
        has_cache: false,
        source: ReactionSource::Tool {
            tool_instance,
            provenance: RunProvenance::default(),
        },
        plan: ReactionPlan {
            stages: vec![vec![PlannedStage {
                name: "harvester".to_owned(),
                tool_instance,
            }]],
        },
        run_token: None,
        run: None,
    };
    serde_json::to_string(&run).unwrap()
}

/// A mock API that holds a token, a network policy, and a started run, and accepts their removal
///
/// # Arguments
///
/// * `run` - The id of the started run
fn confirm_api(run: uuid::Uuid) -> MockApi {
    MockApi::start(move |line| {
        if line.starts_with("DELETE /api/users/tokens/") {
            (200, String::new())
        } else if line.starts_with("GET /api/network-policies/allow-dns") {
            (200, shared_policy())
        } else if line.starts_with("DELETE /api/network-policies/allow-dns") {
            (204, String::new())
        } else if line.starts_with(&format!("GET /api/reactions/corn/{run}")) {
            (200, started_run(run))
        } else if line.contains("/handle/corn/") && line.contains("/fail") {
            (200, r#"{"status":"Failed"}"#.to_owned())
        } else {
            (404, r#"{"error":"not found"}"#.to_owned())
        }
    })
}

/// The line thorctl prints when a confirmation is answered no
const DECLINED: &str = "Cancelled; nothing was changed.";

/// Whether a SGR color sequence appears in the output
///
/// # Arguments
///
/// * `output` - Everything the program wrote to the terminal
fn has_color(output: &str) -> bool {
    // a color sequence is an escape, a bracket, digits and semicolons, and an `m`
    let bytes = output.as_bytes();
    (0..bytes.len().saturating_sub(2)).any(|start| {
        bytes[start] == 0x1b && bytes[start + 1] == b'[' && {
            let rest = &bytes[start + 2..];
            let end = rest
                .iter()
                .position(|byte| !(byte.is_ascii_digit() || *byte == b';'))
                .unwrap_or(rest.len());
            rest.get(end) == Some(&b'm')
        }
    })
}

/// Start `tokens delete one` on a terminal against the mock API
///
/// # Arguments
///
/// * `scratch` - The directory the run lives in
/// * `api` - The mock API the config points at
/// * `envs` - Extra environment variables for the run
fn spawn_token_delete(scratch: &Scratch, api: &MockApi, envs: &[(&str, &str)]) -> Pty {
    scratch.point_at(api);
    let config = scratch.config();
    let all = [
        "--config",
        &config,
        "--skip-update",
        "tokens",
        "delete",
        "one",
    ];
    let mut vars = scratch.envs();
    vars.extend(envs.iter().map(|(key, value)| (*key, (*value).to_owned())));
    let vars: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
    Pty::spawn(thorctl(), &all, &vars).expect("start thorctl on a terminal")
}

/// Answering y at the `tokens delete` prompt deletes the token
#[test]
fn tokens_delete_yes_deletes() {
    let scratch = Scratch::new();
    let api = confirm_api(uuid::Uuid::new_v4());
    let mut pty = spawn_token_delete(&scratch, &api, &[]);
    pty.read_until("Delete scoped token one?", WAIT).unwrap();
    pty.send("y").unwrap();
    let status = pty.finish(WAIT).unwrap();
    assert_eq!(status.code(), Some(0), "{}", pty.output());
    assert!(
        pty.output().contains("Deleted scoped token one"),
        "{}",
        pty.output()
    );
    assert_eq!(api.count("DELETE /api/users/tokens/one"), 1);
    assert!(cursor_visible(pty.output()));
}

/// Answering n, or just Enter (the default is no), keeps the token and says nothing was changed
#[test]
fn tokens_delete_no_and_enter_keep_the_token() {
    for key in ["n", "\r"] {
        let scratch = Scratch::new();
        let api = confirm_api(uuid::Uuid::new_v4());
        let mut pty = spawn_token_delete(&scratch, &api, &[]);
        pty.read_until("Delete scoped token one?", WAIT).unwrap();
        pty.send(key).unwrap();
        let status = pty.finish(WAIT).unwrap();
        assert_eq!(status.code(), Some(0), "{}", pty.output());
        assert!(pty.output().contains(DECLINED), "{}", pty.output());
        assert_eq!(api.count("DELETE "), 0);
    }
}

/// Ctrl-C at the `tokens delete` prompt keeps the token, ends as interrupted, shows the cursor
#[test]
fn tokens_delete_ctrl_c_restores_the_terminal() {
    let scratch = Scratch::new();
    let api = confirm_api(uuid::Uuid::new_v4());
    let mut pty = spawn_token_delete(&scratch, &api, &[]);
    pty.read_until("Delete scoped token one?", WAIT).unwrap();
    pty.send("\x03").unwrap();
    let status = pty.finish(WAIT).unwrap();
    assert!(interrupted(status), "{status:?}\n{}", pty.output());
    assert!(cursor_visible(pty.output()), "{:?}", pty.output());
    assert!(pty.echo_enabled());
    assert_eq!(api.count("DELETE "), 0);
}

/// The prompt works on a dumb terminal like the other delete prompts, and draws no colors on one or with `NO_COLOR`
#[test]
fn tokens_delete_prompt_honors_term_dumb_and_no_color() {
    // TERM=dumb
    let scratch = Scratch::new();
    let api = confirm_api(uuid::Uuid::new_v4());
    let mut pty = spawn_token_delete(&scratch, &api, &[("TERM", "dumb")]);
    pty.read_until("Delete scoped token one?", WAIT).unwrap();
    pty.send("y").unwrap();
    let status = pty.finish(WAIT).unwrap();
    assert_eq!(status.code(), Some(0), "{}", pty.output());
    assert!(
        !has_color(pty.output()),
        "colors on a dumb terminal: {:?}",
        pty.output()
    );
    assert_eq!(api.count("DELETE /api/users/tokens/one"), 1);
    // NO_COLOR
    let scratch = Scratch::new();
    let api = confirm_api(uuid::Uuid::new_v4());
    let mut pty = spawn_token_delete(&scratch, &api, &[("NO_COLOR", "1")]);
    pty.read_until("Delete scoped token one?", WAIT).unwrap();
    pty.send("n").unwrap();
    let status = pty.finish(WAIT).unwrap();
    assert_eq!(status.code(), Some(0), "{}", pty.output());
    assert!(
        !has_color(pty.output()),
        "colors with NO_COLOR: {:?}",
        pty.output()
    );
    assert!(pty.output().contains(DECLINED), "{}", pty.output());
}

/// Without a terminal `tokens delete` is refused naming the flag, and the flag deletes
#[test]
fn tokens_delete_without_a_terminal_needs_the_flag() {
    let scratch = Scratch::new();
    let api = confirm_api(uuid::Uuid::new_v4());
    scratch.point_at(&api);
    let (code, text) = run_without_terminal(&scratch, &["tokens", "delete", "one"]);
    assert_eq!(code, Some(1), "{text}");
    assert!(text.contains("--skip-confirm"), "{text}");
    assert!(
        !text.contains("Dialoguer") && !text.contains("Os {"),
        "raw error: {text}"
    );
    assert_eq!(api.count("DELETE "), 0);
    // the flag (or its --yes alias) deletes without a terminal
    let (code, text) = run_without_terminal(&scratch, &["tokens", "delete", "one", "--yes"]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(api.count("DELETE /api/users/tokens/one"), 1);
}

/// The `network-policies delete` prompt names the policy and the groups that hold it
#[test]
fn network_policy_delete_prompt_names_the_groups() {
    let scratch = Scratch::new();
    let api = confirm_api(uuid::Uuid::new_v4());
    scratch.point_at(&api);
    let config = scratch.config();
    let args = [
        "--config",
        &config,
        "--skip-update",
        "network-policies",
        "delete",
        "allow-dns",
    ];
    let envs = scratch.envs();
    let envs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    // y deletes
    let mut pty = Pty::spawn(thorctl(), &args, &envs).unwrap();
    pty.read_until("Delete network policy 'allow-dns'", WAIT)
        .unwrap();
    pty.read_until("?", WAIT).unwrap();
    assert!(
        pty.output().contains("corn, wheat"),
        "groups missing from the prompt: {}",
        pty.output()
    );
    pty.send("y").unwrap();
    let status = pty.finish(WAIT).unwrap();
    assert_eq!(status.code(), Some(0), "{}", pty.output());
    assert_eq!(api.count("DELETE /api/network-policies/allow-dns"), 1);
    // n keeps it and says nothing was changed
    let mut pty = Pty::spawn(thorctl(), &args, &envs).unwrap();
    pty.read_until("Delete network policy 'allow-dns'", WAIT)
        .unwrap();
    pty.send("n").unwrap();
    let status = pty.finish(WAIT).unwrap();
    assert_eq!(status.code(), Some(0), "{}", pty.output());
    assert!(pty.output().contains(DECLINED), "{}", pty.output());
    assert_eq!(api.count("DELETE /api/network-policies/allow-dns"), 1);
    // Ctrl-C leaves it alone and restores the cursor
    let mut pty = Pty::spawn(thorctl(), &args, &envs).unwrap();
    pty.read_until("Delete network policy 'allow-dns'", WAIT)
        .unwrap();
    pty.send("\x03").unwrap();
    let status = pty.finish(WAIT).unwrap();
    assert!(interrupted(status), "{status:?}\n{}", pty.output());
    assert!(cursor_visible(pty.output()), "{:?}", pty.output());
    assert_eq!(api.count("DELETE /api/network-policies/allow-dns"), 1);
}

/// Without a terminal `network-policies delete` is refused naming the flag, and the flag deletes
#[test]
fn network_policy_delete_without_a_terminal_needs_the_flag() {
    let scratch = Scratch::new();
    let api = confirm_api(uuid::Uuid::new_v4());
    scratch.point_at(&api);
    let (code, text) = run_without_terminal(&scratch, &["network-policies", "delete", "allow-dns"]);
    assert_eq!(code, Some(1), "{text}");
    assert!(text.contains("--skip-confirm"), "{text}");
    assert_eq!(api.count("DELETE "), 0);
    let (code, text) = run_without_terminal(
        &scratch,
        &["network-policies", "delete", "allow-dns", "--skip-confirm"],
    );
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(api.count("DELETE /api/network-policies/allow-dns"), 1);
}

/// Answering n to `runs cancel` and `reactions fail` says nothing was changed and exits 0
#[test]
fn cancel_and_fail_say_when_nothing_was_changed() {
    let run = uuid::Uuid::new_v4();
    for command in [["runs", "cancel"], ["reactions", "fail"]] {
        let scratch = Scratch::new();
        let api = confirm_api(run);
        scratch.point_at(&api);
        let config = scratch.config();
        let id = run.to_string();
        let args = [
            "--config",
            &config,
            "--skip-update",
            command[0],
            command[1],
            &id,
            "-g",
            "corn",
        ];
        let envs = scratch.envs();
        let envs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
        // n declines with a message
        let mut pty = Pty::spawn(thorctl(), &args, &envs).unwrap();
        pty.read_until("Fail run", WAIT).unwrap();
        pty.send("n").unwrap();
        let status = pty.finish(WAIT).unwrap();
        assert_eq!(status.code(), Some(0), "{command:?}\n{}", pty.output());
        assert!(
            pty.output().contains(DECLINED),
            "{command:?}\n{}",
            pty.output()
        );
        assert_eq!(api.count("PATCH ") + api.count("POST "), 0);
        // y fails the run
        let mut pty = Pty::spawn(thorctl(), &args, &envs).unwrap();
        pty.read_until("Fail run", WAIT).unwrap();
        pty.send("y").unwrap();
        let status = pty.finish(WAIT).unwrap();
        assert_eq!(status.code(), Some(0), "{command:?}\n{}", pty.output());
        assert!(
            pty.output().contains("is now Failed"),
            "{command:?}\n{}",
            pty.output()
        );
        // no terminal: refused with exit 1 naming the flag
        let (code, text) =
            run_without_terminal(&scratch, &[command[0], command[1], &id, "-g", "corn"]);
        assert_eq!(code, Some(1), "{command:?}\n{text}");
        assert!(text.contains("--skip-confirm"), "{command:?}\n{text}");
    }
}

/// The model kinds with their `models` subcommand and the name the prompts give them
const MODEL_KINDS: [(&str, thorium::models::EntityKinds, &str); 3] = [
    (
        "endpoints",
        thorium::models::EntityKinds::ModelEndpoint,
        "model endpoint",
    ),
    (
        "profiles",
        thorium::models::EntityKinds::ModelProfile,
        "model profile",
    ),
    (
        "credentials",
        thorium::models::EntityKinds::ModelCredential,
        "model credential",
    ),
];

/// A mock API that holds one model entity, a pipeline, and an image, and accepts their removal
///
/// # Arguments
///
/// * `entity` - The model entity the API returns for its id
fn delete_api(entity: &thorium::models::Entity) -> MockApi {
    // the entity is read by id and deleted by id
    let id = entity.id;
    let body = serde_json::to_string(entity).unwrap();
    // answer the reads and the deletes this test's commands make, and nothing else
    MockApi::start(move |line| {
        if line.starts_with(&format!("GET /api/entities/{id}")) {
            (200, body.clone())
        } else if line.starts_with(&format!("DELETE /api/entities/{id}"))
            || line.starts_with("DELETE /api/pipelines/corn/orbit")
            || line.starts_with("DELETE /api/images/corn/scanner")
        {
            (204, String::new())
        } else {
            (404, r#"{"error":"not found"}"#.to_owned())
        }
    })
}

/// A model entity of one kind named `corn` in the group `wheat`
///
/// # Arguments
///
/// * `kind` - The model kind the entity has
fn model_entity(kind: thorium::models::EntityKinds) -> thorium::models::Entity {
    // fill every field a test doesn't care about
    thorium::models::Entity {
        id: uuid::Uuid::new_v4(),
        name: "corn".to_owned(),
        kind,
        metadata: thorium::models::EntityMetadata::Other,
        description: None,
        submitter: "mcarson".to_owned(),
        groups: vec!["wheat".to_owned()],
        tags: std::collections::HashMap::default(),
        image: None,
        created: chrono::Utc::now(),
    }
}

/// The `models ... delete` prompt names the entity, y deletes it, and n or Enter keeps it
#[test]
fn models_delete_prompt_deletes_on_y_and_keeps_on_n() {
    // the same prompt guards every model kind
    for (subcommand, kind, what) in MODEL_KINDS {
        // an API holding one entity of this kind, and a config pointing at it
        let entity = model_entity(kind);
        let id = entity.id.to_string();
        let api = delete_api(&entity);
        let scratch = Scratch::new();
        scratch.point_at(&api);
        let config = scratch.config();
        let args = [
            "--config",
            &config,
            "--skip-update",
            "models",
            subcommand,
            "delete",
            &id,
        ];
        // keep the run inside the scratch directory
        let envs = scratch.envs();
        let envs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
        // n keeps it and says nothing was changed
        let mut pty = Pty::spawn(thorctl(), &args, &envs).unwrap();
        pty.read_until(&format!("Delete {what} corn?"), WAIT)
            .unwrap();
        pty.send("n").unwrap();
        let status = pty.finish(WAIT).unwrap();
        assert_eq!(status.code(), Some(0), "{subcommand}\n{}", pty.output());
        assert!(pty.output().contains("Cancelled"), "{}", pty.output());
        assert_eq!(api.count("DELETE "), 0, "{subcommand}");
        // Enter takes the default, which is no
        let mut pty = Pty::spawn(thorctl(), &args, &envs).unwrap();
        pty.read_until(&format!("Delete {what} corn?"), WAIT)
            .unwrap();
        pty.send("\r").unwrap();
        let status = pty.finish(WAIT).unwrap();
        assert_eq!(status.code(), Some(0), "{subcommand}\n{}", pty.output());
        assert!(pty.output().contains("Cancelled"), "{}", pty.output());
        assert_eq!(api.count("DELETE "), 0, "{subcommand}");
        // y deletes it
        let mut pty = Pty::spawn(thorctl(), &args, &envs).unwrap();
        pty.read_until(&format!("Delete {what} corn?"), WAIT)
            .unwrap();
        pty.send("y").unwrap();
        let status = pty.finish(WAIT).unwrap();
        assert_eq!(status.code(), Some(0), "{subcommand}\n{}", pty.output());
        assert!(
            pty.output().contains(&format!("Deleted {what} corn")),
            "{}",
            pty.output()
        );
        assert_eq!(api.count(&format!("DELETE /api/entities/{id}")), 1);
    }
}

/// Without a terminal `models ... delete` is refused naming `--skip-confirm`, and either
/// `--skip-confirm` or `--yes` deletes
#[test]
fn models_delete_without_a_terminal_needs_the_flag() {
    // every model kind refuses the same way
    for (subcommand, kind, what) in MODEL_KINDS {
        // an API holding one entity of this kind, and a config pointing at it
        let entity = model_entity(kind);
        let id = entity.id.to_string();
        let api = delete_api(&entity);
        let scratch = Scratch::new();
        scratch.point_at(&api);
        // no flag, no terminal: refused and nothing deleted
        let (code, text) = run_without_terminal(&scratch, &["models", subcommand, "delete", &id]);
        assert_eq!(code, Some(1), "{subcommand}\n{text}");
        assert!(text.contains("--skip-confirm"), "{subcommand}\n{text}");
        assert_eq!(api.count("DELETE "), 0, "{subcommand}");
        // each spelling of the flag deletes
        for (count, flag) in ["--skip-confirm", "--yes"].into_iter().enumerate() {
            let (code, text) =
                run_without_terminal(&scratch, &["models", subcommand, "delete", &id, flag]);
            assert_eq!(code, Some(0), "{subcommand} {flag}\n{text}");
            assert!(text.contains(&format!("Deleted {what} corn")), "{text}");
            assert_eq!(api.count("DELETE "), count + 1, "{subcommand} {flag}");
        }
    }
}

/// `models ... delete --dry-run` names what would be deleted, needs no terminal or flag, and
/// deletes nothing
#[test]
fn models_delete_dry_run_deletes_nothing() {
    // every model kind says what it would delete
    for (subcommand, kind, what) in MODEL_KINDS {
        // an API holding one entity of this kind, and a config pointing at it
        let entity = model_entity(kind);
        let id = entity.id.to_string();
        let api = delete_api(&entity);
        let scratch = Scratch::new();
        scratch.point_at(&api);
        // a dry run needs neither a terminal nor a flag
        let (code, text) = run_without_terminal(
            &scratch,
            &["models", subcommand, "delete", &id, "--dry-run"],
        );
        assert_eq!(code, Some(0), "{subcommand}\n{text}");
        assert!(
            text.contains(&format!("would delete {what} corn")),
            "{subcommand}\n{text}"
        );
        assert_eq!(api.count("DELETE "), 0, "{subcommand}");
    }
}

/// `pipelines delete` and `images delete` take `--yes` as well as `--skip-confirm`, and without
/// either they are refused when there is no terminal
#[test]
fn pipelines_and_images_delete_accept_yes() {
    // an API that accepts the pipeline's and the image's deletion, and a config pointing at it
    let entity = model_entity(thorium::models::EntityKinds::ModelEndpoint);
    let api = delete_api(&entity);
    let scratch = Scratch::new();
    scratch.point_at(&api);
    // both commands take the same flags
    for (path, args) in [
        ("pipelines/corn/orbit", ["pipelines", "delete", "orbit"]),
        ("images/corn/scanner", ["images", "delete", "scanner"]),
    ] {
        // the request line that marks a deletion of this resource
        let delete = format!("DELETE /api/{path}");
        // no flag, no terminal: refused naming the flag
        let (code, text) =
            run_without_terminal(&scratch, &[args[0], args[1], args[2], "-g", "corn"]);
        assert_eq!(code, Some(1), "{path}\n{text}");
        assert!(text.contains("--skip-confirm"), "{path}\n{text}");
        assert_eq!(api.count(&delete), 0, "{path}");
        // each spelling of the flag deletes
        for (count, flag) in ["--skip-confirm", "--yes"].into_iter().enumerate() {
            let (code, text) =
                run_without_terminal(&scratch, &[args[0], args[1], args[2], "-g", "corn", flag]);
            assert_eq!(code, Some(0), "{path} {flag}\n{text}");
            assert!(text.contains("Deleted"), "{path} {flag}\n{text}");
            assert_eq!(api.count(&delete), count + 1, "{path} {flag}");
        }
    }
}
