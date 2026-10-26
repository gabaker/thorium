//! Tests that the thorctl image write commands print why image writes are gone

mod common;

use common::{TestUser, assert_success, unique};
use thorium::Error;
use thorium::models::{ImageBan, ImageBanKind, ImageBanUpdate, ImageUpdate};
use thorium::test_utilities::{self, generators};
use tokio::io::AsyncWriteExt;

/// Change the description of every exported image config under a directory
///
/// # Arguments
///
/// * `dir` - The export directory
fn change_descriptions(dir: &std::path::Path) {
    // walk the export directory
    for entry in std::fs::read_dir(dir)
        .expect("an export directory")
        .flatten()
    {
        let path = entry.path();
        // descend into nested directories
        if path.is_dir() {
            change_descriptions(&path);
            continue;
        }
        // change the description of each JSON config
        if path.extension().is_some_and(|ext| ext == "json") {
            let raw = std::fs::read_to_string(&path).expect("a config");
            let Ok(mut config) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            if config.get("description").is_some() {
                config["description"] = serde_json::json!("changed by import");
                std::fs::write(&path, serde_json::to_string_pretty(&config).expect("json"))
                    .expect("the changed config");
            }
        }
    }
}

/// Run thorctl in a terminal, cancelling the re-edit prompt it shows after a failed update
///
/// # Arguments
///
/// * `user` - The user to run thorctl as
/// * `name` - The name of the run's transcript
/// * `args` - The thorctl arguments after the global flags
async fn in_terminal(user: &TestUser, name: &str, args: &[&str]) -> std::process::Output {
    // build the thorctl command line for a terminal session
    let line = format!(
        "{} --keys {} --skip-update {}",
        env!("CARGO_BIN_EXE_thorctl"),
        user.keys.display(),
        args.join(" ")
    );
    // run it under script so it gets a terminal, feeding it a down arrow and enter to cancel
    let mut child = tokio::process::Command::new("script")
        .args(["-qec", &line, "/dev/null"])
        .env("NO_PROXY", "127.0.0.1,localhost,::1")
        .env("no_proxy", "127.0.0.1,localhost,::1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to start script");
    let mut stdin = child.stdin.take().expect("script's stdin");
    stdin.write_all(b"\x1b[B\r").await.expect("the keys");
    drop(stdin);
    // wait for thorctl to finish and record its transcript
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        child.wait_with_output(),
    )
    .await
    .expect("thorctl timed out")
    .expect("failed to run thorctl");
    user.record(name, args, &output);
    output
}

/// Check that a thorctl run failed printing the image writes refusal
///
/// # Arguments
///
/// * `output` - The run's output
fn assert_gone(output: &std::process::Output) {
    // the run fails and prints the pointer to the tools commands, not the API's raw refusal
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "the image write succeeded: {printed}"
    );
    assert!(
        printed.contains(
            "Images can no longer be created or updated; use tools (thorctl tools create|edit|publish)"
        ),
        "{printed}"
    );
    assert!(!printed.contains("/api/entities/"), "{printed}");
}

/// `images import`, `images edit`, and `images bans create` and `delete` fail printing that image
/// writes are gone and the image is unchanged
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn images_writes_print_gone() -> Result<(), Error> {
    // seed an image with a ban in the user's group, then refuse image writes there
    let user = TestUser::new("images-writes-gone").await?;
    test_utilities::faults::allow_image_writes(&user.group);
    let image = generators::images(&user.group, 1, false, &user.admin)
        .await?
        .remove(0);
    // the edit view of an external image holds no enum values, so the editor round trips it
    let plain = generators::images(&user.group, 1, true, &user.admin)
        .await?
        .remove(0);
    let ban = ImageBan::new(ImageBanKind::generic("seeded"));
    let ban_id = ban.id.to_string();
    let update = ImageUpdate::default().bans(ImageBanUpdate::default().add_ban(ban));
    user.admin
        .images
        .update(&user.group, &image.name, &update)
        .await?;
    test_utilities::faults::forbid_image_writes(&user.group);
    let before = user.admin.images.get(&user.group, &image.name).await?;
    let plain_before = user.admin.images.get(&user.group, &plain.name).await?;
    // export an image so it can be imported again
    let dir = std::env::temp_dir().join(unique("thorctl-images-"));
    let dir_str = dir.to_string_lossy().into_owned();
    let args = [
        "images",
        "export",
        "-g",
        &user.group,
        "-o",
        &dir_str,
        "--config-only",
        "--",
        &plain.name,
    ];
    assert_success(&user.run("images-export", &args).await);
    // change the exported config so importing it would update the image
    change_descriptions(&dir);
    // importing it fails
    let args = [
        "images",
        "import",
        "-g",
        &user.group,
        "-i",
        &dir_str,
        "--skip-push",
        "--overwrite",
    ];
    assert_gone(&user.run("images-import-gone", &args).await);
    // editing it fails
    let editor = dir.join("editor.sh");
    std::fs::write(
        &editor,
        "#!/bin/sh\nsed -i 's/^description:.*/description: changed/' \"$1\"\n",
    )?;
    std::process::Command::new("chmod")
        .arg("+x")
        .arg(&editor)
        .status()?;
    let editor_str = editor.to_string_lossy().into_owned();
    let args = [
        "images",
        "edit",
        "-e",
        &editor_str,
        "--",
        &plain.name,
        &user.group,
    ];
    assert_gone(&in_terminal(&user, "images-edit-gone", &args).await);
    // creating and deleting a ban fail
    let args = [
        "images",
        "bans",
        "create",
        "--",
        &user.group,
        &image.name,
        "banned",
    ];
    assert_gone(&user.run("images-bans-create-gone", &args).await);
    let args = [
        "images",
        "bans",
        "delete",
        "--",
        &user.group,
        &image.name,
        &ban_id,
    ];
    assert_gone(&user.run("images-bans-delete-gone", &args).await);
    // reading images still works
    let args = ["images", "get", "-g", &user.group];
    let output = user.run("images-get-after-gone", &args).await;
    assert_success(&output);
    assert!(common::stdout(&output).contains(&image.name));
    let qualified = format!("{}/{}", user.group, image.name);
    let args = ["images", "describe", "--", &qualified];
    assert_success(&user.run("images-describe-after-gone", &args).await);
    // the images are unchanged
    let after = user.admin.images.get(&user.group, &image.name).await?;
    assert_eq!(
        serde_json::to_value(&after)?,
        serde_json::to_value(&before)?
    );
    let plain_after = user.admin.images.get(&user.group, &plain.name).await?;
    assert_eq!(
        serde_json::to_value(&plain_after)?,
        serde_json::to_value(&plain_before)?
    );
    // deleting an image still works
    let args = [
        "images",
        "delete",
        "-g",
        &user.group,
        "--skip-confirm",
        "--",
        &plain.name,
    ];
    assert_success(&user.run("images-delete-after-gone", &args).await);
    assert!(
        user.admin
            .images
            .get(&user.group, &plain.name)
            .await
            .is_err()
    );
    Ok(())
}

/// Without a terminal, importing or exporting over differing images refuses unless
/// `--skip-conflicts` or `--overwrite` says what to do, and the flags skip or replace them
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn images_import_export_conflicts_need_a_flag_without_a_terminal() -> Result<(), Error> {
    // an image, exported to a directory
    let user = TestUser::new("images-conflicts").await?;
    // image writes stay allowed so the import reaches its conflict handling
    test_utilities::faults::allow_image_writes(&user.group);
    let image = generators::images(&user.group, 1, true, &user.admin)
        .await?
        .remove(0);
    let dir = std::env::temp_dir().join(unique("thorctl-images-conflicts-"));
    let dir_str = dir.to_string_lossy().into_owned();
    // the export arguments with some extra flags before the image name
    let export = |flags: &[&'static str]| -> Vec<&str> {
        let mut args = vec![
            "images",
            "export",
            "-g",
            &user.group,
            "-o",
            &dir_str,
            "--config-only",
        ];
        args.extend_from_slice(flags);
        args.extend(["--", &image.name]);
        args
    };
    assert_success(&user.run("images-export-new", &export(&[])).await);
    // the exported copy now differs from the stored image
    change_descriptions(&dir);
    let path = dir.join("images").join(format!("{}.json", image.name));
    let changed = std::fs::read_to_string(&path)?;
    // importing it with nothing to settle the difference refuses before anything is written
    let import = [
        "images",
        "import",
        "-g",
        &user.group,
        "-i",
        &dir_str,
        "--skip-push",
    ];
    let out = user.run("images-import-refused", &import).await;
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.contains("--skip-conflicts"), "{err}");
    assert!(err.contains(&image.name), "{err}");
    // --skip-conflicts leaves it alone, says how many were skipped, and succeeds (also with -q)
    for quiet in [false, true] {
        let mut args = import.to_vec();
        args.push("--skip-conflicts");
        if quiet {
            args.insert(0, "-q");
        }
        let out = user.run("images-import-skip", &args).await;
        assert_success(&out);
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(err.contains("Skipped 1 existing"), "quiet={quiet}: {err}");
        assert!(err.contains(&image.name), "quiet={quiet}: {err}");
    }
    // exporting over the changed file: the stored image differs from it, so the export refuses
    let out = user.run("images-export-refused", &export(&[])).await;
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        err.contains("--skip-conflicts") && err.contains("--overwrite"),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&path)?, changed);
    // --skip-conflicts keeps the file, says how many were skipped, and succeeds (also with -q)
    for quiet in [false, true] {
        let mut args = export(&["--skip-conflicts"]);
        if quiet {
            args.insert(0, "-q");
        }
        let out = user.run("images-export-skip", &args).await;
        assert_success(&out);
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(err.contains("Skipped 1 existing"), "quiet={quiet}: {err}");
        assert_eq!(std::fs::read_to_string(&path)?, changed);
    }
    // --overwrite replaces the file with the stored image
    assert_success(
        &user
            .run("images-export-overwrite", &export(&["--overwrite"]))
            .await,
    );
    assert_ne!(std::fs::read_to_string(&path)?, changed);
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
