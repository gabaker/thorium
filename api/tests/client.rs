//! Integration tests for the client's masquerade and refresh behavior

use std::fmt::{Debug, Write};
use std::sync::{Arc, Mutex};
use thorium::client::{ClientSettings, Users};
use thorium::models::{
    AssociationKind, AssociationRequest, AssociationTarget, CredentialKind, ElasticSearchOpts,
    EntityMetadataUpdate, EntityRequest, EntityUpdate, GroupUpdate, GroupUsersUpdate, UserCreate,
    UserRole,
};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Record};
use tracing::{Event, Id, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use uuid::Uuid;

/// Get the HTTP status an error carries, failing when it carries none
///
/// # Arguments
///
/// * `result` - The result that should be an error with a status
/// * `context` - What was expected, for failure messages
fn error_status<T>(result: Result<T, Error>, context: &str) -> Result<u16, Error> {
    match result {
        // a success means the call was not limited to the masqueraded user
        Ok(_) => Err(Error::new(format!("{context}: the request succeeded"))),
        // pull the status code out of the error
        Err(error) => error
            .status()
            .map(|status| status.as_u16())
            .ok_or_else(|| Error::new(format!("{context}: the error carried no status: {error}"))),
    }
}

/// Create an entity and return its id
///
/// # Arguments
///
/// * `client` - The client to create the entity with
/// * `group` - The group to create the entity in
async fn create_entity(client: &Thorium, group: &str) -> Result<Uuid, Error> {
    // build a random entity in this group
    let req: EntityRequest = generators::gen_entity(group, generators::gen_other_meta());
    // create it and keep its id
    Ok(client.entities.create(req).await?.id)
}

/// Build an association request between two entities
///
/// # Arguments
///
/// * `source` - The source entity's id
/// * `target` - The target entity's id
fn association(source: Uuid, target: Uuid) -> AssociationRequest {
    // link the two entities with a generic association
    AssociationRequest::new(
        AssociationKind::AssociatedWith,
        AssociationTarget::Entity {
            id: source,
            name: "source".to_owned(),
        },
    )
    .target(AssociationTarget::Entity {
        id: target,
        name: "target".to_owned(),
    })
}

/// Build search options limited to one group
///
/// # Arguments
///
/// * `group` - The group to search in
fn search_in(group: &str) -> ElasticSearchOpts {
    // search everything in this one group
    let mut opts = ElasticSearchOpts::new("*");
    opts.groups = vec![group.to_owned()];
    opts
}

/// A masqueraded client acts as the user for entities, associations, and search
#[tokio::test]
async fn masquerade_limits_entities_associations_and_search() -> Result<(), Error> {
    // get an admin client and a client for a new user
    let admin = test_utilities::admin_client().await?;
    let user_client = generators::client(&admin).await?;
    // the user owns group A and only the admin is in group B
    let group_a = generators::groups(1, &user_client).await?.remove(0).name;
    let group_b = generators::groups(1, &admin).await?.remove(0).name;
    // masquerade a copy of the admin client as the user
    let user = user_client.users.info().await?;
    let mut masked = admin.clone();
    masked.masquerade(&user);
    // an entity created through the masqueraded client is submitted by the user
    let a_id = create_entity(&masked, &group_a).await?;
    let a_entity = admin.entities.get(a_id).await?;
    assert_eq!(
        a_entity.submitter, user.username,
        "masqueraded entity submitter"
    );
    // an entity in group B is hidden from the masqueraded client just as from the user
    let b_id = create_entity(&admin, &group_b).await?;
    test_utilities::assert_status(masked.entities.get(b_id).await, 404, "masqueraded get in B")?;
    // a search of group B is refused the same way it is for the user's own client
    let masked_search = error_status(
        masked.search.search(&search_in(&group_b)).await,
        "masqueraded search of B",
    )?;
    let user_search = error_status(
        user_client.search.search(&search_in(&group_b)).await,
        "user search of B",
    )?;
    assert_eq!(masked_search, user_search, "search of B status");
    assert!(
        matches!(masked_search, 401 | 404),
        "search of B returned {masked_search}"
    );
    // an association naming the group B entity fails as it does for the user's own client
    let masked_assoc = error_status(
        masked.associations.create(&association(a_id, b_id)).await,
        "masqueraded association to B",
    )?;
    let user_assoc = error_status(
        user_client
            .associations
            .create(&association(a_id, b_id))
            .await,
        "user association to B",
    )?;
    assert_eq!(masked_assoc, user_assoc, "association to B status");
    assert!(
        matches!(masked_assoc, 401 | 404),
        "association to B returned {masked_assoc}"
    );
    // after reverting, the same reads succeed as the admin
    masked.revert_masquerade();
    masked.entities.get(b_id).await?;
    // the admin's search of B must get past group authorization, though the Elastic query
    // behind it can still fail when the test deployment has no result indexes
    if let Err(error) = masked.search.search(&search_in(&group_b)).await {
        let status = error.status().map(|status| status.as_u16());
        assert!(
            !matches!(status, Some(401 | 403 | 404)),
            "admin search of B after revert was refused: {error}"
        );
    }
    // associations need a group holding both ends, so the admin adds group B to the A entity
    let update = EntityUpdate::default().group(&group_b);
    masked.entities.update(a_id, update).await?;
    masked.associations.create(&association(a_id, b_id)).await?;
    Ok(())
}

/// A refresh moves every handler and the revert target to the refreshed token
#[tokio::test]
async fn refresh_replaces_masquerade_and_revert_target() -> Result<(), Error> {
    // get an admin client and create a user with a known password
    let admin = test_utilities::admin_client().await?;
    let username = generators::gen_string(24);
    let password = generators::gen_string(64);
    let bp = UserCreate::new(&username, &password, "fake@fake.gov")
        .skip_verification()
        .role(UserRole::User);
    let secret_key = Some(&test_utilities::CONF.thorium.secret_key);
    Users::create(&admin.host, bp, secret_key, &ClientSettings::default()).await?;
    // build a client for this user and a group it owns
    let mut client = Thorium::build(&admin.host)
        .basic_auth(username.clone(), password.clone())
        .build()
        .await?;
    let group = generators::groups(1, &client).await?.remove(0).name;
    // refresh this user's token, which logs out the token the client was built with
    client.refresh(&username, &password).await?;
    // entities created after the refresh are submitted by the refreshed user
    let id = create_entity(&client, &group).await?;
    assert_eq!(
        admin.entities.get(id).await?.submitter,
        username,
        "submitter after refresh"
    );
    // masquerade as a user that shares the group, so its creates are submitted by that user
    let other_client = generators::client(&admin).await?;
    let other = other_client.users.info().await?;
    let users_req = GroupUsersUpdate::default().direct_add(&other.username);
    let update = GroupUpdate::default().users(users_req);
    client.groups.update(&group, &update).await?;
    client.masquerade(&other);
    let id = create_entity(&client, &group).await?;
    assert_eq!(
        admin.entities.get(id).await?.submitter,
        other.username,
        "submitter while masquerading"
    );
    // the old token was logged out, so reverting must land on the refreshed token
    client.revert_masquerade();
    let id = create_entity(&client, &group).await?;
    assert_eq!(
        admin.entities.get(id).await?.submitter,
        username,
        "submitter after revert"
    );
    Ok(())
}

/// What a tracing layer saw: the names of the spans it saw and every field value in them
#[derive(Clone, Default)]
struct Captured {
    /// The names of the spans that were created
    spans: Arc<Mutex<Vec<String>>>,
    /// Every span and event field, rendered as text
    text: Arc<Mutex<String>>,
}

/// Writes the fields of a span or an event as text
struct FieldText<'a>(&'a mut String);

impl Visit for FieldText<'_> {
    /// Write one field as `name=value`
    ///
    /// # Arguments
    ///
    /// * `field` - The field that was recorded
    /// * `value` - The field's value
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        // every value kind falls back to its debug form, so all of them are written
        let _ = write!(self.0, "{}={value:?} ", field.name());
    }
}

impl<S> tracing_subscriber::Layer<S> for Captured
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    /// Remember a new span's name and its fields
    ///
    /// # Arguments
    ///
    /// * `attrs` - The span's attributes
    /// * `_id` - The span's id
    /// * `_ctx` - The layer's context
    fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
        // keep the name and write every attribute
        self.spans
            .lock()
            .unwrap()
            .push(attrs.metadata().name().to_owned());
        attrs.record(&mut FieldText(&mut self.text.lock().unwrap()));
    }

    /// Remember fields recorded on a span after it was created
    ///
    /// # Arguments
    ///
    /// * `_id` - The span's id
    /// * `values` - The values that were recorded
    /// * `_ctx` - The layer's context
    fn on_record(&self, _id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        // write every recorded value
        values.record(&mut FieldText(&mut self.text.lock().unwrap()));
    }

    /// Remember an event's fields
    ///
    /// # Arguments
    ///
    /// * `event` - The event
    /// * `_ctx` - The layer's context
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // write every field of the event
        event.record(&mut FieldText(&mut self.text.lock().unwrap()));
    }
}

/// The client's spans and events never hold a credential's secret, whether a credential is
/// created, read, rotated, or refused
#[tokio::test]
async fn client_spans_hold_no_credential_secrets() -> Result<(), Error> {
    // start the test API before capturing anything, so only this client's tracing is seen
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let secret = format!("sk-{}", generators::gen_string(32).to_lowercase());
    let rotated = format!("sk-{}", generators::gen_string(32).to_lowercase());
    // capture this thread's spans and events while the client works with the secrets
    let captured = Captured::default();
    let guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    // create and read a credential, then rotate its secret
    let credential = generators::model_credential(
        &[group.as_str()],
        CredentialKind::ApiKey,
        Some(&secret),
        &admin,
    )
    .await?;
    let update = EntityUpdate::default()
        .metadata(EntityMetadataUpdate::model_credential().credential_secret(rotated.as_str()));
    admin.entities.update(credential.id, update).await?;
    // a refused create fails without echoing the request it carried
    let refused = generators::model_credential(
        &["no-such-group"],
        CredentialKind::ApiKey,
        Some(&secret),
        &admin,
    )
    .await;
    assert!(refused.is_err(), "a credential was created in no group");
    drop(guard);
    // the client's spans were seen, so the check below looked at real tracing
    let spans = captured.spans.lock().unwrap().clone();
    for name in [
        "Thorium::Entities::create",
        "Thorium::Entities::get",
        "Thorium::Entities::update",
    ] {
        assert!(spans.iter().any(|seen| seen == name), "no {name} span");
    }
    // no field in them holds a secret
    let text = captured.text.lock().unwrap().clone();
    assert!(!text.is_empty(), "no span or event fields were captured");
    for (index, value) in [&secret, &rotated].into_iter().enumerate() {
        assert!(
            !text.contains(value),
            "a span or event holds secret #{index}"
        );
    }
    Ok(())
}
