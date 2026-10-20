//! Tests that stored password hashes can't choose their own Argon2 cost

use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use thorium::client::{ClientSettings, Users};
use thorium::models::UserCreate;
use thorium::test_utilities::{self, CONF};
use thorium::{Error, Thorium, fail};

/// Hash a password the way Thorium does but with a chosen algorithm and cost
///
/// # Arguments
///
/// * `password` - The password to hash
/// * `algorithm` - The Argon2 variant to hash with
/// * `params` - The memory, time, and lane costs to hash with
fn hash(password: &str, algorithm: Algorithm, params: Params) -> String {
    // hash with Thorium's secret key so only the algorithm and cost differ
    Argon2::new_with_secret(
        CONF.thorium.secret_key.as_bytes(),
        algorithm,
        Version::V0x13,
        params,
    )
    .unwrap()
    .hash_password(
        password.as_bytes(),
        &SaltString::from_b64("c2FsdHNhbHRzYWx0c2FsdA").unwrap(),
    )
    .unwrap()
    .to_string()
}

/// Create a user with a password and return their name
///
/// # Arguments
///
/// * `client` - An admin client
/// * `password` - The user's password
async fn user(client: &Thorium, password: &str) -> Result<String, Error> {
    // create a user that needs no email verification
    let name = format!("europa-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let create = UserCreate::new(&name, password, "europa@fake.gov").skip_verification();
    Users::create(
        &client.host,
        create,
        Some(&CONF.thorium.secret_key),
        &ClientSettings::default(),
    )
    .await?;
    Ok(name)
}

/// Replace a user's stored password hash directly in Redis
///
/// # Arguments
///
/// * `name` - The user whose hash to replace
/// * `hash` - The hash to store
async fn plant(name: &str, hash: &str) -> Result<(), Error> {
    // write the hash where the API stores user passwords
    let key = format!("{}:user_data:{name}", CONF.thorium.namespace);
    let redis = test_utilities::redis_pool().await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let (): () = bb8_redis::redis::cmd("HSET")
        .arg(key)
        .arg("password")
        .arg(hash)
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

/// Log in with basic auth
///
/// # Arguments
///
/// * `client` - A client pointed at the API
/// * `name` - The user to log in as
/// * `password` - The password to log in with
async fn login(client: &Thorium, name: &str, password: &str) -> Result<Thorium, Error> {
    // build a client with basic auth, which logs in to get a token
    Thorium::build(client.host.clone())
        .basic_auth(name, password)
        .build()
        .await
}

/// Check that a login was refused as unauthorized
///
/// # Arguments
///
/// * `attempt` - The result of the login
/// * `case` - What the login was testing
fn assert_unauthorized(attempt: Result<Thorium, Error>, case: &str) {
    // a refused login is an error carrying a 401
    match attempt {
        Ok(_) => panic!("{case}: the login succeeded"),
        Err(err) => assert_eq!(
            err.status().map(|status| status.as_u16()),
            Some(401),
            "{case}: {err}"
        ),
    }
}

/// Logging in as a user whose stored hash isn't argon2id with Thorium's own cost is refused,
/// even with the right password, and costly hashes are refused without being computed
#[serial_test::serial]
#[tokio::test]
async fn login_refuses_foreign_hashes() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    let password = "CornSilkPw7Q9z";
    let name = user(&client, password).await?;
    // a normal login works
    login(&client, &name, password).await?;
    // hashes of the right password with another algorithm or another cost are refused
    let foreign = [
        hash(password, Algorithm::Argon2i, Params::default()),
        hash(password, Algorithm::Argon2d, Params::default()),
        hash(
            password,
            Algorithm::Argon2id,
            Params::new(8192, 1, 1, None).unwrap(),
        ),
        hash(
            password,
            Algorithm::Argon2id,
            Params::new(19_456, 3, 1, None).unwrap(),
        ),
    ];
    for (index, planted) in foreign.iter().enumerate() {
        plant(&name, planted).await?;
        let attempt = login(&client, &name, password).await;
        assert_unauthorized(attempt, &format!("foreign hash {index}"));
    }
    // a hash naming a 4 GiB memory cost is refused without spending it
    plant(
        &name,
        "$argon2id$v=19$m=4194304,t=1,p=1$c2FsdHNhbHRzYWx0c2FsdA$\
        AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    )
    .await?;
    let start = Instant::now();
    let attempt = login(&client, &name, password).await;
    assert_unauthorized(attempt, "a 4 GiB memory cost");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "refusing a costly hash took {:?}",
        start.elapsed()
    );
    // the API keeps serving and other users still log in
    let other = user(&client, password).await?;
    login(&client, &other, password).await?;
    Ok(())
}

/// Restoring a backup whose users carry a foreign hash is refused before anything is wiped
#[serial_test::serial]
#[tokio::test]
async fn restore_refuses_foreign_hashes() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    let password = "CornSilkPw7Q9z";
    let name = user(&client, password).await?;
    // back up the system, then give our user a hash with another cost
    let mut backup = client.system.backup().await?;
    let costly = hash(
        password,
        Algorithm::Argon2id,
        Params::new(8192, 1, 1, None).unwrap(),
    );
    for user in &mut backup.users {
        if user.username == name {
            user.password = Some(costly.clone());
        }
    }
    // the restore is refused
    let restored = client.system.restore(&backup).await;
    fail!(restored, 400);
    // nothing was wiped: our user still logs in with their own hash
    login(&client, &name, password).await?;
    Ok(())
}
