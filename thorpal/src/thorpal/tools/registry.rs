//! The compile-time registry of ThorPal's optional compiled in tools.
//!
//! Every optional tool (today the `ghidra` and `transforms` groups) registers
//! itself here by stacking `#[thorium_derive::thorpal_tool(...)]` above its
//! `#[rig::tool_macro]` attribute; the attribute emits an [`inventory`]
//! submission carrying the tool's group, external requirements, log routing,
//! and coverage metadata, so annotating the function is the only step needed
//! to add a new compiled in tool. A workflow's [`AgentConfig`] then enables
//! tools per sub-agent by group or individual name against this registry.
//!
//! The always-on orchestration tools (forum, think, finish, rename, and the
//! sub-agent delegation tools) are structural to ThorPal's loop and stay
//! hand-registered rather than living here.
//!
//! [`AgentConfig`]: crate::thorpal::AgentConfig

use std::collections::BTreeSet;
use std::sync::{Arc, LazyLock};

use rig::AgentBuilder;
use rig::agent::WithBuilderTools;
use rig::tool::ToolContext;

use crate::thorpal::tools::scripts::ScriptHost;

/// An external requirement a tool needs before it can run
///
/// Requirements are validated once per run in `ThorPalBuilder::build`, so an
/// agent can never be built with a tool whose backing setup is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// This tool has no external requirements
    None,
    /// This tool drives Ghidra and needs a Ghidra install path configured
    Ghidra,
}

/// A single compiled in tool registered with `#[thorium_derive::thorpal_tool]`
pub struct ToolEntry {
    /// The tool name the model calls this tool by (rig's `Tool::NAME`)
    pub name: &'static str,
    /// The tool group this tool belongs to (e.g. `ghidra`)
    pub group: &'static str,
    /// The external requirement this tool needs before it can run
    pub requires: Requirement,
    /// The tracing target prefixes this tool's streaming logs are emitted
    /// under, empty for tools that never stream logs
    pub log_targets: &'static [&'static str],
    /// Whether calling this tool counts as examining a function for coverage
    pub examining: bool,
    /// Whether this tool takes a `target` arg naming a single function
    pub function_scoped: bool,
    /// Whether this tool mutates the analyzed project (write-side)
    pub mutating: bool,
    /// Registers this tool's Rhai script binding into an engine, when it has one
    ///
    /// `None` for tools scripts cannot call. `run_script` applies the binding of
    /// every enabled tool to its engine, so the functions a script can call
    /// always mirror the running agent's resolved toolset.
    pub rhai_bind: Option<fn(&mut rhai::Engine, &ScriptHost)>,
    /// The model-visible Rhai signature and one-line doc for this binding
    pub rhai_signature: Option<&'static str>,
    /// The full model-visible reference for this binding
    ///
    /// A multi-line description of the binding's arguments, overloads, and the
    /// exact shape of its return value, served on demand by the `rhai_docs`
    /// tool so the compact `rhai_signature` preamble list stays cheap.
    pub rhai_reference: Option<&'static str>,
    /// Adds this tool to an agent builder
    pub apply: fn(AgentBuilder<WithBuilderTools>) -> AgentBuilder<WithBuilderTools>,
}

// collect every submitted tool entry at link time
inventory::collect!(ToolEntry);

/// The resolved registry tool names of the agent a tool call runs under
///
/// Every sub-agent invocation inserts its own resolved toolset into its
/// [`ToolContext`] (replacing whatever the caller's inherited context carried),
/// so tools like `run_script` can mirror the running agent's toolset at
/// runtime -- disabling a tool in the workflow also disables its Rhai binding.
#[derive(Debug, Clone, Default)]
pub struct EnabledTools(pub Arc<BTreeSet<&'static str>>);

impl EnabledTools {
    /// Build the enabled set from an agent's resolved toolset
    ///
    /// # Arguments
    ///
    /// * `entries` - The resolved tool entries to collect the names of
    pub fn from_entries(entries: &[&'static ToolEntry]) -> Self {
        // collect just the stable tool names into the shared set
        EnabledTools(Arc::new(entries.iter().map(|entry| entry.name).collect()))
    }

    /// Pull the enabled set out of a tool context, empty when none is set
    ///
    /// # Arguments
    ///
    /// * `context` - The tool call context the run was prompted with
    pub fn from_context(context: &ToolContext) -> Self {
        // a context without an enabled set (e.g. the main agent) means no
        // registry tools, and therefore no script bindings, are enabled
        context.get::<EnabledTools>().cloned().unwrap_or_default()
    }

    /// Get the enabled tool entries that carry a Rhai binding, sorted by name
    pub fn script_bindings(&self) -> Vec<&'static ToolEntry> {
        // walk the stable registry order, keeping enabled entries with bindings
        all()
            .iter()
            .filter(|entry| entry.rhai_bind.is_some() && self.0.contains(entry.name))
            .copied()
            .collect()
    }
}

/// Every registered tool sorted by (group, name)
///
/// Inventory's iteration order is link-dependent, and tool order is visible to
/// the model, so every consumer goes through this stable ordering instead.
static ALL: LazyLock<Vec<&'static ToolEntry>> = LazyLock::new(|| {
    // gather every entry inventory collected
    let mut entries: Vec<&'static ToolEntry> = inventory::iter::<ToolEntry>.into_iter().collect();
    // sort them into a stable, deterministic order
    entries.sort_unstable_by_key(|entry| (entry.group, entry.name));
    entries
});

/// Get every registered tool sorted by (group, name)
pub fn all() -> &'static [&'static ToolEntry] {
    &ALL
}

/// Find a registered tool by name
///
/// # Arguments
///
/// * `name` - The tool name to look for
pub fn find(name: &str) -> Option<&'static ToolEntry> {
    // scan the registry for a tool with this exact name
    all().iter().find(|entry| entry.name == name).copied()
}

/// Get every registered tool in a group, empty if no group has this name
///
/// # Arguments
///
/// * `group` - The group name to collect the tools of
pub fn group(group: &str) -> Vec<&'static ToolEntry> {
    // collect every entry registered under this group name
    all()
        .iter()
        .filter(|entry| entry.group == group)
        .copied()
        .collect()
}

/// Whether calling this tool counts as examining a function for coverage
///
/// # Arguments
///
/// * `name` - The tool name to check
pub fn is_examining(name: &str) -> bool {
    // unregistered tools can never examine a function
    find(name).is_some_and(|entry| entry.examining)
}

/// Whether this tool takes a `target` arg naming a single function
///
/// # Arguments
///
/// * `name` - The tool name to check
pub fn is_function_scoped(name: &str) -> bool {
    // unregistered tools are never function scoped
    find(name).is_some_and(|entry| entry.function_scoped)
}

/// Whether this tool mutates the analyzed project (write-side)
///
/// # Arguments
///
/// * `name` - The tool name to check
pub fn is_mutating(name: &str) -> bool {
    // unregistered tools can never mutate anything
    find(name).is_some_and(|entry| entry.mutating)
}

/// Get the (tool name, tracing target prefixes) pairs for every tool that
/// streams logs
pub fn streaming_routes() -> impl Iterator<Item = (&'static str, &'static [&'static str])> {
    // only tools that declared log targets can stream logs
    all()
        .iter()
        .filter(|entry| !entry.log_targets.is_empty())
        .map(|entry| (entry.name, entry.log_targets))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ghidra group holds exactly the twenty-five Ghidra tools.
    ///
    /// This also catches any PascalCase drift between `thorpal_tool` and rig's
    /// `tool_macro`: a drifted struct ident would fail to compile, and a
    /// missing registration would fail this list.
    #[test]
    fn ghidra_group_has_expected_tools() {
        // collect the ghidra group's tool names (already sorted by name)
        let names: Vec<&str> = group("ghidra").iter().map(|entry| entry.name).collect();
        assert_eq!(
            names,
            [
                "callees",
                "callers",
                "decomp_function",
                "disassemble_function",
                "emulate_function",
                "find_function",
                "get_function",
                "import_child",
                "list_comments",
                "list_exports",
                "list_funcs",
                "list_imports",
                "list_programs",
                "list_strings",
                "memory_map",
                "read",
                "rename_function",
                "rename_symbol",
                "search_memory",
                "set_comment",
                "set_return_type",
                "set_signature",
                "string_refs",
                "xrefs_from",
                "xrefs_to",
            ]
        );
        // every ghidra tool needs an install and streams ghidra-cli logs
        for entry in group("ghidra") {
            assert_eq!(entry.requires, Requirement::Ghidra);
            assert_eq!(entry.log_targets, ["ghidra_cli"]);
        }
    }

    /// The containers group holds the static container/decompression tools.
    #[test]
    fn containers_group_has_expected_tools() {
        // collect the containers group's tool names (already sorted by name)
        let names: Vec<&str> = group("containers").iter().map(|entry| entry.name).collect();
        assert_eq!(
            names,
            [
                "decompress",
                "extract_file",
                "extract_macros",
                "list_container",
                "png_pixels"
            ]
        );
        // container tools need no install, stream no logs, and never mutate
        for entry in group("containers") {
            assert_eq!(entry.requires, Requirement::None);
            assert!(entry.log_targets.is_empty());
            assert!(!entry.mutating);
        }
    }

    /// The decode group holds the native byte-decoding tools.
    #[test]
    fn decode_group_has_expected_tools() {
        // collect the decode group's tool names (already sorted by name)
        let names: Vec<&str> = group("decode").iter().map(|entry| entry.name).collect();
        assert_eq!(
            names,
            [
                "api_hash_match",
                "byte_transform",
                "decode_bytes",
                "digest",
                "entropy_map",
                "grep_bytes",
                "pairwise_sub",
                "scan_refs",
                "stack_stores",
                "strings_range",
            ]
        );
        // decode tools need no install, stream no logs, and never mutate
        for entry in group("decode") {
            assert_eq!(entry.requires, Requirement::None);
            assert!(entry.log_targets.is_empty());
            assert!(!entry.mutating);
        }
    }

    /// The file group holds the raw-sample tools and needs no external setup.
    #[test]
    fn file_group_has_expected_tools() {
        // collect the file group's tool names (already sorted by name)
        let names: Vec<&str> = group("file").iter().map(|entry| entry.name).collect();
        assert_eq!(
            names,
            ["file_info", "read_file", "read_text", "strings_file"]
        );
        // raw file reads need no install, stream no logs, and never mutate
        for entry in group("file") {
            assert_eq!(entry.requires, Requirement::None);
            assert!(entry.log_targets.is_empty());
            assert!(!entry.mutating);
        }
    }

    /// The transforms group holds the xor tool and needs no external setup.
    #[test]
    fn transforms_group_has_expected_tools() {
        // collect the transforms group's tool names
        let names: Vec<&str> = group("transforms").iter().map(|entry| entry.name).collect();
        assert_eq!(names, ["bytewise_xor"]);
        // pure transforms need no install and stream no logs
        for entry in group("transforms") {
            assert_eq!(entry.requires, Requirement::None);
            assert!(entry.log_targets.is_empty());
        }
    }

    /// The scripts group holds exactly the nine Rhai script tools.
    #[test]
    fn scripts_group_has_expected_tools() {
        // collect the scripts group's tool names (already sorted by name)
        let names: Vec<&str> = group("scripts").iter().map(|entry| entry.name).collect();
        assert_eq!(
            names,
            [
                "append_script",
                "create_script",
                "edit_script",
                "list_scripts",
                "promote_script",
                "read_script",
                "rhai_docs",
                "run_script",
                "search_scripts",
            ]
        );
        // script tools need no external setup and never mutate the project
        for entry in group("scripts") {
            assert_eq!(entry.requires, Requirement::None);
            assert!(!entry.mutating);
            // only run_script streams logs, under the rhai_script target
            if entry.name == "run_script" {
                assert_eq!(entry.log_targets, ["rhai_script"]);
            } else {
                assert!(entry.log_targets.is_empty());
            }
        }
    }

    /// The memory group holds exactly the four cross-run memory tools.
    #[test]
    fn memory_group_has_expected_tools() {
        // collect the memory group's tool names (already sorted by name)
        let names: Vec<&str> = group("memory").iter().map(|entry| entry.name).collect();
        assert_eq!(
            names,
            [
                "list_memories",
                "read_memory",
                "save_memory",
                "search_memories"
            ]
        );
        // memory tools need no external setup, stream no logs, never mutate
        // the project, and carry no script bindings
        for entry in group("memory") {
            assert_eq!(entry.requires, Requirement::None);
            assert!(entry.log_targets.is_empty());
            assert!(!entry.mutating);
            assert!(entry.rhai_bind.is_none());
        }
    }

    /// The Rhai bindings cover exactly the read-side tools scripts may call.
    #[test]
    fn rhai_bindings_match_expected_tools() {
        // collect every tool that registers a binding into script engines
        let bound: Vec<&str> = all()
            .iter()
            .filter(|entry| entry.rhai_bind.is_some())
            .map(|entry| entry.name)
            .collect();
        assert_eq!(
            bound,
            [
                "decompress",
                "extract_file",
                "extract_macros",
                "list_container",
                "api_hash_match",
                "byte_transform",
                "decode_bytes",
                "digest",
                "grep_bytes",
                "pairwise_sub",
                "strings_range",
                "file_info",
                "read_file",
                "read_text",
                "strings_file",
                "callees",
                "callers",
                "decomp_function",
                "disassemble_function",
                "emulate_function",
                "find_function",
                "get_function",
                "list_comments",
                "list_exports",
                "list_funcs",
                "list_imports",
                "list_strings",
                "memory_map",
                "read",
                "search_memory",
                "string_refs",
                "xrefs_from",
                "xrefs_to",
                "bytewise_xor",
            ]
        );
        // every binding carries a model-visible signature, and only bindings do
        for entry in all() {
            assert_eq!(entry.rhai_bind.is_some(), entry.rhai_signature.is_some());
            // a full reference without a binding would document a phantom fn
            if entry.rhai_reference.is_some() {
                assert!(entry.rhai_bind.is_some());
            }
            // the write-side tools must never leak into script engines
            if entry.mutating {
                assert!(entry.rhai_bind.is_none());
            }
        }
    }

    /// The enabled-toolset wrapper gates which bindings a script run sees.
    #[test]
    fn enabled_tools_gate_script_bindings() {
        // an empty set (the main agent's default) exposes no bindings
        assert!(EnabledTools::default().script_bindings().is_empty());
        // a set with one bound and one unbound tool exposes just the binding
        let xor = find("bytewise_xor").expect("bytewise_xor should be registered");
        let docs = find("rhai_docs").expect("rhai_docs should be registered");
        let enabled = EnabledTools::from_entries(&[xor, docs]);
        let bound: Vec<&str> = enabled
            .script_bindings()
            .iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(bound, ["bytewise_xor"]);
    }

    /// The coverage flags match the tools that read or target functions.
    #[test]
    fn coverage_flags_match_expected_tools() {
        // only the code-reading tools count as examining a function
        let examining: Vec<&str> = all()
            .iter()
            .filter(|entry| entry.examining)
            .map(|entry| entry.name)
            .collect();
        assert_eq!(
            examining,
            [
                "decomp_function",
                "disassemble_function",
                "emulate_function"
            ]
        );
        // every tool that takes a function target is recorded for coverage
        let function_scoped: Vec<&str> = all()
            .iter()
            .filter(|entry| entry.function_scoped)
            .map(|entry| entry.name)
            .collect();
        assert_eq!(
            function_scoped,
            [
                "callees",
                "callers",
                "decomp_function",
                "disassemble_function",
                "emulate_function",
                "get_function",
                "set_return_type",
                "set_signature",
                "xrefs_from",
                "xrefs_to",
            ]
        );
    }

    /// The mutating flags match exactly the write-side project tools.
    #[test]
    fn mutating_flags_match_expected_tools() {
        // only the annotation tools and the child import may mutate the project
        let mutating: Vec<&str> = all()
            .iter()
            .filter(|entry| entry.mutating)
            .map(|entry| entry.name)
            .collect();
        assert_eq!(
            mutating,
            [
                "import_child",
                "rename_function",
                "rename_symbol",
                "set_comment",
                "set_return_type",
                "set_signature",
            ]
        );
        // the accessor resolves mutating and read-only tools correctly
        assert!(is_mutating("rename_function"));
        assert!(!is_mutating("list_strings"));
        assert!(!is_mutating("not_a_tool"));
    }

    /// Lookups by name hit registered tools and miss unknown ones.
    #[test]
    fn lookups_resolve_names() {
        // a registered tool resolves with its metadata intact
        let entry = find("decomp_function").expect("decomp_function should be registered");
        assert_eq!(entry.group, "ghidra");
        assert!(is_examining("decomp_function"));
        assert!(is_function_scoped("callers"));
        // unknown names miss cleanly everywhere
        assert!(find("decompile").is_none());
        assert!(!is_examining("list_funcs"));
        assert!(!is_function_scoped("bytewise_xor"));
        assert!(group("ghydra").is_empty());
    }
}
