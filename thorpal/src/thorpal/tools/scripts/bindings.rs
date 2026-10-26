//! The Rhai bindings registry tools expose to analysis scripts.
//!
//! Each registry tool that scripts may call registers a registrar here through
//! the `rhai_bind` arg of its `#[thorium_derive::thorpal_tool]` attribute. A
//! registrar adds one host function (named after the tool) to a script engine;
//! `run_script` applies exactly the registrars of the running agent's resolved
//! toolset, so disabling a tool in the workflow also removes its binding.
//!
//! Bindings run on the blocking script thread, so each one bridges back onto
//! the async runtime with the [`ScriptHost`]'s captured handle and `block_on`.
//! Host-side failures surface as catchable Rhai runtime errors via
//! [`script_err`], so a script can `try`/`catch` them or let them fail the run
//! with a useful message.

use std::path::PathBuf;

use ghidra_cli::models::ListOptions;
use rhai::{Array, Dynamic, Engine, EvalAltResult, ImmutableString, Map};
use serde::Serialize;

use super::{ScriptHost, script_err};
use crate::thorpal::tools::containers;
use crate::thorpal::tools::decode;
use crate::thorpal::tools::ghidra;

/// Serializes a host-side value into a script-native Rhai value
///
/// # Arguments
///
/// * `value` - The value to hand into the script
fn to_dyn<T: Serialize>(value: &T) -> Result<Dynamic, Box<EvalAltResult>> {
    // lean on serde so typed ghidra-cli results become rhai maps/arrays
    rhai::serde::to_dynamic(value).map_err(script_err)
}

/// Decodes a plain lowercase hex string into a script-native blob
///
/// # Arguments
///
/// * `hex` - The hex string to decode
pub(crate) fn decode_hex(hex: &str) -> Result<rhai::Blob, Box<EvalAltResult>> {
    // each byte takes two hex characters
    if !hex.len().is_multiple_of(2) {
        return Err(script_err("hex payload has an odd number of characters"));
    }
    // decode each two-character chunk into a single byte
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        let byte = u8::from_str_radix(&hex[i..i + 2], 16)
            .map_err(|_| script_err(format!("invalid hex at position {i}")))?;
        bytes.push(byte);
    }
    Ok(bytes)
}

/// Clamps a script-provided limit to a usable optional listing limit
///
/// # Arguments
///
/// * `limit` - The limit the script passed
fn clamp_limit(limit: i64) -> Option<usize> {
    // a negative limit from a script is treated as no limit at all
    usize::try_from(limit).ok()
}

/// Registers the `read` binding: read raw bytes at an address into a blob
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding calls back into
pub fn bind_read(engine: &mut Engine, host: &ScriptHost) {
    // clone the host into the closure so it outlives this registration
    let host = host.clone();
    engine.register_fn(
        "read",
        move |address: &str, size: i64| -> Result<rhai::Blob, Box<EvalAltResult>> {
            // bridge onto the async runtime from the blocking script thread
            host.handle.block_on(async {
                let op = host.ghidra.op(None).await.map_err(script_err)?;
                let ghidra = op.ghidra();
                // read the bytes and decode the bridge's hex payload to a blob
                let size = usize::try_from(size).map_err(|_| script_err("size must be >= 0"))?;
                let read = ghidra
                    .memory
                    .read(address, size)
                    .await
                    .map_err(script_err)?;
                decode_hex(&read.hex)
            })
        },
    );
}

/// Registers the `decomp_function` binding: decompile a function to C
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding calls back into
pub fn bind_decomp_function(engine: &mut Engine, host: &ScriptHost) {
    // clone the host into the closure so it outlives this registration
    let host = host.clone();
    engine.register_fn(
        "decomp_function",
        move |target: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            // bridge onto the async runtime from the blocking script thread
            host.handle.block_on(async {
                let op = host.ghidra.op(None).await.map_err(script_err)?;
                let ghidra = op.ghidra();
                // decompile with params and vars, like the tool does
                let result = ghidra
                    .function
                    .decompile(target, true, true)
                    .await
                    .map_err(script_err)?;
                to_dyn(&result)
            })
        },
    );
}

/// Converts a Rhai integer array into an emulation argument list.
///
/// # Arguments
///
/// * `args` - The script-supplied array of positional argument values
fn emulate_args(args: Array) -> Result<Vec<u64>, Box<EvalAltResult>> {
    // accept only non-negative integers; anything else is a script bug worth naming
    args.into_iter()
        .map(|value| {
            value
                .as_int()
                .ok()
                .and_then(|int| u64::try_from(int).ok())
                .ok_or_else(|| {
                    script_err(anyhow::anyhow!(
                        "emulate_function args must be non-negative integers"
                    ))
                })
        })
        .collect()
}

/// Converts a Rhai options value into a non-negative budget override.
///
/// # Arguments
///
/// * `key` - The options key being parsed, named in errors
/// * `value` - The script-supplied value
fn emulate_budget(key: &str, value: &Dynamic) -> Result<u64, Box<EvalAltResult>> {
    // reject non-integers and negatives outright, naming the key
    value
        .as_int()
        .ok()
        .and_then(|int| u64::try_from(int).ok())
        .ok_or_else(|| {
            script_err(anyhow::anyhow!(
                "emulate_function {key} must be a non-negative integer"
            ))
        })
}

/// Parses an `emulate_function` options map into the shared request inputs.
///
/// Accepts the full knob set the agent tool exposes: `args`, `registers`,
/// `buffer_hex`, `buffer_address`, `fuel`, `revisit_cap`, `timeout_secs`.
/// Unknown keys are rejected by name so typos never silently no-op.
///
/// # Arguments
///
/// * `opts` - The script-supplied options map
pub(crate) fn parse_emulate_opts(opts: Map) -> Result<ghidra::EmulateOpts, Box<EvalAltResult>> {
    // start from the all-defaults shape and fill what the script names
    let mut parsed = ghidra::EmulateOpts::default();
    for (key, value) in opts {
        match key.as_str() {
            "args" => {
                // reuse the positional-argument validation of the array forms
                let array = value.try_cast::<Array>().ok_or_else(|| {
                    script_err(anyhow::anyhow!("emulate_function args must be an array"))
                })?;
                parsed.args = emulate_args(array)?;
            }
            "registers" => {
                // convert the rhai map into JSON, then share the tool's parser
                // so both surfaces accept the exact same value forms
                let map = value.try_cast::<Map>().ok_or_else(|| {
                    script_err(anyhow::anyhow!(
                        "emulate_function registers must be a map of name -> value"
                    ))
                })?;
                let mut object = serde_json::Map::new();
                for (name, raw) in map {
                    // integers and strings are the two value forms the parser takes
                    let json = if let Ok(int) = raw.as_int() {
                        serde_json::Value::from(int)
                    } else if let Some(text) = raw.read_lock::<ImmutableString>() {
                        serde_json::Value::from(text.to_string())
                    } else {
                        return Err(script_err(anyhow::anyhow!(
                            "register `{name}` must be an integer or a hex string"
                        )));
                    };
                    object.insert(name.to_string(), json);
                }
                parsed.registers =
                    ghidra::parse_register_values(Some(&serde_json::Value::Object(object)))
                        .map_err(|error| script_err(anyhow::anyhow!(error)))?;
            }
            "buffer_hex" => {
                parsed.buffer_hex = Some(
                    value
                        .read_lock::<ImmutableString>()
                        .map(|text| text.to_string())
                        .ok_or_else(|| {
                            script_err(anyhow::anyhow!(
                                "emulate_function buffer_hex must be a string"
                            ))
                        })?,
                );
            }
            "buffer_address" => {
                parsed.buffer_address = Some(
                    value
                        .read_lock::<ImmutableString>()
                        .map(|text| text.to_string())
                        .ok_or_else(|| {
                            script_err(anyhow::anyhow!(
                                "emulate_function buffer_address must be a string"
                            ))
                        })?,
                );
            }
            "fuel" => parsed.fuel = Some(emulate_budget("fuel", &value)?),
            "revisit_cap" => parsed.revisit_cap = Some(emulate_budget("revisit_cap", &value)?),
            "timeout_secs" => parsed.timeout_secs = Some(emulate_budget("timeout_secs", &value)?),
            other => {
                return Err(script_err(anyhow::anyhow!(
                    "unknown emulate_function option `{other}`; supported: args, registers, \
                     buffer_hex, buffer_address, fuel, revisit_cap, timeout_secs"
                )));
            }
        }
    }
    Ok(parsed)
}

/// Runs one budget-clamped emulation and converts the result for Rhai.
///
/// # Arguments
///
/// * `host` - The host handle the binding calls back into
/// * `target` - The function to emulate (symbol, hex address, or `FUN_<hex>`)
/// * `opts` - The caller-tunable inputs the request is built from
fn run_emulation(
    host: &ScriptHost,
    target: &str,
    opts: ghidra::EmulateOpts,
) -> Result<Dynamic, Box<EvalAltResult>> {
    // the shared builder validates the buffer and clamps every budget to the
    // same server-side caps the agent tool enforces, so semantics never drift
    let request = ghidra::build_emulate_request(target.to_owned(), opts)
        .map_err(|error| script_err(anyhow::anyhow!(error)))?;
    // bridge onto the async runtime from the blocking script thread
    host.handle.block_on(async {
        let op = host.ghidra.op(None).await.map_err(script_err)?;
        let client = op.ghidra();
        let result = client.emulate.run(&request).await.map_err(script_err)?;
        to_dyn(&result)
    })
}

/// Registers the `emulate_function` binding: emulate a function and recover
/// the strings it builds at runtime
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding calls back into
pub fn bind_emulate_function(engine: &mut Engine, host: &ScriptHost) {
    // register one overload per optional argument, as rhai has no Option args
    let bare = host.clone();
    engine.register_fn(
        "emulate_function",
        move |target: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            run_emulation(&bare, target, ghidra::EmulateOpts::default())
        },
    );
    let with_args = host.clone();
    engine.register_fn(
        "emulate_function",
        move |target: &str, args: Array| -> Result<Dynamic, Box<EvalAltResult>> {
            let opts = ghidra::EmulateOpts {
                args: emulate_args(args)?,
                ..Default::default()
            };
            run_emulation(&with_args, target, opts)
        },
    );
    let with_fuel = host.clone();
    engine.register_fn(
        "emulate_function",
        move |target: &str, args: Array, fuel: i64| -> Result<Dynamic, Box<EvalAltResult>> {
            // reject a negative override outright, matching the options form
            let fuel = u64::try_from(fuel).map_err(|_| {
                script_err(anyhow::anyhow!(
                    "emulate_function fuel must be non-negative"
                ))
            })?;
            let opts = ghidra::EmulateOpts {
                args: emulate_args(args)?,
                fuel: Some(fuel),
                ..Default::default()
            };
            run_emulation(&with_fuel, target, opts)
        },
    );
    // the options-map form unlocks the full knob set the agent tool has
    let with_opts = host.clone();
    engine.register_fn(
        "emulate_function",
        move |target: &str, opts: Map| -> Result<Dynamic, Box<EvalAltResult>> {
            run_emulation(&with_opts, target, parse_emulate_opts(opts)?)
        },
    );
}

/// Registers the `disassemble_function` binding: disassemble a function
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding calls back into
pub fn bind_disassemble_function(engine: &mut Engine, host: &ScriptHost) {
    // clone the host into the closure so it outlives this registration
    let host = host.clone();
    engine.register_fn(
        "disassemble_function",
        move |target: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            // bridge onto the async runtime from the blocking script thread
            host.handle.block_on(async {
                let op = host.ghidra.op(None).await.map_err(script_err)?;
                let ghidra = op.ghidra();
                // keep the structured instructions so scripts can match on fields
                let instructions = ghidra
                    .function
                    .disasm(target, Some(100))
                    .await
                    .map_err(script_err)?;
                to_dyn(&instructions)
            })
        },
    );
}

/// Registers the `get_function` binding: get one function's details
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding calls back into
pub fn bind_get_function(engine: &mut Engine, host: &ScriptHost) {
    // clone the host into the closure so it outlives this registration
    let host = host.clone();
    engine.register_fn(
        "get_function",
        move |target: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            // bridge onto the async runtime from the blocking script thread
            host.handle.block_on(async {
                let op = host.ghidra.op(None).await.map_err(script_err)?;
                let ghidra = op.ghidra();
                let function = ghidra.function.get(target).await.map_err(script_err)?;
                to_dyn(&function)
            })
        },
    );
}

/// Lists functions for the `list_funcs` binding's arity overloads
///
/// # Arguments
///
/// * `host` - The host handle the binding calls back into
/// * `filter` - An optional case-insensitive name substring filter
/// * `limit` - An optional cap on how many functions come back
fn list_funcs_impl(
    host: &ScriptHost,
    filter: Option<String>,
    limit: Option<usize>,
) -> Result<Dynamic, Box<EvalAltResult>> {
    // bridge onto the async runtime from the blocking script thread
    host.handle.block_on(async {
        let op = host.ghidra.op(None).await.map_err(script_err)?;
        let ghidra = op.ghidra();
        let functions = ghidra
            .function
            .list(ListOptions { limit, filter })
            .await
            .map_err(script_err)?;
        to_dyn(&functions)
    })
}

/// Registers the `list_funcs` binding: list the binary's functions
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding calls back into
pub fn bind_list_funcs(engine: &mut Engine, host: &ScriptHost) {
    // register one overload per optional argument, as rhai has no Option args
    let unfiltered = host.clone();
    engine.register_fn("list_funcs", move || {
        list_funcs_impl(&unfiltered, None, None)
    });
    let filtered = host.clone();
    engine.register_fn("list_funcs", move |filter: &str| {
        list_funcs_impl(&filtered, Some(filter.to_owned()), None)
    });
    let capped = host.clone();
    engine.register_fn("list_funcs", move |filter: &str, limit: i64| {
        list_funcs_impl(&capped, Some(filter.to_owned()), clamp_limit(limit))
    });
}

/// Lists strings for the `list_strings` binding's arity overloads
///
/// # Arguments
///
/// * `host` - The host handle the binding calls back into
/// * `filter` - An optional case-insensitive value substring filter
/// * `limit` - An optional cap on how many strings come back
fn list_strings_impl(
    host: &ScriptHost,
    filter: Option<String>,
    limit: Option<usize>,
) -> Result<Dynamic, Box<EvalAltResult>> {
    // bridge onto the async runtime from the blocking script thread
    host.handle.block_on(async {
        let op = host.ghidra.op(None).await.map_err(script_err)?;
        let ghidra = op.ghidra();
        let strings = ghidra
            .string
            .list(ListOptions { limit, filter })
            .await
            .map_err(script_err)?;
        to_dyn(&strings)
    })
}

/// Registers the `list_strings` binding: list the binary's defined strings
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding calls back into
pub fn bind_list_strings(engine: &mut Engine, host: &ScriptHost) {
    // register one overload per optional argument, as rhai has no Option args
    let unfiltered = host.clone();
    engine.register_fn("list_strings", move || {
        list_strings_impl(&unfiltered, None, None)
    });
    let filtered = host.clone();
    engine.register_fn("list_strings", move |filter: &str| {
        list_strings_impl(&filtered, Some(filter.to_owned()), None)
    });
    let capped = host.clone();
    engine.register_fn("list_strings", move |filter: &str, limit: i64| {
        list_strings_impl(&capped, Some(filter.to_owned()), clamp_limit(limit))
    });
}

/// Registers the `list_imports` binding: list the binary's imported symbols
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding calls back into
pub fn bind_list_imports(engine: &mut Engine, host: &ScriptHost) {
    // clone the host into the closure so it outlives this registration
    let host = host.clone();
    engine.register_fn(
        "list_imports",
        move || -> Result<Dynamic, Box<EvalAltResult>> {
            // bridge onto the async runtime from the blocking script thread
            host.handle.block_on(async {
                let op = host.ghidra.op(None).await.map_err(script_err)?;
                let ghidra = op.ghidra();
                let imports = ghidra.symbol.imports().await.map_err(script_err)?;
                to_dyn(&imports)
            })
        },
    );
}

/// Registers the `list_exports` binding: list the binary's exported symbols
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding calls back into
pub fn bind_list_exports(engine: &mut Engine, host: &ScriptHost) {
    // clone the host into the closure so it outlives this registration
    let host = host.clone();
    engine.register_fn(
        "list_exports",
        move || -> Result<Dynamic, Box<EvalAltResult>> {
            // bridge onto the async runtime from the blocking script thread
            host.handle.block_on(async {
                let op = host.ghidra.op(None).await.map_err(script_err)?;
                let ghidra = op.ghidra();
                let exports = ghidra.symbol.exports().await.map_err(script_err)?;
                to_dyn(&exports)
            })
        },
    );
}

/// Lists comments for the `list_comments` binding's arity overloads
///
/// # Arguments
///
/// * `host` - The host handle the binding calls back into
/// * `filter` - An optional case-insensitive text substring filter
/// * `limit` - An optional cap on how many comments come back
fn list_comments_impl(
    host: &ScriptHost,
    filter: Option<String>,
    limit: Option<usize>,
) -> Result<Dynamic, Box<EvalAltResult>> {
    // bridge onto the async runtime from the blocking script thread
    host.handle.block_on(async {
        let op = host.ghidra.op(None).await.map_err(script_err)?;
        let ghidra = op.ghidra();
        let comments = ghidra
            .comment
            .list(ListOptions { limit, filter })
            .await
            .map_err(script_err)?;
        to_dyn(&comments)
    })
}

/// Registers the `list_comments` binding: list the project's comments
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding calls back into
pub fn bind_list_comments(engine: &mut Engine, host: &ScriptHost) {
    // register one overload per optional argument, as rhai has no Option args
    let unfiltered = host.clone();
    engine.register_fn("list_comments", move || {
        list_comments_impl(&unfiltered, None, None)
    });
    let filtered = host.clone();
    engine.register_fn("list_comments", move |filter: &str| {
        list_comments_impl(&filtered, Some(filter.to_owned()), None)
    });
    let capped = host.clone();
    engine.register_fn("list_comments", move |filter: &str, limit: i64| {
        list_comments_impl(&capped, Some(filter.to_owned()), clamp_limit(limit))
    });
}

/// Registers the `find_function` binding: find functions by name pattern
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding calls back into
pub fn bind_find_function(engine: &mut Engine, host: &ScriptHost) {
    // clone the host into the closure so it outlives this registration
    let host = host.clone();
    engine.register_fn(
        "find_function",
        move |pattern: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            // bridge onto the async runtime from the blocking script thread
            host.handle.block_on(async {
                let op = host.ghidra.op(None).await.map_err(script_err)?;
                let ghidra = op.ghidra();
                let results = ghidra.find.function(pattern).await.map_err(script_err)?;
                to_dyn(&results)
            })
        },
    );
}

/// Registers the `search_memory` binding: search memory for a pattern
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding calls back into
pub fn bind_search_memory(engine: &mut Engine, host: &ScriptHost) {
    // clone the host into the closure so it outlives this registration
    let host = host.clone();
    engine.register_fn(
        "search_memory",
        move |pattern: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            // bridge onto the async runtime from the blocking script thread
            host.handle.block_on(async {
                let op = host.ghidra.op(None).await.map_err(script_err)?;
                let ghidra = op.ghidra();
                let results = ghidra.memory.search(pattern).await.map_err(script_err)?;
                to_dyn(&results)
            })
        },
    );
}

/// Matches string refs for the `string_refs` binding's arity overloads
///
/// # Arguments
///
/// * `host` - The host handle the binding calls back into
/// * `value` - The string value to match (case-insensitive substring)
/// * `limit` - An optional cap on how many matched strings come back
fn string_refs_impl(
    host: &ScriptHost,
    value: &str,
    limit: Option<usize>,
) -> Result<Dynamic, Box<EvalAltResult>> {
    // bridge onto the async runtime from the blocking script thread
    host.handle.block_on(async {
        let op = host.ghidra.op(None).await.map_err(script_err)?;
        let ghidra = op.ghidra();
        let matches = ghidra
            .string
            .refs(value, false, limit)
            .await
            .map_err(script_err)?;
        to_dyn(&matches)
    })
}

/// Registers the `string_refs` binding: find code referencing a string value
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding calls back into
pub fn bind_string_refs(engine: &mut Engine, host: &ScriptHost) {
    // register one overload per optional argument, as rhai has no Option args
    let uncapped = host.clone();
    engine.register_fn("string_refs", move |value: &str| {
        string_refs_impl(&uncapped, value, None)
    });
    let capped = host.clone();
    engine.register_fn("string_refs", move |value: &str, limit: i64| {
        string_refs_impl(&capped, value, clamp_limit(limit))
    });
}

/// Registers the `xrefs_to` binding: find every reference to a target
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding calls back into
pub fn bind_xrefs_to(engine: &mut Engine, host: &ScriptHost) {
    // clone the host into the closure so it outlives this registration
    let host = host.clone();
    engine.register_fn(
        "xrefs_to",
        move |target: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            // bridge onto the async runtime from the blocking script thread
            host.handle.block_on(async {
                let op = host.ghidra.op(None).await.map_err(script_err)?;
                let ghidra = op.ghidra();
                let xrefs = ghidra.xref.to(target).await.map_err(script_err)?;
                to_dyn(&xrefs)
            })
        },
    );
}

/// Registers the `xrefs_from` binding: find every reference a target makes
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding calls back into
pub fn bind_xrefs_from(engine: &mut Engine, host: &ScriptHost) {
    // clone the host into the closure so it outlives this registration
    let host = host.clone();
    engine.register_fn(
        "xrefs_from",
        move |target: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            // bridge onto the async runtime from the blocking script thread
            host.handle.block_on(async {
                let op = host.ghidra.op(None).await.map_err(script_err)?;
                let ghidra = op.ghidra();
                let xrefs = ghidra.xref.from(target).await.map_err(script_err)?;
                to_dyn(&xrefs)
            })
        },
    );
}

/// Walks callers for the `callers` binding's arity overloads
///
/// # Arguments
///
/// * `host` - The host handle the binding calls back into
/// * `target` - The function to walk the call graph upward from
/// * `depth` - An optional number of call-graph levels to walk
fn callers_impl(
    host: &ScriptHost,
    target: &str,
    depth: Option<usize>,
) -> Result<Dynamic, Box<EvalAltResult>> {
    // bridge onto the async runtime from the blocking script thread
    host.handle.block_on(async {
        let op = host.ghidra.op(None).await.map_err(script_err)?;
        let ghidra = op.ghidra();
        let payload = ghidra
            .graph
            .callers(target, depth)
            .await
            .map_err(script_err)?;
        to_dyn(&payload)
    })
}

/// Registers the `callers` binding: walk the call graph upward
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding calls back into
pub fn bind_callers(engine: &mut Engine, host: &ScriptHost) {
    // register one overload per optional argument, as rhai has no Option args
    let direct = host.clone();
    engine.register_fn("callers", move |target: &str| {
        callers_impl(&direct, target, None)
    });
    let walked = host.clone();
    engine.register_fn("callers", move |target: &str, depth: i64| {
        callers_impl(&walked, target, clamp_limit(depth))
    });
}

/// Walks callees for the `callees` binding's arity overloads
///
/// # Arguments
///
/// * `host` - The host handle the binding calls back into
/// * `target` - The function to walk the call graph downward from
/// * `depth` - An optional number of call-graph levels to walk
fn callees_impl(
    host: &ScriptHost,
    target: &str,
    depth: Option<usize>,
) -> Result<Dynamic, Box<EvalAltResult>> {
    // bridge onto the async runtime from the blocking script thread
    host.handle.block_on(async {
        let op = host.ghidra.op(None).await.map_err(script_err)?;
        let ghidra = op.ghidra();
        let payload = ghidra
            .graph
            .callees(target, depth)
            .await
            .map_err(script_err)?;
        to_dyn(&payload)
    })
}

/// Registers the `callees` binding: walk the call graph downward
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding calls back into
pub fn bind_callees(engine: &mut Engine, host: &ScriptHost) {
    // register one overload per optional argument, as rhai has no Option args
    let direct = host.clone();
    engine.register_fn("callees", move |target: &str| {
        callees_impl(&direct, target, None)
    });
    let walked = host.clone();
    engine.register_fn("callees", move |target: &str, depth: i64| {
        callees_impl(&walked, target, clamp_limit(depth))
    });
}

/// Registers the `memory_map` binding: get the program's memory map
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding calls back into
pub fn bind_memory_map(engine: &mut Engine, host: &ScriptHost) {
    // clone the host into the closure so it outlives this registration
    let host = host.clone();
    engine.register_fn(
        "memory_map",
        move || -> Result<Dynamic, Box<EvalAltResult>> {
            // bridge onto the async runtime from the blocking script thread
            host.handle.block_on(async {
                let op = host.ghidra.op(None).await.map_err(script_err)?;
                let ghidra = op.ghidra();
                let map = ghidra.memory.map().await.map_err(script_err)?;
                to_dyn(&map)
            })
        },
    );
}

/// Resolves a script-side `target` onto a file path, synchronously
///
/// An empty target means the ORIGINAL sample; anything else names a file in
/// the session's extracted workspace, falling back to a sibling sample of a
/// multi-sample session by its file name (the workspace keeps precedence, so
/// extracted names resolve exactly as before). A store-less run (persistence
/// disabled) surfaces a catchable error instead of a wiring panic.
///
/// # Arguments
///
/// * `host` - The host handle carrying the sample path and session store
/// * `target` - The extracted file name, or `""` for the original sample
fn resolve_target_sync(host: &ScriptHost, target: &str) -> Result<PathBuf, Box<EvalAltResult>> {
    // an empty target reads the original sample straight off the host
    if target.is_empty() {
        return Ok(host.ghidra.binary().to_path_buf());
    }
    // extracted files only exist when the run has a session store
    let Some(store) = &host.store else {
        return Err(script_err(
            "extracted files need a session store, but this run has session \
             persistence disabled; only the original sample (target \"\") can be read",
        ));
    };
    // resolve the name inside the extracted workspace on the async side
    let path = host
        .handle
        .block_on(store.extracted_path(target))
        .map_err(script_err)?;
    // a missing file gets a hint listing what actually exists
    if !path.is_file() {
        // fall back to a sibling sample addressed by its file name (after the
        // workspace miss, so extracted names keep winning)
        if let Some(extra) = host
            .ghidra
            .extra_binaries()
            .iter()
            .find(|extra| extra.file_name().is_some_and(|file| file == target))
        {
            return Ok(extra.clone());
        }
        let known = host
            .handle
            .block_on(store.list_extracted())
            .unwrap_or_default();
        let mut hint = if known.is_empty() {
            "nothing has been extracted yet".to_owned()
        } else {
            format!("known extracted files: {}", known.join(", "))
        };
        // name the sibling samples too so a near-miss self-corrects
        let siblings: Vec<String> = host
            .ghidra
            .extra_binaries()
            .iter()
            .filter_map(|extra| extra.file_name())
            .map(|file| file.to_string_lossy().into_owned())
            .collect();
        if !siblings.is_empty() {
            hint = format!("{hint}; sibling samples: {}", siblings.join(", "));
        }
        return Err(script_err(format!(
            "no extracted file named '{target}' exists; {hint}"
        )));
    }
    Ok(path)
}

/// Registers the `read_file` binding: read raw bytes of the original sample
///
/// Reads the file synchronously on the script thread (plain file io needs no
/// async bridge), reusing the tool's own window reader so semantics never
/// drift. Works without any Ghidra install since only the sample path comes
/// from the session.
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding reads the sample path from
pub fn bind_read_file(engine: &mut Engine, host: &ScriptHost) {
    // register the original-sample overload and the targeted one
    let original = host.clone();
    engine.register_fn(
        "read_file",
        move |offset: i64, size: i64| -> Result<rhai::Blob, Box<EvalAltResult>> {
            read_file_impl(original.ghidra.binary().to_path_buf(), offset, size)
        },
    );
    let targeted = host.clone();
    engine.register_fn(
        "read_file",
        move |target: &str, offset: i64, size: i64| -> Result<rhai::Blob, Box<EvalAltResult>> {
            // resolve the target first so misses stay catchable
            let path = resolve_target_sync(&targeted, target)?;
            read_file_impl(path, offset, size)
        },
    );
}

/// Reads one raw window for the `read_file` binding's overloads
///
/// # Arguments
///
/// * `path` - The file to read (the sample or an extracted child)
/// * `offset` - The file offset to start reading at
/// * `size` - The most bytes to read, clamped to the script read cap
fn read_file_impl(path: PathBuf, offset: i64, size: i64) -> Result<rhai::Blob, Box<EvalAltResult>> {
    // reject a negative offset and clamp the size to the script cap
    let offset = u64::try_from(offset).map_err(|_| script_err("offset must be >= 0"))?;
    let size = usize::try_from(size).map_err(|_| script_err("size must be >= 0"))?;
    let size = size.min(crate::thorpal::tools::file::MAX_SCRIPT_READ);
    // read the window straight off the disk; the error names the path
    let (bytes, _) =
        crate::thorpal::tools::file::read_window_sync(&path, offset, size).map_err(script_err)?;
    Ok(bytes)
}

/// Extracts raw-file strings for the `strings_file` binding's arity overloads
///
/// # Arguments
///
/// * `path` - The file to scan (the sample or an extracted child)
/// * `min_length` - The minimum printable run length counted as a string
/// * `limit` - An optional cap on how many strings come back
fn strings_file_impl(
    path: PathBuf,
    min_length: i64,
    limit: Option<usize>,
) -> Result<Dynamic, Box<EvalAltResult>> {
    // clamp the knobs to the same ranges the tool enforces
    let min_length = usize::try_from(min_length).unwrap_or(0).max(2);
    let limit = limit
        .unwrap_or(crate::thorpal::tools::file::DEFAULT_STRINGS)
        .clamp(1, crate::thorpal::tools::file::MAX_STRINGS);
    // scan one window from the start of the raw file, reusing the tool's cores
    let (bytes, _) = crate::thorpal::tools::file::read_window_sync(
        &path,
        0,
        crate::thorpal::tools::file::SCAN_WINDOW,
    )
    .map_err(script_err)?;
    // a single-window script scan treats the window edge as end-of-data
    let (strings, _, _) =
        crate::thorpal::tools::file::extract_strings(&bytes, 0, min_length, limit, None, true);
    to_dyn(&strings)
}

/// Registers the `strings_file` binding: extract strings from the raw file
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding reads the sample path from
pub fn bind_strings_file(engine: &mut Engine, host: &ScriptHost) {
    // register one overload per optional argument, as rhai has no Option args
    let uncapped = host.clone();
    engine.register_fn("strings_file", move |min_length: i64| {
        strings_file_impl(uncapped.ghidra.binary().to_path_buf(), min_length, None)
    });
    let capped = host.clone();
    engine.register_fn("strings_file", move |min_length: i64, limit: i64| {
        strings_file_impl(
            capped.ghidra.binary().to_path_buf(),
            min_length,
            clamp_limit(limit),
        )
    });
    // the string-first overloads scan an extracted file instead
    let targeted = host.clone();
    engine.register_fn("strings_file", move |target: &str, min_length: i64| {
        let path = resolve_target_sync(&targeted, target)?;
        strings_file_impl(path, min_length, None)
    });
    let targeted_capped = host.clone();
    engine.register_fn(
        "strings_file",
        move |target: &str, min_length: i64, limit: i64| {
            let path = resolve_target_sync(&targeted_capped, target)?;
            strings_file_impl(path, min_length, clamp_limit(limit))
        },
    );
}

/// Reads one decoded text window for the `read_text` binding's overloads
///
/// # Arguments
///
/// * `path` - The file to read (the sample or an extracted child)
/// * `line` - The 1-based line to start from
/// * `max_lines` - An optional cap on how many lines come back
fn read_text_impl(
    path: PathBuf,
    line: i64,
    max_lines: Option<usize>,
) -> Result<Dynamic, Box<EvalAltResult>> {
    // clamp the knobs to the same ranges the tool enforces
    let start_line = usize::try_from(line).unwrap_or(1).max(1);
    let max_lines = max_lines
        .unwrap_or(crate::thorpal::tools::file::MAX_TEXT_LINES)
        .clamp(1, crate::thorpal::tools::file::MAX_TEXT_LINES);
    // decode one window from the start of the raw file via the tool's core
    let window = crate::thorpal::tools::file::read_text_sync(
        &path,
        0,
        start_line,
        max_lines,
        crate::thorpal::tools::file::MAX_TEXT_CHARS,
        None,
    )
    .map_err(script_err)?;
    // scripts consume the numbered lines themselves, not the window wrapper
    to_dyn(&window.lines)
}

/// Registers the `read_text` binding: read a raw file as line-numbered text
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding reads the sample path from
pub fn bind_read_text(engine: &mut Engine, host: &ScriptHost) {
    // register one overload per optional argument, as rhai has no Option args
    let uncapped = host.clone();
    engine.register_fn("read_text", move |line: i64| {
        read_text_impl(uncapped.ghidra.binary().to_path_buf(), line, None)
    });
    let capped = host.clone();
    engine.register_fn("read_text", move |line: i64, max_lines: i64| {
        read_text_impl(
            capped.ghidra.binary().to_path_buf(),
            line,
            clamp_limit(max_lines),
        )
    });
    // the string-first overloads read an extracted file instead
    let targeted = host.clone();
    engine.register_fn("read_text", move |target: &str, line: i64| {
        let path = resolve_target_sync(&targeted, target)?;
        read_text_impl(path, line, None)
    });
    let targeted_capped = host.clone();
    engine.register_fn(
        "read_text",
        move |target: &str, line: i64, max_lines: i64| {
            let path = resolve_target_sync(&targeted_capped, target)?;
            read_text_impl(path, line, clamp_limit(max_lines))
        },
    );
}

/// Registers the `file_info` binding: the raw sample file's identity
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding reads the sample path from
pub fn bind_file_info(engine: &mut Engine, host: &ScriptHost) {
    // register the original-sample overload and the targeted one
    let original = host.clone();
    engine.register_fn(
        "file_info",
        move || -> Result<Dynamic, Box<EvalAltResult>> {
            // reuse the tool's own streamed identity pass so semantics never drift
            let info = crate::thorpal::tools::file::file_identity_sync(original.ghidra.binary())
                .map_err(script_err)?;
            to_dyn(&info)
        },
    );
    let targeted = host.clone();
    engine.register_fn(
        "file_info",
        move |target: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            // resolve the target first so misses stay catchable
            let path = resolve_target_sync(&targeted, target)?;
            let info =
                crate::thorpal::tools::file::file_identity_sync(&path).map_err(script_err)?;
            to_dyn(&info)
        },
    );
}

/// Registers the `list_container` binding: list a container's entries
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding resolves file paths through
pub fn bind_list_container(engine: &mut Engine, host: &ScriptHost) {
    // register the original-sample overload and the targeted one
    let original = host.clone();
    engine.register_fn(
        "list_container",
        move || -> Result<Dynamic, Box<EvalAltResult>> {
            list_container_impl(original.ghidra.binary().to_path_buf())
        },
    );
    let targeted = host.clone();
    engine.register_fn(
        "list_container",
        move |target: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            // resolve the target first so misses stay catchable
            let path = resolve_target_sync(&targeted, target)?;
            list_container_impl(path)
        },
    );
}

/// Lists a container for the `list_container` binding's overloads
///
/// # Arguments
///
/// * `path` - The container file to list
fn list_container_impl(path: PathBuf) -> Result<Dynamic, Box<EvalAltResult>> {
    // the binding has no limit/filter knobs, so list at the tool's max page
    // size -- a truncation note a script cannot act on should be rare
    let listing = containers::list_container_sync(&path, 0, containers::MAX_ENTRIES, None)
        .map_err(script_err)?;
    to_dyn(&listing)
}

/// Registers the `extract_file` binding: extract a container entry
///
/// Rhai overloads cannot distinguish `(entry, password)` from `(target,
/// entry)`, so the two-string form is always `(target, entry)` and a password
/// requires the three-string form with `""` targeting the original sample.
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding resolves paths and the store through
pub fn bind_extract_file(engine: &mut Engine, host: &ScriptHost) {
    // one overload per calling shape, as rhai has no Option args
    let plain = host.clone();
    engine.register_fn(
        "extract_file",
        move |entry: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            extract_file_impl(&plain, "", entry, None)
        },
    );
    let targeted = host.clone();
    engine.register_fn(
        "extract_file",
        move |target: &str, entry: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            extract_file_impl(&targeted, target, entry, None)
        },
    );
    let decrypted = host.clone();
    engine.register_fn(
        "extract_file",
        move |target: &str, entry: &str, password: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            extract_file_impl(&decrypted, target, entry, Some(password))
        },
    );
}

/// Extracts one entry for the `extract_file` binding's overloads
///
/// # Arguments
///
/// * `host` - The host handle the binding resolves paths and the store through
/// * `target` - The extracted file name, or `""` for the original sample
/// * `entry` - The container entry to extract
/// * `password` - The ZIP password, when the archive needs one
fn extract_file_impl(
    host: &ScriptHost,
    target: &str,
    entry: &str,
    password: Option<&str>,
) -> Result<Dynamic, Box<EvalAltResult>> {
    // resolve the container to extract from
    let path = resolve_target_sync(host, target)?;
    // extraction lands in the workspace, so a store is required
    let Some(store) = &host.store else {
        return Err(script_err(
            "extract_file needs a session store, but this run has session \
             persistence disabled",
        ));
    };
    // serialize workspace writes so a script extraction can neither race a
    // tool extraction's `.tmp` sibling nor leapfrog the total-budget check
    let _guard = host.handle.block_on(store.lock_extracted());
    // resolve the workspace directory and its remaining budget
    let dest_dir = host
        .handle
        .block_on(store.extracted_dir())
        .map_err(script_err)?;
    let used = host
        .handle
        .block_on(store.extracted_usage())
        .map_err(script_err)?;
    let limits = containers::ExtractLimits {
        entry: containers::MAX_ENTRY_BYTES,
        total: containers::MAX_TOTAL_EXTRACTED.saturating_sub(used),
    };
    // extract right here on the blocking script thread
    let extracted = containers::extract_entry_sync(&path, 0, entry, password, &dest_dir, limits)
        .map_err(script_err)?;
    to_dyn(&extracted)
}

/// Registers the `extract_macros` binding: extract Office macro content
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `host` - The host handle the binding resolves paths and the store through
pub fn bind_extract_macros(engine: &mut Engine, host: &ScriptHost) {
    // one overload per calling shape, as rhai has no Option args
    let plain = host.clone();
    engine.register_fn(
        "extract_macros",
        move || -> Result<Dynamic, Box<EvalAltResult>> { extract_macros_impl(&plain, "") },
    );
    let targeted = host.clone();
    engine.register_fn(
        "extract_macros",
        move |target: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            extract_macros_impl(&targeted, target)
        },
    );
}

/// Extracts Office macro content for the `extract_macros` binding's overloads
///
/// # Arguments
///
/// * `host` - The host handle the binding resolves paths and the store through
/// * `target` - The extracted file name, or `""` for the original sample
fn extract_macros_impl(host: &ScriptHost, target: &str) -> Result<Dynamic, Box<EvalAltResult>> {
    // resolve the document to read the macros from
    let path = resolve_target_sync(host, target)?;
    // extracted module source lands in the workspace, so a store is required
    let Some(store) = &host.store else {
        return Err(script_err(
            "extract_macros needs a session store, but this run has session \
             persistence disabled",
        ));
    };
    // serialize workspace writes so a script extraction can neither race a
    // tool extraction's `.tmp` sibling nor leapfrog the total-budget check
    let _guard = host.handle.block_on(store.lock_extracted());
    // resolve the workspace directory and its remaining budget
    let dest_dir = host
        .handle
        .block_on(store.extracted_dir())
        .map_err(script_err)?;
    let used = host
        .handle
        .block_on(store.extracted_usage())
        .map_err(script_err)?;
    let limits = containers::ExtractLimits {
        entry: containers::MAX_ENTRY_BYTES,
        total: containers::MAX_TOTAL_EXTRACTED.saturating_sub(used),
    };
    // extract right here on the blocking script thread
    let report =
        containers::office::extract_macros_sync(&path, 0, &dest_dir, limits).map_err(script_err)?;
    to_dyn(&report)
}

/// Registers the `decompress` binding: decompress a blob in memory
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `_host` - Unused; the pure in-memory path never calls back into the host
pub fn bind_decompress(engine: &mut Engine, _host: &ScriptHost) {
    // the plain form covers every self-describing format
    engine.register_fn(
        "decompress",
        |data: rhai::Blob, format: &str| -> Result<rhai::Blob, Box<EvalAltResult>> {
            // reuse the tool's own sync core under the same output cap
            containers::decompress_bytes(&data, format, None, containers::MAX_DECOMPRESS_BYTES)
                .map_err(script_err)
        },
    );
    // the props form covers headerless raw LZMA1 streams
    engine.register_fn(
        "decompress",
        |data: rhai::Blob,
         format: &str,
         props: rhai::Map|
         -> Result<rhai::Blob, Box<EvalAltResult>> {
            // convert the rhai map into the JSON props the core parses
            let props: serde_json::Value =
                rhai::serde::from_dynamic(&Dynamic::from(props)).map_err(script_err)?;
            containers::decompress_bytes(
                &data,
                format,
                Some(&props),
                containers::MAX_DECOMPRESS_BYTES,
            )
            .map_err(script_err)
        },
    );
}

/// Registers the `bytewise_xor` binding: xor two equal-length hex strings
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `_host` - Unused; a pure transform never calls back into the host
pub fn bind_bytewise_xor(engine: &mut Engine, _host: &ScriptHost) {
    // reuse the tool's own implementation so semantics never drift
    engine.register_fn(
        "bytewise_xor",
        |a: &str, b: &str| -> Result<String, Box<EvalAltResult>> {
            crate::thorpal::tools::xor::bytewise_xor(a.to_owned(), b.to_owned()).map_err(script_err)
        },
    );
}

/// Enforce the decode tools' shared input cap on a script-provided blob
///
/// # Arguments
///
/// * `data` - The blob a binding is about to transform
fn check_decode_input(data: &[u8]) -> Result<(), Box<EvalAltResult>> {
    // bound the input so a script can never feed an absurd buffer through
    if data.len() > decode::MAX_DECODE_INPUT {
        return Err(script_err(format!(
            "the input is {} bytes; decode bindings accept at most {} bytes per call",
            data.len(),
            decode::MAX_DECODE_INPUT
        )));
    }
    Ok(())
}

/// Registers the `decode_bytes` binding: run a standardized decode pipeline
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `_host` - Unused; the pure in-memory path never calls back into the host
pub fn bind_decode_bytes(engine: &mut Engine, host: &ScriptHost) {
    // the pipeline runs entirely in memory over the script's blob
    engine.register_fn(
        "decode_bytes",
        |data: rhai::Blob, steps: rhai::Array| -> Result<rhai::Blob, Box<EvalAltResult>> {
            check_decode_input(&data)?;
            // convert the rhai step maps into the JSON the core parses
            let steps: Vec<serde_json::Value> = steps
                .iter()
                .map(rhai::serde::from_dynamic)
                .collect::<Result<_, _>>()
                .map_err(script_err)?;
            let parsed = decode::parse_steps(&steps).map_err(script_err)?;
            // run the pipeline and hand the raw output back to the script
            let (output, _) = decode::run_pipeline(&data, &parsed).map_err(script_err)?;
            Ok(output)
        },
    );
    // save_bytes rides the decode toolset: a payload a script decoded in
    // memory lands in the extracted workspace exactly like a tool's `name`
    // save, where every file tool and import_child can address it
    let saver = host.clone();
    engine.register_fn(
        "save_bytes",
        move |name: &str, data: rhai::Blob| -> Result<Dynamic, Box<EvalAltResult>> {
            save_bytes_impl(&saver, name, &data)
        },
    );
}

/// Saves a script-built payload into the extracted workspace
///
/// Mirrors the decode tools' `name` save path step for step: the same
/// workspace lock, the same traversal-proof name validation, and the same
/// per-entry and total budgets, so a script save can never do anything a
/// tool save could not.
///
/// # Arguments
///
/// * `host` - The host handle the binding reaches the store through
/// * `name` - The output's workspace name
/// * `data` - The payload bytes to save
fn save_bytes_impl(
    host: &ScriptHost,
    name: &str,
    data: &[u8],
) -> Result<Dynamic, Box<EvalAltResult>> {
    // the output lands in the extracted workspace, so a store is required
    let Some(store) = &host.store else {
        return Err(script_err(
            "save_bytes needs a session store, but this run has session \
             persistence disabled",
        ));
    };
    // reject an oversize payload before touching the workspace at all
    if data.len() as u64 > containers::MAX_ENTRY_BYTES {
        return Err(script_err(format!(
            "save_bytes payload is {} bytes; one workspace entry may hold at \
             most {} bytes",
            data.len(),
            containers::MAX_ENTRY_BYTES
        )));
    }
    // serialize workspace writes so a script save can neither race a tool
    // save's `.tmp` sibling nor leapfrog the total-budget check
    let _guard = host.handle.block_on(store.lock_extracted());
    // resolve and validate the output's workspace name up front
    let dest = host
        .handle
        .block_on(store.extracted_path(name))
        .map_err(script_err)?;
    // budget the payload against the workspace's remaining space
    let used = host
        .handle
        .block_on(store.extracted_usage())
        .map_err(script_err)?;
    let cap = containers::MAX_TOTAL_EXTRACTED.saturating_sub(used);
    // write and identify right here on the blocking script thread
    containers::write_capped(
        data,
        &dest,
        cap,
        &format!(
            "saving stopped: it would exceed the session's remaining extracted \
             workspace budget of {cap} bytes; nothing was written"
        ),
    )
    .map_err(script_err)?;
    let extracted = containers::finish_extracted(name.to_owned(), &dest).map_err(script_err)?;
    to_dyn(&extracted)
}

/// Applies one keyed byte transform for the `byte_transform` overloads
///
/// # Arguments
///
/// * `data` - The blob to transform
/// * `op` - The transform op name
/// * `key` - The key bytes
fn byte_transform_impl(
    data: &[u8],
    op: &str,
    key: &[u8],
) -> Result<rhai::Blob, Box<EvalAltResult>> {
    check_decode_input(data)?;
    decode::transform(op, key, data).map_err(script_err)
}

/// Registers the `byte_transform` binding: keyed transforms and brute forcing
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `_host` - Unused; the pure in-memory path never calls back into the host
pub fn bind_byte_transform(engine: &mut Engine, _host: &ScriptHost) {
    // the int-key overload covers the single-byte ops (and rol's bit count)
    engine.register_fn(
        "byte_transform",
        |data: rhai::Blob, op: &str, key: i64| -> Result<rhai::Blob, Box<EvalAltResult>> {
            let key = u8::try_from(key).map_err(|_| script_err("key must be 0-255"))?;
            byte_transform_impl(&data, op, &[key])
        },
    );
    // the blob-key overload covers repeating multi-byte xor keys
    engine.register_fn(
        "byte_transform",
        |data: rhai::Blob, op: &str, key: rhai::Blob| -> Result<rhai::Blob, Box<EvalAltResult>> {
            byte_transform_impl(&data, op, &key)
        },
    );
    // the keyless overload brute-forces the op's key space; the whole result
    // comes back (not just the candidates) so the input-clamp note survives
    engine.register_fn(
        "byte_transform",
        |data: rhai::Blob, op: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            check_decode_input(&data)?;
            let result = decode::brute(op, &data, decode::DEFAULT_BRUTE_TOP, "strings")
                .map_err(script_err)?;
            to_dyn(&result)
        },
    );
}

/// Registers the `pairwise_sub` binding: decode a pairwise-SUB buffer
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `_host` - Unused; the pure in-memory path never calls back into the host
pub fn bind_pairwise_sub(engine: &mut Engine, _host: &ScriptHost) {
    // the plain overload pairs from the first byte
    engine.register_fn(
        "pairwise_sub",
        |data: rhai::Blob| -> Result<rhai::Blob, Box<EvalAltResult>> {
            check_decode_input(&data)?;
            Ok(crate::thorpal::detect::decode::pairwise::decode(&data))
        },
    );
    // the parity overload skips one byte before pairing
    engine.register_fn(
        "pairwise_sub",
        |data: rhai::Blob, parity: i64| -> Result<rhai::Blob, Box<EvalAltResult>> {
            check_decode_input(&data)?;
            // only the two parities mean anything for byte pairs
            if !(0..=1).contains(&parity) {
                return Err(script_err("parity must be 0 or 1"));
            }
            let shifted = &data[(parity as usize).min(data.len())..];
            Ok(crate::thorpal::detect::decode::pairwise::decode(shifted))
        },
    );
}

/// Parses a script's hash-target array for the `api_hash_match` overloads
///
/// # Arguments
///
/// * `targets` - The script-side array of ints and/or hex strings
fn parse_script_targets(targets: &rhai::Array) -> Result<Vec<u32>, Box<EvalAltResult>> {
    // convert each entry to JSON and reuse the tool's own coercion
    targets
        .iter()
        .map(|value| {
            let json: serde_json::Value = rhai::serde::from_dynamic(value).map_err(script_err)?;
            decode::parse_hash_target(&json).map_err(script_err)
        })
        .collect()
}

/// Matches hashes for the `api_hash_match` binding's overloads
///
/// # Arguments
///
/// * `targets` - The parsed hash targets
/// * `family` - The hash family name
/// * `opts` - The optional opts map, already converted to JSON
fn api_hash_match_impl(
    targets: &[u32],
    family: &str,
    opts: Option<&serde_json::Value>,
) -> Result<Dynamic, Box<EvalAltResult>> {
    // pull each knob out of the opts map, mirroring the tool's arguments
    let get = |key: &str| opts.and_then(|opts| opts.get(key));
    let as_u64 = |key: &str| -> Result<Option<u64>, Box<EvalAltResult>> {
        match get(key) {
            None => Ok(None),
            Some(value) => value
                .as_u64()
                .map(Some)
                .ok_or_else(|| script_err(format!("`{key}` must be a non-negative integer"))),
        }
    };
    let seed = as_u64("seed")?;
    let multiplier = as_u64("multiplier")?.or(as_u64("mul")?);
    // the salt mirrors salt_hex + salt_position
    let salt = match get("salt_hex") {
        None => None,
        Some(value) => {
            let text = value
                .as_str()
                .ok_or_else(|| script_err("`salt_hex` must be a hex string"))?;
            let bytes = decode::parse_hex(text).map_err(script_err)?;
            // the shared parser keeps the binding and the tool in lockstep
            let suffix = decode::parse_salt_position(
                get("salt_position").and_then(serde_json::Value::as_str),
            )
            .map_err(script_err)?;
            Some((bytes, suffix))
        }
    };
    let variants = decode::parse_variants(get("variants")).map_err(script_err)?;
    // extra names extend the dictionary from the script side too
    let extra_names: Vec<String> = match get("extra_names") {
        None => Vec::new(),
        Some(value) => value
            .as_array()
            .ok_or_else(|| script_err("`extra_names` must be an array of strings"))?
            .iter()
            .map(|name| {
                name.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| script_err("`extra_names` must be an array of strings"))
            })
            .collect::<Result<_, _>>()?,
    };
    // run the shared matcher and hand the structured result to the script
    let result = decode::api_hash_match_sync(
        targets,
        family,
        seed,
        multiplier,
        salt,
        &variants,
        &extra_names,
    )
    .map_err(script_err)?;
    to_dyn(&result)
}

/// Registers the `api_hash_match` binding: resolve API-name hashes
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `_host` - Unused; the pure in-memory path never calls back into the host
pub fn bind_api_hash_match(engine: &mut Engine, _host: &ScriptHost) {
    // the plain overload takes just the targets and the family
    engine.register_fn(
        "api_hash_match",
        |targets: rhai::Array, family: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            let targets = parse_script_targets(&targets)?;
            api_hash_match_impl(&targets, family, None)
        },
    );
    // the opts overload adds seed/multiplier/salt/variants/extra_names
    engine.register_fn(
        "api_hash_match",
        |targets: rhai::Array,
         family: &str,
         opts: rhai::Map|
         -> Result<Dynamic, Box<EvalAltResult>> {
            let targets = parse_script_targets(&targets)?;
            // convert the rhai map into the JSON the shared parsing reads
            let opts: serde_json::Value =
                rhai::serde::from_dynamic(&Dynamic::from(opts)).map_err(script_err)?;
            api_hash_match_impl(&targets, family, Some(&opts))
        },
    );
}

/// The shape the `grep_bytes` binding hands back to scripts
///
/// Carries the limit indicator alongside the matches so a script can tell a
/// complete scan from a capped one instead of silently missing hits.
#[derive(Serialize)]
struct ScriptGrep {
    /// The matches found, in offset order
    matches: Vec<crate::thorpal::tools::decode::GrepMatch>,
    /// Whether the match limit cut the scan short
    hit_limit: bool,
    /// The blob offset the scan stopped at (safe to resume a sliced blob from)
    scanned_to: u64,
}

/// Greps a blob for the `grep_bytes` binding's overloads
///
/// # Arguments
///
/// * `data` - The blob to scan
/// * `keywords` - The script-side keyword array
/// * `opts` - The optional opts map, already converted to JSON
fn grep_bytes_impl(
    data: &[u8],
    keywords: &rhai::Array,
    opts: Option<&serde_json::Value>,
) -> Result<Dynamic, Box<EvalAltResult>> {
    check_decode_input(data)?;
    // every keyword must be a string
    let keywords: Vec<String> = keywords
        .iter()
        .map(|keyword| {
            keyword
                .clone()
                .into_string()
                .map_err(|_| script_err("`keywords` must be an array of strings"))
        })
        .collect::<Result<_, _>>()?;
    // pull each knob out of the opts map, mirroring the tool's arguments
    let get = |key: &str| opts.and_then(|opts| opts.get(key));
    let encodings =
        decode::GrepEncodings::parse(get("encodings").and_then(serde_json::Value::as_str))
            .map_err(script_err)?;
    let case_insensitive = get("case_insensitive")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let context = get("context")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(decode::DEFAULT_CONTEXT)
        .min(decode::MAX_CONTEXT);
    let limit = get("limit")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(decode::DEFAULT_MATCHES)
        .clamp(1, decode::MAX_MATCHES);
    // a blob is a complete buffer, so its edge is the end of the data
    let (matches, scanned_to, hit_limit) = decode::grep_window(
        data,
        0,
        &keywords,
        encodings,
        case_insensitive,
        context,
        limit,
        true,
    )
    .map_err(script_err)?;
    to_dyn(&ScriptGrep {
        matches,
        hit_limit,
        scanned_to,
    })
}

/// Registers the `grep_bytes` binding: keyword search over a blob
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `_host` - Unused; the pure in-memory path never calls back into the host
pub fn bind_grep_bytes(engine: &mut Engine, _host: &ScriptHost) {
    // the plain overload scans with the defaults
    engine.register_fn(
        "grep_bytes",
        |data: rhai::Blob, keywords: rhai::Array| -> Result<Dynamic, Box<EvalAltResult>> {
            grep_bytes_impl(&data, &keywords, None)
        },
    );
    // the opts overload adds encodings/case/context/limit knobs
    engine.register_fn(
        "grep_bytes",
        |data: rhai::Blob,
         keywords: rhai::Array,
         opts: rhai::Map|
         -> Result<Dynamic, Box<EvalAltResult>> {
            let opts: serde_json::Value =
                rhai::serde::from_dynamic(&Dynamic::from(opts)).map_err(script_err)?;
            grep_bytes_impl(&data, &keywords, Some(&opts))
        },
    );
}

/// Extracts strings from a blob for the `strings_range` overloads
///
/// # Arguments
///
/// * `data` - The blob to scan
/// * `min_length` - The minimum printable run length counted as a string
/// * `limit` - An optional cap on how many strings come back
fn strings_range_impl(
    data: &[u8],
    min_length: i64,
    limit: Option<usize>,
) -> Result<Dynamic, Box<EvalAltResult>> {
    check_decode_input(data)?;
    // clamp the knobs to the same ranges the tool enforces
    let min_length = usize::try_from(min_length).unwrap_or(0).max(2);
    let limit = limit
        .unwrap_or(crate::thorpal::tools::file::DEFAULT_STRINGS)
        .clamp(1, crate::thorpal::tools::file::MAX_STRINGS);
    // a blob is a complete buffer, so its edge is the end of the data
    let (strings, _, _) =
        crate::thorpal::tools::file::extract_strings(data, 0, min_length, limit, None, true);
    to_dyn(&strings)
}

/// Registers the `strings_range` binding: extract strings from a blob
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `_host` - Unused; the pure in-memory path never calls back into the host
pub fn bind_strings_range(engine: &mut Engine, _host: &ScriptHost) {
    // register one overload per optional argument, as rhai has no Option args
    engine.register_fn("strings_range", |data: rhai::Blob, min_length: i64| {
        strings_range_impl(&data, min_length, None)
    });
    engine.register_fn(
        "strings_range",
        |data: rhai::Blob, min_length: i64, limit: i64| {
            strings_range_impl(&data, min_length, clamp_limit(limit))
        },
    );
}

/// Registers the `digest` binding: hash a blob to lowercase hex
///
/// # Arguments
///
/// * `engine` - The engine to register into
/// * `_host` - Unused; the pure in-memory path never calls back into the host
pub fn bind_digest(engine: &mut Engine, _host: &ScriptHost) {
    // hash entirely in memory; the real streaming path is the tool's
    engine.register_fn(
        "digest",
        |data: rhai::Blob, algo: &str| -> Result<String, Box<EvalAltResult>> {
            check_decode_input(&data)?;
            let algo = decode::DigestAlgo::parse(algo).map_err(script_err)?;
            Ok(decode::digest_bytes(algo, &data))
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a Rhai map from key/value pairs for the options tests.
    ///
    /// # Arguments
    ///
    /// * `entries` - The key/value pairs to insert
    fn rhai_map(entries: Vec<(&str, Dynamic)>) -> Map {
        // collect into the engine's native map shape
        let mut map = Map::new();
        for (key, value) in entries {
            map.insert(key.into(), value);
        }
        map
    }

    /// The options map round-trips every knob the agent tool exposes.
    #[test]
    fn emulate_opts_full_map_round_trips() {
        // name every supported key once, mixing the register value forms
        let registers = rhai_map(vec![
            ("esi", Dynamic::from(0x403000_i64)),
            ("edi", Dynamic::from("0x500000".to_string())),
        ]);
        let opts = rhai_map(vec![
            ("args", Dynamic::from(vec![Dynamic::from(7_i64)])),
            ("registers", Dynamic::from(registers)),
            ("buffer_hex", Dynamic::from("41414141".to_string())),
            ("buffer_address", Dynamic::from("0x600000".to_string())),
            ("fuel", Dynamic::from(42_i64)),
            ("revisit_cap", Dynamic::from(500_i64)),
            ("timeout_secs", Dynamic::from(20_i64)),
        ]);
        let parsed = parse_emulate_opts(opts).expect("full options map");
        assert_eq!(parsed.args, vec![7]);
        assert_eq!(parsed.registers.get("esi"), Some(&0x403000));
        assert_eq!(parsed.registers.get("edi"), Some(&0x500000));
        assert_eq!(parsed.buffer_hex.as_deref(), Some("41414141"));
        assert_eq!(parsed.buffer_address.as_deref(), Some("0x600000"));
        assert_eq!(parsed.fuel, Some(42));
        assert_eq!(parsed.revisit_cap, Some(500));
        assert_eq!(parsed.timeout_secs, Some(20));
    }

    /// Unknown option keys are rejected by name so typos never silently no-op.
    #[test]
    fn emulate_opts_reject_unknown_keys() {
        // a plausible typo must fail loudly, naming itself and the valid set
        let opts = rhai_map(vec![("fule", Dynamic::from(42_i64))]);
        let err = parse_emulate_opts(opts).expect_err("typo must fail");
        let text = err.to_string();
        assert!(text.contains("fule"), "error must name the key: {text}");
        assert!(
            text.contains("supported"),
            "error must list the keys: {text}"
        );
    }

    /// Budget values must be non-negative integers, named on failure.
    #[test]
    fn emulate_opts_reject_bad_budgets() {
        // a negative budget is a script bug worth naming
        let opts = rhai_map(vec![("fuel", Dynamic::from(-1_i64))]);
        let err = parse_emulate_opts(opts).expect_err("negative fuel");
        assert!(err.to_string().contains("fuel"));
        // a non-map registers value is rejected before any register parses
        let opts = rhai_map(vec![("registers", Dynamic::from(1_i64))]);
        let err = parse_emulate_opts(opts).expect_err("non-map registers");
        assert!(err.to_string().contains("registers"));
    }

    /// Bad register values surface the shared parser's by-name errors.
    #[test]
    fn emulate_opts_reject_bad_register_values() {
        // garbage hex fails through the shared parser, naming the register
        let registers = rhai_map(vec![("esi", Dynamic::from("0xzz".to_string()))]);
        let opts = rhai_map(vec![("registers", Dynamic::from(registers))]);
        let err = parse_emulate_opts(opts).expect_err("garbage hex");
        assert!(err.to_string().contains("esi"));
    }
}
