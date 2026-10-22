//! Integration tests that run the test API with its real default worker stack size
//!
//! The API's workers run on 2 MiB stacks in a debug build started from `main`, while the other
//! test binaries give them more room. These tests stand the API up with the default size, so a
//! request path whose futures outgrow it aborts this binary instead of passing unnoticed.

use thorium::models::{
    AgentEntityRequest, AssociationKind, AssociationRequest, AssociationTarget,
    EntityMetadataUpdate, EntityUpdate, ToolSetEntityRequest,
};
use thorium::test_utilities::generators::{GroupRoles, god_group, moon_user};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium};
use uuid::Uuid;

/// The worker stack size `tokio::main` gives the API by default
const DEFAULT_STACK: usize = 2 << 20;

/// Get the sorted groups an entity is in, as an admin sees them
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `id` - The entity to read
async fn groups_of(admin: &Thorium, id: Uuid) -> Result<Vec<String>, Error> {
    // read the entity and sort its groups
    let mut groups = admin.entities.get(id).await?.groups;
    groups.sort();
    Ok(groups)
}

/// Widening edge writes from agents and toolsets run on the default worker stacks, through
/// their updates, their creates, and the generic association route
#[tokio::test]
async fn widening_edges_on_default_stacks() -> Result<(), Error> {
    // stand the API up with the default worker stack size
    assert_eq!(
        test_utilities::use_api_stack_size(DEFAULT_STACK),
        DEFAULT_STACK,
        "the API runs with another stack size"
    );
    let admin = test_utilities::admin_client().await?;
    // io edits entities in minerva and mars
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let mars = god_group(&admin, "mars", &suffix, &roles).await?;
    let both = {
        let mut both = vec![minerva.clone(), mars.clone()];
        both.sort();
        both
    };
    // four tools in minerva, and an agent and a toolset in minerva and mars
    let spec = generators::gen_agentic_spec();
    let (called, _) = generators::tool(&[&minerva], "1.0.0", &spec, &io.client).await?;
    let (included, _) = generators::tool(&[&minerva], "1.0.0", &spec, &io.client).await?;
    let (created_with, _) = generators::tool(&[&minerva], "1.0.0", &spec, &io.client).await?;
    let (generic, _) = generators::tool(&[&minerva], "1.0.0", &spec, &io.client).await?;
    let agent = io
        .client
        .entities
        .create(generators::gen_agent(
            "lead",
            &[&minerva, &mars],
            AgentEntityRequest::new(generators::gen_agent_config("lead")),
        ))
        .await?
        .id;
    let tool_set = io
        .client
        .entities
        .create(generators::gen_toolset(
            "kit",
            &[&minerva, &mars],
            ToolSetEntityRequest::default(),
        ))
        .await?
        .id;
    // the agent calls the first tool, widening it into mars
    let link = EntityMetadataUpdate::Agent {
        config: None,
        model_auth: None,
        harness: None,
        clear_harness: None,
        add_tools: vec![called.id],
        remove_tools: Vec::new(),
        add_tool_sets: Vec::new(),
        remove_tool_sets: Vec::new(),
        add_sub_agents: Vec::new(),
        remove_sub_agents: Vec::new(),
        extend_target_groups: true,
    };
    io.client
        .entities
        .update(agent, EntityUpdate::default().metadata(link))
        .await?;
    assert_eq!(groups_of(&admin, called.id).await?, both);
    // the toolset includes the second tool, widening it into mars
    let include = EntityMetadataUpdate::ToolSet {
        read_only: None,
        selector: None,
        clear_selector: None,
        auto_instance: None,
        add_tools: vec![included.id],
        remove_tools: Vec::new(),
        extend_target_groups: true,
    };
    io.client
        .entities
        .update(tool_set, EntityUpdate::default().metadata(include))
        .await?;
    assert_eq!(groups_of(&admin, included.id).await?, both);
    // an agent created calling the third tool widens it into mars
    let req = AgentEntityRequest::new(generators::gen_agent_config("helper"))
        .tool(created_with.id)
        .extend_target_groups();
    io.client
        .entities
        .create(generators::gen_agent("helper", &[&minerva, &mars], req))
        .await?;
    assert_eq!(groups_of(&admin, created_with.id).await?, both);
    // the generic route's call to the fourth tool widens it into mars
    let calls = AssociationRequest::new(
        AssociationKind::Calls,
        AssociationTarget::Entity {
            id: agent,
            name: "lead".to_owned(),
        },
    )
    .target(AssociationTarget::Entity {
        id: generic.id,
        name: generic.name.clone(),
    })
    .extend_target_groups();
    io.client.associations.create(&calls).await?;
    assert_eq!(groups_of(&admin, generic.id).await?, both);
    Ok(())
}
