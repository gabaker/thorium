//! Support for compiling and running jq filters against json values
//!
//! This is used by sigma rule actions to extract values from the entities
//! that a sigma rule hit on. Filters are user authored but run inside
//! Thorium's services, so compilation also rejects constructs that could
//! crash the process running them or leak info from it (function
//! definitions, process killing/environment reading builtins, and overly
//! long filters).

use jaq_core::load::lex::StrPart;
use jaq_core::load::parse::{Pattern, Term};
use jaq_core::load::{Arena, File, Loader};
use jaq_core::path::Part;
use jaq_core::{Compiler, Ctx, Filter, Native, RcIter};
use jaq_json::Val;

/// The max number of outputs a single jq filter may yield
///
/// This bounds filters with unbounded output streams (e.g. `repeat(0)`) so they
/// fail instead of consuming unbounded time and memory. Filters that loop
/// forever without yielding outputs must be bounded by the caller (e.g. with a
/// timeout) since they cannot be detected here.
pub const MAX_JQ_OUTPUTS: usize = 1024;

/// The max length of a single jq filter in bytes
///
/// jq filters are parsed with a recursive descent parser whose stack use grows
/// with filter length, so an unbounded filter can overflow the stack of the
/// thread compiling it and abort the whole process. This cap also bounds how
/// much compile work a single filter can cost since every filter is compiled
/// synchronously while validating rule saves.
pub const MAX_JQ_FILTER_LEN: usize = 1024;

/// The jq builtins that user authored filters may not call
///
/// These builtins kill the process running the filter (`halt`/`halt_error`),
/// dump its environment (`env`), read its input stream (`input`/`inputs`), or
/// write to its stderr/logs (`debug`/`stderr`), none of which a filter that
/// extracts values from entity json should ever need. Field accesses like
/// `.env` are paths rather than calls and so stay allowed.
const DENIED_JQ_FUNS: [&str; 7] = [
    "debug",
    "env",
    "halt",
    "halt_error",
    "input",
    "inputs",
    "stderr",
];

/// An error from compiling or running a jq filter
#[derive(Debug, Clone)]
pub struct JqError {
    /// The jq filter that caused this error
    pub filter: String,
    /// The reason this filter failed to compile or run
    pub reason: String,
}

impl JqError {
    /// Create a new jq error
    ///
    /// # Arguments
    ///
    /// * `filter` - The jq filter that caused this error
    /// * `reason` - The reason this filter failed to compile or run
    fn new<F: Into<String>, R: Into<String>>(filter: F, reason: R) -> Self {
        JqError {
            filter: filter.into(),
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for JqError {
    /// Write this jq error to a formatter
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write this error too
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "jq filter '{}' failed: {}", self.filter, self.reason)
    }
}

impl std::error::Error for JqError {}

/// Flatten the errors from loading (lexing/parsing) a jq filter into a single string
///
/// # Arguments
///
/// * `errors` - The errors to flatten
fn format_load_errors(errors: &jaq_core::load::Errors<&str, ()>) -> String {
    // format each files load error with its debug representation
    let formatted = errors
        .iter()
        .map(|(_, error)| format!("{error:?}"))
        .collect::<Vec<String>>();
    // join all of our formatted errors into a single string
    formatted.join("; ")
}

/// Flatten the errors from compiling a jq filter into a single string
///
/// # Arguments
///
/// * `errors` - The errors to flatten
fn format_compile_errors(errors: &jaq_core::compile::Errors<&str, ()>) -> String {
    // format each undefined symbol error in each file
    let formatted = errors
        .iter()
        .flat_map(|(_, errs)| errs.iter())
        .map(|(name, undefined)| format!("undefined {} '{name}'", undefined.as_str()))
        .collect::<Vec<String>>();
    // join all of our formatted errors into a single string
    formatted.join("; ")
}

/// Check a parsed jq binding pattern for constructs user filters may not use
///
/// # Arguments
///
/// * `filter` - The original jq filter being checked
/// * `pattern` - The parsed binding pattern to check
fn check_pattern(filter: &str, pattern: &Pattern<&str>) -> Result<(), JqError> {
    // check this pattern based on what kind of pattern it is
    match pattern {
        // plain variable bindings contain no terms to check
        Pattern::Var(_) => Ok(()),
        // check each sub pattern in an array pattern
        Pattern::Arr(patterns) => patterns
            .iter()
            .try_for_each(|pattern| check_pattern(filter, pattern)),
        // check each object pattern entries key term and sub pattern
        Pattern::Obj(entries) => entries.iter().try_for_each(|(key, pattern)| {
            // check this entries key term
            check_term(filter, key)?;
            // check this entries sub pattern
            check_pattern(filter, pattern)
        }),
    }
}

/// Check a parsed jq term for constructs user filters may not use
///
/// This rejects function definitions since jaq evaluates non tail recursive
/// calls with unbounded stack use (a recursive def can abort the whole process
/// with a stack overflow at run time) and rejects calls to the builtins in
/// [`DENIED_JQ_FUNS`]. The recursion here is bounded by [`MAX_JQ_FILTER_LEN`]
/// since a terms depth cannot exceed the filters length.
///
/// # Arguments
///
/// * `filter` - The original jq filter being checked
/// * `term` - The parsed term to check
fn check_term(filter: &str, term: &Term<&str>) -> Result<(), JqError> {
    // check this term based on what kind of term it is
    match term {
        // reject function definitions since recursive defs overflow the stack
        Term::Def(_, _) => Err(JqError::new(
            filter,
            "function definitions are not allowed in these jq filters",
        )),
        // reject calls to denied builtins and check the calls args
        Term::Call(name, args) => {
            // reject this filter if it calls a denied builtin
            if DENIED_JQ_FUNS.contains(name) {
                return Err(JqError::new(
                    filter,
                    format!("the jq builtin '{name}' is not allowed in these jq filters"),
                ));
            }
            // check each of this calls args
            args.iter().try_for_each(|arg| check_term(filter, arg))
        }
        // check the terms interpolated into a string
        Term::Str(_, parts) => parts.iter().try_for_each(|part| match part {
            // check this interpolated term
            StrPart::Term(interpolated) => check_term(filter, interpolated),
            // literal string parts contain no terms to check
            StrPart::Str(_) | StrPart::Char(_) => Ok(()),
        }),
        // check the term building an array if one exists
        Term::Arr(arr) => arr.iter().try_for_each(|term| check_term(filter, term)),
        // check each object entries key term and optional value term
        Term::Obj(entries) => entries.iter().try_for_each(|(key, value)| {
            // check this entries key term
            check_term(filter, key)?;
            // check this entries value term if one was set
            value.iter().try_for_each(|value| check_term(filter, value))
        }),
        // check the term a negation or label wraps
        Term::Neg(term) | Term::Label(_, term) => check_term(filter, term),
        // check both sides of a pipe and any binding pattern between them
        Term::Pipe(left, pattern, right) => {
            // check the left side of this pipe
            check_term(filter, left)?;
            // check the binding pattern if this pipe binds one
            pattern
                .iter()
                .try_for_each(|pattern| check_pattern(filter, pattern))?;
            // check the right side of this pipe
            check_term(filter, right)
        }
        // check both sides of a binary operation
        Term::BinOp(left, _, right) => {
            // check the left side of this operation
            check_term(filter, left)?;
            // check the right side of this operation
            check_term(filter, right)
        }
        // check a reduce/foreachs source, binding pattern, and body terms
        Term::Fold(_, source, pattern, terms) => {
            // check the term this fold consumes
            check_term(filter, source)?;
            // check this folds binding pattern
            check_pattern(filter, pattern)?;
            // check this folds init/update/extract terms
            terms.iter().try_for_each(|term| check_term(filter, term))
        }
        // check the tried term and any catch handler
        Term::TryCatch(tried, catch) => {
            // check the term being tried
            check_term(filter, tried)?;
            // check the catch handler if one was set
            catch.iter().try_for_each(|catch| check_term(filter, catch))
        }
        // check each if/then branch and any else branch
        Term::IfThenElse(branches, otherwise) => {
            // check each condition and the branch it guards
            branches.iter().try_for_each(|(cond, branch)| {
                // check this branches condition
                check_term(filter, cond)?;
                // check the branch this condition guards
                check_term(filter, branch)
            })?;
            // check the else branch if one was set
            otherwise
                .iter()
                .try_for_each(|otherwise| check_term(filter, otherwise))
        }
        // check the term a path is applied to and any terms in its parts
        Term::Path(term, path) => {
            // check the term this path is applied to
            check_term(filter, term)?;
            // check the terms in each of this paths parts
            path.0.iter().try_for_each(|(part, _)| match part {
                // check this parts index term
                Part::Index(index) => check_term(filter, index),
                // check this parts range bound terms if they were set
                Part::Range(from, to) => {
                    // check the lower bound of this range
                    from.iter().try_for_each(|from| check_term(filter, from))?;
                    // check the upper bound of this range
                    to.iter().try_for_each(|to| check_term(filter, to))
                }
            })
        }
        // these terms contain no sub terms or calls to check
        Term::Id | Term::Recurse | Term::Num(_) | Term::Break(_) | Term::Var(_) => Ok(()),
    }
}

/// A compiled jq filter
pub struct CompiledJq {
    /// The original jq filter
    filter: String,
    /// The compiled jq filter
    compiled: Filter<Native<Val>>,
}

impl CompiledJq {
    /// Compile a jq filter
    ///
    /// Beyond compilation this rejects filters that are longer than
    /// [`MAX_JQ_FILTER_LEN`] bytes, define functions, use module directives, or
    /// call the builtins in [`DENIED_JQ_FUNS`] since user authored filters run
    /// inside Thorium's services.
    ///
    /// # Arguments
    ///
    /// * `filter` - The jq filter to compile
    pub fn new(filter: &str) -> Result<Self, JqError> {
        // bound filter length so parsing cannot overflow the stack and compile
        // work stays bounded
        if filter.len() > MAX_JQ_FILTER_LEN {
            return Err(JqError::new(
                filter,
                format!("filter is longer than {MAX_JQ_FILTER_LEN} bytes"),
            ));
        }
        // build a loader with the standard jq definitions
        let loader = Loader::new(jaq_std::defs().chain(jaq_json::defs()));
        // build an arena to parse this filter in
        let arena = Arena::default();
        // lex and parse this filter
        let modules = loader
            .load(
                &arena,
                File {
                    path: (),
                    code: filter,
                },
            )
            .map_err(|errors| JqError::new(filter, format_load_errors(&errors)))?;
        // parse this filter on its own so it can be checked for restricted
        // constructs since loaded modules do not expose their parsed bodies
        let Some(parsed) = jaq_core::load::parse(filter, |parser| parser.term()) else {
            // module directives are the only syntax the module load accepts
            // that a bare term parse does not
            return Err(JqError::new(
                filter,
                "module directives are not allowed in these jq filters",
            ));
        };
        // reject function definitions and denied builtins
        check_term(filter, &parsed)?;
        // compile this parsed filter with the standard jq functions
        let compiled = Compiler::default()
            .with_funs(jaq_std::funs().chain(jaq_json::funs()))
            .compile(modules)
            .map_err(|errors| JqError::new(filter, format_compile_errors(&errors)))?;
        // build our compiled jq filter
        let built = CompiledJq {
            filter: filter.to_owned(),
            compiled,
        };
        Ok(built)
    }

    /// Run this filter against a json value and cast all outputs to strings
    ///
    /// Strings are passed through unquoted while numbers and bools are cast with
    /// `to_string` and arrays/objects are serialized to compact json. This fails
    /// closed: a runtime error, a null output, or a filter that yields no outputs
    /// all return an error.
    ///
    /// # Arguments
    ///
    /// * `target` - The json value to run this filter against
    pub fn run(&self, target: &serde_json::Value) -> Result<Vec<String>, JqError> {
        // jq filters can read extra values from an input stream but we don't use one
        let inputs = RcIter::new(core::iter::empty());
        // run our compiled filter against this target value
        let outputs = self
            .compiled
            .run((Ctx::new([], &inputs), Val::from(target.clone())));
        // most filters yield a single output so start with room for one cast
        let mut casts = Vec::with_capacity(1);
        // cast each of this filters outputs to a string
        for output in outputs {
            // fail closed on filters with unbounded output streams
            if casts.len() >= MAX_JQ_OUTPUTS {
                return Err(JqError::new(
                    &self.filter,
                    format!("filter returned more than {MAX_JQ_OUTPUTS} outputs"),
                ));
            }
            // check if this output is a runtime error
            let val = output.map_err(|error| JqError::new(&self.filter, error.to_string()))?;
            // convert this output back to a serde_json value so we can cast it
            match serde_json::Value::from(val) {
                // fail closed on null outputs since they likely signal a missing field
                serde_json::Value::Null => {
                    return Err(JqError::new(&self.filter, "filter returned null"));
                }
                // pass strings through without quotes
                serde_json::Value::String(cast) => casts.push(cast),
                // cast bools to strings
                serde_json::Value::Bool(cast) => casts.push(cast.to_string()),
                // cast numbers to strings
                serde_json::Value::Number(cast) => casts.push(cast.to_string()),
                // serialize arrays and objects to compact json
                value => {
                    // serialize this array or object
                    let serialized = serde_json::to_string(&value)
                        .map_err(|error| JqError::new(&self.filter, error.to_string()))?;
                    // add this serialized value
                    casts.push(serialized);
                }
            }
        }
        // fail closed if this filter yielded no outputs
        if casts.is_empty() {
            return Err(JqError::new(&self.filter, "filter returned no outputs"));
        }
        Ok(casts)
    }
}

/// Validate that a jq filter compiles and uses no restricted constructs
///
/// # Arguments
///
/// * `filter` - The jq filter to validate
pub fn validate(filter: &str) -> Result<(), JqError> {
    // try to compile this filter and toss the compiled filter
    CompiledJq::new(filter).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid jq filter compiles
    #[test]
    fn compiles_valid_filter() {
        // compile a simple field access filter
        CompiledJq::new(".pid").expect("'.pid' should compile");
        // compile a filter using std functions
        CompiledJq::new(".pid | tostring").expect("'.pid | tostring' should compile");
    }

    /// An invalid jq filter fails to compile
    #[test]
    fn rejects_invalid_filter() {
        // garbage syntax must fail to lex/parse
        assert!(CompiledJq::new("][").is_err(), "'][' should not compile");
        // undefined functions must fail to compile
        assert!(
            CompiledJq::new("notafunc").is_err(),
            "'notafunc' should not compile"
        );
    }

    /// Extract a pid from a windows process like json blob
    #[test]
    fn extracts_pid() {
        // build a windows process like json blob
        let target = serde_json::json!({"pid": 4321, "name": "evil.exe"});
        // compile and run a pid extraction filter
        let outputs = CompiledJq::new(".pid").unwrap().run(&target).unwrap();
        // we should get the pid as a bare string
        assert_eq!(outputs, vec!["4321".to_owned()]);
    }

    /// jq string literals are passed through unquoted
    #[test]
    fn passes_string_literals() {
        // the target doesn't matter for a literal filter
        let target = serde_json::json!({});
        // compile and run a string literal filter
        let outputs = CompiledJq::new("\"memdump\"")
            .unwrap()
            .run(&target)
            .unwrap();
        // we should get the literal without json quotes
        assert_eq!(outputs, vec!["memdump".to_owned()]);
    }

    /// Null and missing fields fail closed
    #[test]
    fn fails_closed_on_null() {
        // build a json blob with a null field
        let target = serde_json::json!({"pid": 4321, "parent_pid": null});
        // a null field must fail
        assert!(
            CompiledJq::new(".parent_pid")
                .unwrap()
                .run(&target)
                .is_err()
        );
        // a missing field also yields null and so must also fail
        assert!(
            CompiledJq::new(".not_a_field")
                .unwrap()
                .run(&target)
                .is_err()
        );
    }

    /// A filter with no outputs fails closed
    #[test]
    fn fails_closed_on_empty() {
        // build a json blob to scan
        let target = serde_json::json!({"pid": 4321});
        // the empty builtin yields no outputs and must fail
        assert!(CompiledJq::new("empty").unwrap().run(&target).is_err());
    }

    /// A filter with an unbounded output stream fails closed
    #[test]
    fn fails_closed_on_unbounded_outputs() {
        // build a json blob to scan
        let target = serde_json::json!({"pid": 4321});
        // the repeat builtin yields outputs forever and must fail at the cap
        assert!(CompiledJq::new("repeat(0)").unwrap().run(&target).is_err());
    }

    /// Filters can yield multiple outputs in order
    #[test]
    fn extracts_multiple_outputs() {
        // build a json blob with multiple fields
        let target = serde_json::json!({"pid": 4321, "name": "evil.exe"});
        // compile and run a multi output filter
        let outputs = CompiledJq::new(".pid, .name")
            .unwrap()
            .run(&target)
            .unwrap();
        // both outputs should be returned in order
        assert_eq!(outputs, vec!["4321".to_owned(), "evil.exe".to_owned()]);
    }

    /// Arrays and objects are serialized to compact json
    #[test]
    fn serializes_compound_outputs() {
        // build a json blob with an array field
        let target = serde_json::json!({"args": ["-a", "-b"], "info": {"key": "value"}});
        // arrays are serialized to compact json
        let arrays = CompiledJq::new(".args").unwrap().run(&target).unwrap();
        assert_eq!(arrays, vec!["[\"-a\",\"-b\"]".to_owned()]);
        // objects are serialized to compact json
        let objects = CompiledJq::new(".info").unwrap().run(&target).unwrap();
        assert_eq!(objects, vec!["{\"key\":\"value\"}".to_owned()]);
    }

    /// jq alternative operators allow defaults for optional fields
    #[test]
    fn supports_fallbacks() {
        // build a json blob with a null parent pid
        let target = serde_json::json!({"pid": 4321, "parent_pid": null});
        // use the jq alternative operator to default null fields
        let outputs = CompiledJq::new(".parent_pid // 0")
            .unwrap()
            .run(&target)
            .unwrap();
        // we should get the fallback value
        assert_eq!(outputs, vec!["0".to_owned()]);
    }

    /// Overly long filters are rejected before they are parsed
    #[test]
    fn rejects_too_long_filters() {
        // build a filter one byte past the length cap
        let filter = format!(".{}", "a".repeat(MAX_JQ_FILTER_LEN));
        // this filter must be rejected without being parsed
        assert!(CompiledJq::new(&filter).is_err());
        // a deeply piped filter past the cap must also be rejected since the
        // parsers recursion grows with filter length
        let piped = ". | ".repeat(MAX_JQ_FILTER_LEN) + ".";
        assert!(CompiledJq::new(&piped).is_err());
    }

    /// Function definitions are rejected since recursive defs overflow the stack
    #[test]
    fn rejects_function_definitions() {
        // a non tail recursive def would abort the process at run time
        assert!(CompiledJq::new("def f: f + 1; f").is_err());
        // defs nested inside other terms must also be rejected
        assert!(CompiledJq::new("(def f: .; f)").is_err());
        // even harmless defs are rejected since defs are not needed to extract args
        assert!(CompiledJq::new("def pid: .pid; pid").is_err());
    }

    /// Builtins that kill the process or leak its state are rejected
    #[test]
    fn rejects_denied_builtins() {
        // every denied builtin must fail to compile
        for denied in DENIED_JQ_FUNS {
            assert!(
                CompiledJq::new(denied).is_err(),
                "'{denied}' should be rejected"
            );
        }
        // denied builtins must be rejected in nested positions too
        assert!(CompiledJq::new(".pid | halt").is_err());
        assert!(CompiledJq::new("[env]").is_err());
        assert!(CompiledJq::new("try halt_error(7) catch .").is_err());
        assert!(CompiledJq::new("\"\\(env)\"").is_err());
    }

    /// Field accesses and literals that share a denied builtins name stay allowed
    #[test]
    fn allows_denied_names_as_fields() {
        // build a json blob with a field named like a denied builtin
        let target = serde_json::json!({"env": "prod"});
        // field accesses are paths rather than calls so they must compile
        let outputs = CompiledJq::new(".env").unwrap().run(&target).unwrap();
        assert_eq!(outputs, vec!["prod".to_owned()]);
        // string literals that spell a denied builtin must also compile
        let literal = CompiledJq::new("\"halt\"").unwrap().run(&target).unwrap();
        assert_eq!(literal, vec!["halt".to_owned()]);
    }

    /// Module directives are rejected
    #[test]
    fn rejects_module_directives() {
        // module metadata loads cleanly but is not a bare term so it must be rejected
        assert!(CompiledJq::new("module {}; .pid").is_err());
        // includes fail to load since filters cannot read other modules
        assert!(CompiledJq::new("include \"foo\"; .pid").is_err());
    }
}
