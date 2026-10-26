//! Checks made before an import writes anything
//!
//! These catch what would fail partway through an import: groups the caller can't write to,
//! groups that don't exist when nothing may create them, and specs only an admin may publish.

use std::collections::BTreeSet;
use thorium::models::{SecurityContext, ToolSpec, WorkloadEnvironment};

use super::plan::{ImportPlan, Options};
use super::resolve::{Ids, Prepared, prepare_spec};
use super::world::World;
use crate::handlers::toolbox::v2::plan_types::{GroupState, ItemKind, ItemState};
use crate::handlers::toolbox::v2::types::ToolboxManifestV2;

/// Whether a spec sets a security context only an admin may publish
///
/// # Arguments
///
/// * `spec` - The typed spec to check
fn needs_admin(spec: &ToolSpec) -> bool {
    // only workloads have a security context, and only some environments
    let workload = match spec {
        ToolSpec::Image(workload) => workload,
        ToolSpec::Harness(harness) => &harness.workload,
        ToolSpec::Agentic(_) => return false,
    };
    match &workload.environment {
        WorkloadEnvironment::K8s(env) => env.security_context != SecurityContext::default(),
        WorkloadEnvironment::BareMetal(env) => env.security_context != SecurityContext::default(),
        _ => false,
    }
}

/// Find everything that would stop an import from writing what its plan says
///
/// # Arguments
///
/// * `manifest` - The manifest being imported
/// * `world` - What exists in Thorium
/// * `opts` - What the import is asked to do
/// * `plan` - The import's plan
#[must_use]
pub fn check(
    manifest: &ToolboxManifestV2,
    world: &World,
    opts: &Options,
    plan: &ImportPlan,
) -> Vec<String> {
    let mut problems = Vec::new();
    // only groups that something is written to matter
    let written: BTreeSet<&str> = plan
        .plan
        .items
        .iter()
        .filter(|item| matches!(item.state, ItemState::Create | ItemState::Update))
        .map(|item| item.group.as_str())
        .collect();
    // groups the caller can't write to, and groups nothing may create
    for group in &plan.plan.groups {
        match group.state {
            GroupState::Forbidden if written.contains(group.name.as_str()) => {
                problems.push(format!(
                    "you can't create tools, toolsets, agents, workflows, or pipelines in group \
                     {}: your role or the group's settings don't allow it",
                    group.name
                ));
            }
            GroupState::Missing if opts.create && !opts.create_groups => {
                problems.push(format!(
                    "group {} doesn't exist; use --create-groups to create it",
                    group.name
                ));
            }
            _ => {}
        }
    }
    // pipeline names have a length limit the API enforces
    for name in manifest.pipelines.keys() {
        if name.chars().count() > crate::handlers::toolbox::prompt::RESOURCE_NAME_MAX {
            problems.push(format!(
                "pipeline {name}: the name is longer than {} characters",
                crate::handlers::toolbox::prompt::RESOURCE_NAME_MAX
            ));
        }
    }
    // specs only an admin may publish
    let is_admin = world.caller.as_ref().is_some_and(|caller| caller.is_admin);
    if !is_admin {
        let ids = Ids::from_world(manifest, world);
        for (name, tool) in &manifest.tools {
            // a tool the import leaves alone needs no rights
            let touched = plan.plan.items.iter().any(|item| {
                item.item.kind == ItemKind::Tool
                    && item.item.name == *name
                    && matches!(item.state, ItemState::Create | ItemState::Update)
            });
            if !touched {
                continue;
            }
            for (label, version) in &tool.versions {
                // a spec that can't be prepared is reported by the plan
                if let Ok(Prepared::Ready(ready)) =
                    prepare_spec(manifest, world, &ids, version, &tool.groups)
                    && needs_admin(&ready.spec)
                {
                    problems.push(format!(
                        "tool {name}@{label}: only an admin can publish a workload with a \
                         non-default security context"
                    ));
                }
            }
        }
    }
    problems
}
