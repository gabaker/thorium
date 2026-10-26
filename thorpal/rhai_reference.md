# Rhai Scripting Reference

Rhai is a small, Rust-like scripting language. Scripts you save with
`create_script` run in an embedded Rhai engine via `run_script`. This is a
condensed reference of the language features available to your scripts; the
host functions you can call are listed at the end of this document and follow
your enabled tools.

## Script shape

- Statements end with `;`. Blocks `{ ... }` are expressions.
- The script's **final expression** (no trailing `;`) is returned as the tool
  result, serialized to JSON. `return value;` also works. A script that ends
  with a statement returns `()` (null).
- `print("text")` and `debug(value)` stream live to the user while the script
  runs and are returned alongside the result.
- The JSON value passed to `run_script` as `args` is available as the constant
  `args` (null when none was passed).
- Comments are `// line` and `/* block */`.

## Variables and types

```rhai
let x = 42;               // integer (64-bit signed)
let f = 1.5;              // float (64-bit)
let flag = true;          // boolean
let s = "hello";          // string
let c = 'A';              // character
const KEY = 0x5a;         // constant; hex/binary literals work: 0x5a, 0b1010
let nothing = ();         // unit, serialized as null
```

`type_of(x)` returns a type name string. Integers support the usual
arithmetic plus bit ops: `& | ^ << >>`.

## Strings

```rhai
let s = "ab" + "cd";              // concatenation
let n = s.len();                  // length
let up = s.to_upper();            // also to_lower()
let has = s.contains("bc");       // substring test
let part = s.sub_string(1, 2);    // start, length
let fields = s.split(",");        // -> array of strings
s.trim();                         // IN PLACE: trims s itself, returns ()
s.replace("ab", "xy");            // IN PLACE: replaces in s, returns ()
let i = "ff".parse_int(16);       // parse with radix; parse_int() for base 10
let hex = 255.to_hex();           // int -> "ff" (blobs: b.to_hex())
let msg = `x = ${x}, s = ${s}`;   // backtick strings interpolate ${expr}
```

## Arrays

```rhai
let a = [1, 2, 3];
a.push(4);                        // append
let n = a.len();
let first = a[0];                 // zero-based indexing
let doubled = a.map(|x| x * 2);
let evens = a.filter(|x| x % 2 == 0);
let sum = a.reduce(|total, x| total + x, 0);
let has = a.contains(3);          // also: 3 in a
a.sort();                         // in-place sort
```

## Blobs (byte arrays)

Binary data (e.g. from the `read` host function) is a *blob*: a byte array
whose elements read as integers.

```rhai
let b = blob(16);                 // 16 zero bytes
let byte = b[0];                  // -> int (0-255)
b[0] = 0x41;                      // set a byte
let n = b.len();
let v = b.parse_le_int(0..4);     // little-endian int from a byte range
let w = b.parse_be_int(4..8);     // big-endian
let text = b.as_string();         // reinterpret the bytes as a UTF-8 string
let ints = b.to_array();          // -> array of ints
```

## Maps (objects)

```rhai
let m = #{ name: "entry", address: "00401000" };
let name = m.name;                // or m["name"]
m.size = 64;                      // add/overwrite a field
let keys = m.keys();              // -> array
let known = "name" in m;          // membership test
```

Host functions that return structured data (functions, strings, xrefs)
return maps and arrays of maps with the field names documented per function.

## Control flow

```rhai
if x > 0 { print("pos"); } else { print("non-pos"); }

for i in 0..16 { print(i); }      // 0..16 excludes 16; 0..=15 includes
for item in my_array { print(item); }

let i = 0;
while i < 10 { i += 1; }

loop {
    i += 1;
    if i > 20 { break; }
}

switch x {
    1 => print("one"),
    2 | 3 => print("two or three"),
    _ => print("other"),
}
```

## Functions and closures

```rhai
fn decode(b, key) {
    let out = blob(b.len());
    for i in 0..b.len() {
        out[i] = b[i] ^ key;
    }
    out                            // last expression is the return value
}

let add_one = |x| x + 1;           // closure (call it with .call)
let y = add_one.call(41);
```

## Errors

```rhai
if bad { throw "something went wrong"; }

try {
    let data = read("00401000", 16);   // host failures are catchable
} catch (err) {
    print(`read failed: ${err}`);
}
```

Host function failures (e.g. a bad address) raise runtime errors with the
underlying message; uncaught errors fail the run but your `print` output is
still returned.

## Gotchas

- Several string/array methods mutate IN PLACE and return `()`: `trim`,
  `replace`, `sort`, `push`, `reverse`, `clear`. `let t = s.trim();` makes
  `t` unit -- call `s.trim();` then use `s`.
- Plain `fn` arguments are BY VALUE: mutating a parameter inside a `fn`
  does not change the caller's variable. Return the result instead of
  writing to an out-parameter.
- Hex conversions: `"ff".parse_int(16)` -> int, `255.to_hex()` -> `"ff"`.
  For whole buffers use the always-available builtins: `hex_to_blob("deadbeef")`
  -> blob, `blob_to_hex(b)` -> string, `hexdump(b)` / `hexdump(b, base)` for a
  classic dump, and `to_char(65)` -> `"A"` for a code point.
- Host result fields documented as "or ()" can be unit: check with
  `field == ()` or `"field" in m` (e.g. a function's `entry_point` may be
  unit/empty -- fall back to `address`).
- Big scans: never iterate the whole binary in one run. Take skip/limit
  (or an address range) from `args` and run in batches.

## Limits

- Every run has an operation cap and a wall-clock deadline (default 300s,
  `timeout_secs` on `run_script` raises it to at most 600s). A script that
  exceeds either is terminated with a clear error.
- Deeply nested expressions fail to compile with `Expression exceeds maximum
  complexity` -- hoist nested map/array literals into `let` variables and
  split long chains into steps.
- Calling a function that is not defined and not in your enabled host
  functions fails **at call time** with `Function not found` -- compiling a
  script only catches syntax errors.

## Worked example

Read 64 bytes at an address from `args`, xor-decode them with a one-byte key,
and return the decoded text plus a hex preview:

```rhai
let addr = args.address;           // e.g. {"address": "00402000", "key": 0x5a}
let key = args.key;
let data = read(addr, 64);
let out = blob(data.len());
for i in 0..data.len() {
    out[i] = data[i] ^ key;
    if i % 16 == 15 { print(`decoded ${i + 1} bytes`); }
}
#{ text: out.as_string(), bytes: out.to_array() }
```
