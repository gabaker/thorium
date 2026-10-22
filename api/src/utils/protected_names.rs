//! The names tools, toolsets, workflows, and pipelines can't take
//!
//! These are the words for the analysis-execution entity kinds themselves. A command such as
//! `thorctl run <kind> <name>` takes a kind word followed by a name, so an entity named like a
//! kind would make that command line and the docs that describe it ambiguous. The list stays
//! limited to the words (and their plural and toolset spellings) that name those kinds; ordinary
//! words, and the words for other entities such as agents, models, runs, or images, are not
//! protected. Only new names are checked: an entity that already has one of these names keeps
//! working.

/// The names no new tool, toolset, workflow, or pipeline may take (compared in lowercase)
pub const PROTECTED_NAMES: [&str; 12] = [
    "tool",
    "tools",
    "toolset",
    "toolsets",
    "tool-set",
    "tool-sets",
    "tool_set",
    "tool_sets",
    "workflow",
    "workflows",
    "pipeline",
    "pipelines",
];

/// Check whether a name is one of the protected names
///
/// The whole name is compared without regard to case, so `Tool` is protected but `tool-runner`
/// and `my-pipeline` are not.
///
/// # Arguments
///
/// * `name` - The name to check
#[must_use]
pub fn is_protected(name: &str) -> bool {
    // compare the whole name to every protected name without regard to case
    PROTECTED_NAMES
        .iter()
        .any(|protected| protected.eq_ignore_ascii_case(name))
}

/// Build the one line refusal for a protected name
///
/// # Arguments
///
/// * `name` - The refused name
#[must_use]
pub fn refusal(name: &str) -> String {
    // name the offending word and the whole list on one line
    format!(
        "the name '{name}' is reserved for the kind of entity it names; \
         protected names (any case): {}",
        PROTECTED_NAMES.join(", ")
    )
}

/// Check that a new name is not protected
///
/// # Arguments
///
/// * `name` - The name to check
///
/// # Errors
///
/// Returns the one line refusal when the name is protected.
pub fn check(name: &str) -> Result<(), String> {
    // refuse a protected name with the line that names it and the list
    if is_protected(name) {
        Err(refusal(name))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{PROTECTED_NAMES, check, is_protected, refusal};

    /// Every protected word and its upper and mixed case spellings are refused
    #[test]
    fn protected_words_are_refused_in_any_case() {
        // check each word, its uppercase, and a mixed case spelling
        for word in PROTECTED_NAMES {
            let mut mixed = word.to_owned();
            mixed.replace_range(..1, &word[..1].to_uppercase());
            for name in [word.to_owned(), word.to_uppercase(), mixed] {
                assert!(is_protected(&name), "{name} should be protected");
                assert_eq!(check(&name), Err(refusal(&name)));
            }
        }
    }

    /// The list is the kind words and their toolset spellings, nothing else
    #[test]
    fn the_list_is_exactly_the_kind_words() {
        assert_eq!(PROTECTED_NAMES.len(), 12);
        // other entity words are not protected
        for word in ["agent", "model", "run", "instance", "image", "reaction"] {
            assert!(!is_protected(word), "{word} should be allowed");
        }
    }

    /// Names that only contain a protected word are allowed
    #[test]
    fn names_containing_a_word_are_allowed() {
        for name in [
            "tool-runner",
            "my-pipeline",
            "tool-pipeline",
            "workflow2",
            "toolbox",
            "pipeline-",
            "x-tools",
            "",
        ] {
            assert!(check(name).is_ok(), "{name} should be allowed");
        }
    }

    /// The refusal is one line that names the word and every protected name
    #[test]
    fn the_refusal_is_one_line_naming_word_and_list() {
        let line = refusal("Workflow");
        assert!(!line.contains('\n'));
        assert!(line.contains("'Workflow'"));
        assert!(line.ends_with("tool, tools, toolset, toolsets, tool-set, tool-sets, tool_set, tool_sets, workflow, workflows, pipeline, pipelines"));
    }
}
