//! Tests that `tokens delete` and `network-policies delete` confirm before deleting, and refuse
//! without a terminal unless told to skip the question

mod common;

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use common::{TestUser, assert_success, stdout, unique};
use thorium::Error;
use thorium::client::{ClientSettings, Users};
use thorium::models::{NetworkPolicyRequest, ScopedTokenRequest, UserCreate, UserRole};
use thorium::test_utilities;
use tokio::process::Command;

/// The text a command wrote to stderr
///
/// # Arguments
///
/// * `output` - The command's output
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Create an admin and a keys file for it
///
/// # Arguments
///
/// * `user` - A test user, whose API the admin is created on
async fn admin_keys(user: &TestUser) -> Result<PathBuf, Error> {
    // network policies are managed by admins only
    let name = unique("ctladmin");
    let password = unique("pw");
    let create = UserCreate::new(&name, &password, "fake@fake.gov")
        .skip_verification()
        .role(UserRole::Admin);
    let secret_key = Some(&test_utilities::CONF.thorium.secret_key);
    Users::create(
        &user.admin.host,
        create,
        secret_key,
        &ClientSettings::default(),
    )
    .await?;
    let dir = std::env::temp_dir().join(unique("thorctl-admin-"));
    std::fs::create_dir_all(&dir)?;
    let keys = dir.join("keys.yml");
    std::fs::write(
        &keys,
        format!(
            "api: {}\nusername: {name}\npassword: {password}\n",
            user.admin.host
        ),
    )?;
    Ok(keys)
}

/// Run `thorctl` without a terminal with an admin's keys
///
/// # Arguments
///
/// * `keys` - The admin's keys file
/// * `args` - The arguments after the global flags
async fn as_admin(keys: &Path, args: &[&str]) -> Output {
    // stdin is /dev/null, so there is no one to ask
    tokio::time::timeout(
        Duration::from_secs(120),
        Command::new(env!("CARGO_BIN_EXE_thorctl"))
            .arg("--keys")
            .arg(keys)
            .arg("--skip-update")
            .args(args)
            .env("NO_PROXY", "127.0.0.1,localhost,::1")
            .env("no_proxy", "127.0.0.1,localhost,::1")
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .expect("thorctl timed out")
    .expect("thorctl ran")
}

/// Without a terminal `tokens delete` refuses naming the flag and keeps the token; the flag deletes
#[tokio::test]
async fn tokens_delete_needs_the_flag_without_a_terminal() -> Result<(), Error> {
    // a scoped token the user owns
    let mut user = TestUser::new("confirm-tokens").await?;
    let name = unique("token");
    let req = ScopedTokenRequest::new(&name).groups(vec![user.group.clone()]);
    let scoped = user.client.users.create_scoped_token(&req).await?;
    user.evidence.register_secret(&scoped.token);
    // deleting it without the flag is refused and the token stays
    let out = user
        .run("tokens-delete-refused", &["tokens", "delete", &name])
        .await;
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("--skip-confirm"), "{}", stderr(&out));
    let listed = user.run("tokens-get-kept", &["tokens", "get"]).await;
    assert!(stdout(&listed).contains(&name), "{}", stdout(&listed));
    // the flag deletes it
    let out = user
        .run(
            "tokens-delete-skip",
            &["tokens", "delete", &name, "--skip-confirm"],
        )
        .await;
    assert_success(&out);
    assert!(stdout(&out).contains(&format!("Deleted scoped token {name}")));
    let listed = user.run("tokens-get-gone", &["tokens", "get"]).await;
    assert!(!stdout(&listed).contains(&name), "{}", stdout(&listed));
    Ok(())
}

/// Without a terminal `network-policies delete` refuses naming the flag and keeps the policy; the
/// flag deletes it
#[tokio::test]
async fn network_policy_delete_needs_the_flag_without_a_terminal() -> Result<(), Error> {
    // a policy shared by the user's group, and an admin to delete it
    let user = TestUser::new("confirm-network-policies").await?;
    let keys = admin_keys(&user).await?;
    let name = unique("policy");
    user.admin
        .network_policies
        .create(NetworkPolicyRequest::new(&name, [user.group.as_str()]))
        .await?;
    // deleting it without the flag is refused and the policy stays
    let out = as_admin(&keys, &["network-policies", "delete", &name]).await;
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("--skip-confirm"), "{}", stderr(&out));
    assert!(user.admin.network_policies.get(&name, None).await.is_ok());
    // the flag deletes it
    let out = as_admin(
        &keys,
        &["network-policies", "delete", &name, "--skip-confirm"],
    )
    .await;
    assert_success(&out);
    assert!(user.admin.network_policies.get(&name, None).await.is_err());
    Ok(())
}
