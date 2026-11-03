#!/usr/bin/env python3
"""Check that the Scylla keyspaces and S3 buckets CI derives from test code fit their limits.

The integration tests run in names CI makes up per job, and several tests build further names
on top of them. A name over its limit fails only in CI, since a local run uses a shorter run id,
so this script works out the longest name each test can make from the source alone (no build):

* thoradm's harness names every test binary's keyspace `testing_<run>_<binary>` and its buckets
  `testing-<run>-<binary>-<role>`, where the run id is CI's `<7 hex>-<5 hex>` (13 characters) and
  the binary is the test target's name, so every thoradm test target is measured
* a test that builds a keyspace (`<base>_<suffix>`) or bucket (`<base>-<suffix>`) with `format!`
  from the run's namespace is measured with that namespace at its CI length: the api harness's
  and thoradm's base namespace `testing_ci_<7 hex>_<8 hex>` (27 characters), or a thoradm
  harness namespace for the binary the file belongs to. A suffix passed in by a helper's caller
  is measured at every call site, from a string literal, a `let` of one, or the literals of the
  array a `for` loop walks; a suffix the script can't follow is reported so it gets a literal

Limits: a keyspace (a Thorium namespace) is at most `NAMESPACE_MAX_LEN` characters (read from
api/src/conf.rs; Scylla's own limit is 48), and an S3 bucket at most 63. The script also checks
that the harness code still builds names the way this model assumes, so a change to the run id or
namespace shape fails here until the model is updated.

Each finding is printed as a `path:line: level[rule]: message` line, like
.github/scripts/lint_report.py, and `--sarif` also writes them as SARIF for lint_report.py to
report. Exit status is 1 when any finding is an error and 0 otherwise. Only the standard library
is used. `--root` checks another checkout.
"""

import argparse
import ast
import bisect
import json
import re
import sys
from pathlib import Path

# the repository root, two levels above .github/scripts
DEFAULT_ROOT = Path(__file__).resolve().parents[2]
# the directories of integration test code, each with the crate whose harness names its tests
TEST_DIRS = {"api/tests": "api", "thoradm/tests": "thoradm", "thorctl/tests": "thorctl"}
# the longest S3 bucket name
BUCKET_MAX = 63
# where the namespace length limit is declared
NAMESPACE_LIMIT_FILE = "api/src/conf.rs"
NAMESPACE_LIMIT_RE = re.compile(r"pub const NAMESPACE_MAX_LEN: usize = (\d+);")
# a CI namespace at its full length, as the api harness and thoradm's base namespace build it
CI_NAMESPACE = "testing_ci_0000000_00000000"
# thoradm's run id in CI (<7 hex>-<5 hex>) as its harness uses it, with the dash made an underscore
CI_THORADM_RUN = "0000000_00000"
# the harness code each part of the model is read from, with the source each must still contain
MODEL_SOURCES = {
    "the api harness's CI namespace (testing_ci_<7 hex>_<8 hex>)": (
        "api/src/test_utilities/api.rs",
        [r'format!\("\{TEST_NAMESPACE_PREFIX\}ci_\{epoch_minutes:07x\}_\{random:08x\}"\)',
         r'TEST_NAMESPACE_PREFIX: &str = "testing_"'],
    ),
    "thoradm's CI base namespace (testing_ci_<7 hex>_<8 hex>)": (
        "thoradm/tests/common/base_namespace.rs",
        [r'format!\("\{TEST_PREFIX\}ci_\{epoch_minutes:07x\}_\{random:08x\}"\)'],
    ),
    "thoradm's CI run id (<7 hex>-<5 hex>)": (
        "thoradm/tests/common/harness.rs",
        [r"hex\(stamp, 7\) && hex\(random, 5\)"],
    ),
    "thoradm's per-binary names (testing_<run>_<binary>, testing-<run>-<binary>-<role>)": (
        "thoradm/tests/common/harness.rs",
        [r'format!\("\{\}_\{\}", run_id\(\), binary_name\(\)\)',
         r'format!\("testing_\{id\}"\)',
         r'"testing-\{\}-\{\}",\s*id\.replace',
         r"pub const BUCKET_ROLES: &\[&str\] = &\["],
    ),
}
# the bucket roles the api harness's buckets are named for, the longest of which a `{role}`
# placeholder in api test code stands for
API_BUCKET_ROLES = ["files", "repos", "results", "ephemeral", "attachments"]
# the rules a finding can break, as rule -> (level, description)
RULES = {
    "keyspace-too-long": ("error", "A keyspace CI derives is longer than the namespace limit"),
    "bucket-too-long": ("error", "An S3 bucket CI derives is longer than 63 characters"),
    "binary-name": ("error", "A thoradm test target's name can't name a keyspace and buckets"),
    "unmeasured": ("error", "A name derived from the CI namespace can't be measured statically"),
    "stale-model": ("error", "The harness no longer builds names the way this check assumes"),
}
# a Rust identifier
IDENT_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
# a function item's name, after its `fn` keyword
FN_RE = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")
# an impl block's self type
IMPL_RE = re.compile(r"\bimpl\b(?:\s*<[^{]*?>)?\s+(?:[\w:<>, ]+?\s+for\s+)?([A-Za-z_]\w*)[^{;]*\{")
# a `let` binding of a plain name
LET_RE = re.compile(r"\blet\s+(?:mut\s+)?([A-Za-z_]\w*)\s*(?::[^=;]+)?=\s*")
# a `for` loop's pattern and the array literal it walks
FOR_RE = re.compile(r"\bfor\s+(\(?[\w\s,]*\)?)\s+in\s+&?\[")
# the next comment, string, or quote in Rust source: a raw or byte string's prefix counts only at
# the start of a token
TOKEN_RE = re.compile(r'//|/\*|(?<![A-Za-z0-9_])b?r#*"|(?<![A-Za-z0-9_])b"|"|\'')
# a char literal
CHAR_RE = re.compile(r"'(\\u\{[0-9a-fA-F]+\}|\\.|[^\\'\n])'")
# the literal text a keyspace or bucket name can hold
NAME_TEXT_RE = re.compile(r"[A-Za-z0-9_-]*")
# a slice of a fixed length, such as `&unique[..8]`
SLICE_RE = re.compile(r"\[\s*\.\.\s*(\d+)\s*\]\s*$")


def mask(text):
    """Blank out comments and the contents of string and char literals, keeping every offset.

    Returns the masked text and the string literals as (start, end, value), where start and end
    span the whole literal including its quotes.

    # Arguments

    * `text` - The Rust source
    """
    out = list(text)
    strings = []
    length = len(text)
    i = 0
    while True:
        # jump to the next comment, string, or quote
        token = TOKEN_RE.search(text, i)
        if token is None:
            break
        i = token.start()
        kind = token.group(0)
        if kind == "//":
            end = text.find("\n", i)
            end = length if end < 0 else end
            blank(out, i, end)
            i = end
        elif kind == "/*":
            depth, j = 1, i + 2
            while j < length and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            blank(out, i, j)
            i = j
        elif kind.endswith('"') and "r" in kind:
            # a raw string ends at a quote followed by as many hashes as it opened with
            hashes = kind.count("#")
            start = token.end()
            end = text.find('"' + "#" * hashes, start)
            end = length if end < 0 else end
            strings.append((i, end + 1 + hashes, text[start:end]))
            blank(out, start, end)
            i = end + 1 + hashes
        elif kind.endswith('"'):
            start = token.end()
            j = start
            value = []
            while j < length and text[j] != '"':
                if text[j] == "\\" and j + 1 < length:
                    nxt = text[j + 1]
                    if nxt == "\n":
                        # a backslash before a newline drops the newline and the next line's indent
                        j += 2
                        while j < length and text[j] in " \t\n":
                            j += 1
                        continue
                    value.append({"n": "\n", "t": "\t", "r": "\r", "0": "\0"}.get(nxt, nxt))
                    j += 2
                    continue
                value.append(text[j])
                j += 1
            strings.append((i, j + 1, "".join(value)))
            blank(out, start, j)
            i = j + 1
        else:
            # a char literal ('x', '\n', '\u{1F600}'); otherwise a lifetime or label
            match = CHAR_RE.match(text, i)
            if match:
                blank(out, i + 1, match.end() - 1)
                i = match.end()
            else:
                i += 1
    return "".join(out), strings


def blank(out, start, end):
    """Replace a range of characters with spaces, keeping newlines so lines still count.

    # Arguments

    * `out` - The characters of the masked source
    * `start` - The first offset to blank
    * `end` - The offset after the last
    """
    for k in range(start, end):
        if out[k] != "\n":
            out[k] = " "


def matching(masked, start):
    """Return the offset just past the bracket that closes the one at `start`.

    # Arguments

    * `masked` - The masked source
    * `start` - The offset of an opening `(`, `[`, or `{`
    """
    pairs = {"(": ")", "[": "]", "{": "}"}
    stack = []
    for i in range(start, len(masked)):
        char = masked[i]
        if char in pairs:
            stack.append(pairs[char])
        elif char in ")]}":
            if not stack or stack.pop() != char:
                return i + 1
            if not stack:
                return i + 1
    return len(masked)


def split_args(masked, start, end):
    """Split the text between two offsets at its top-level commas, as (start, end) spans.

    # Arguments

    * `masked` - The masked source
    * `start` - The offset just inside the opening bracket
    * `end` - The offset of the closing bracket
    """
    spans = []
    depth = 0
    begin = start
    for i in range(start, end):
        char = masked[i]
        if char in "([{":
            depth += 1
        elif char in ")]}":
            depth -= 1
        elif char == "<" and depth == 0 and masked[begin:i].strip().endswith("::"):
            depth += 1
        elif char == "," and depth == 0:
            spans.append((begin, i))
            begin = i + 1
    if masked[begin:end].strip():
        spans.append((begin, end))
    return spans


class Source:
    """One Rust file of test code, masked and split into functions."""

    def __init__(self, root, rel, crate):
        """Read and index a file.

        # Arguments

        * `root` - The repository root
        * `rel` - The file's path relative to the root
        * `crate` - The crate whose tests the file belongs to
        """
        self.rel = rel
        self.crate = crate
        self.text = (root / rel).read_text(encoding="utf-8")
        self.masked, strings = mask(self.text)
        self.strings = {start: (end, value) for start, end, value in strings}
        self.line_starts = [0] + [m.end() for m in re.finditer("\n", self.text)]
        self.functions = sorted(self.find_functions(), key=lambda function: function["start"])
        self.function_starts = [function["start"] for function in self.functions]

    def line(self, offset):
        """Return the 1-based line of an offset.

        # Arguments

        * `offset` - The offset in the file
        """
        low, high = 0, len(self.line_starts)
        while low + 1 < high:
            middle = (low + high) // 2
            if self.line_starts[middle] <= offset:
                low = middle
            else:
                high = middle
        return low + 1

    def literal(self, start, end):
        """Return the value of the string literal spanning exactly start..end, or None.

        # Arguments

        * `start` - The start of the expression
        * `end` - The end of the expression
        """
        expr = self.masked[start:end]
        lead = len(expr) - len(expr.lstrip())
        begin = start + lead
        # a literal may be borrowed or turned into a String
        while self.masked[begin] == "&":
            begin += 1
        found = self.strings.get(begin)
        if found is None:
            return None
        literal_end, value = found
        rest = self.masked[literal_end:end].strip()
        if rest in ("", ".to_owned()", ".to_string()", ".into()"):
            return value
        return None

    def find_functions(self):
        """Find every function with its name, impl type, parameter names, and body span."""
        impls = []
        for match in IMPL_RE.finditer(self.masked):
            brace = match.end() - 1
            impls.append((brace, matching(self.masked, brace), match.group(1)))
        functions = []
        for match in FN_RE.finditer(self.masked):
            paren = self.masked.find("(", match.end())
            if paren < 0:
                continue
            params_end = matching(self.masked, paren) - 1
            params = []
            for start, end in split_args(self.masked, paren + 1, params_end):
                param = self.masked[start:end].split(":")[0].replace("mut ", "").strip()
                params.append(param if IDENT_RE.fullmatch(param) else None)
            body = re.compile(r"[{;]").search(self.masked, params_end)
            if body is None or body.group(0) == ";":
                continue
            body_end = matching(self.masked, body.start())
            owner = None
            for impl_start, impl_end, name in impls:
                if impl_start < match.start() < impl_end:
                    owner = name
            functions.append(
                {"name": match.group(1), "owner": owner, "params": params, "start": body.start(), "end": body_end}
            )
        return functions

    def enclosing(self, offset):
        """Return the innermost function whose body holds an offset, or None.

        # Arguments

        * `offset` - The offset in the file
        """
        found = None
        index = bisect.bisect_left(self.function_starts, offset)
        # the innermost function holding the offset is the one starting last before it
        for function in reversed(self.functions[:index]):
            if offset < function["end"]:
                found = function
                break
        return found


class Model:
    """The CI name lengths and limits, read from the checkout."""

    def __init__(self, root, findings):
        """Read the limits and check the harness still builds names as modelled.

        # Arguments

        * `root` - The repository root
        * `findings` - The list to add stale-model findings to
        """
        self.keyspace_max = 48
        limit_path = root / NAMESPACE_LIMIT_FILE
        match = NAMESPACE_LIMIT_RE.search(limit_path.read_text(encoding="utf-8")) if limit_path.exists() else None
        if match:
            self.keyspace_max = int(match.group(1))
        else:
            findings.append(finding("stale-model", NAMESPACE_LIMIT_FILE, 1, "NAMESPACE_MAX_LEN is not declared here"))
        for what, (rel, patterns) in MODEL_SOURCES.items():
            path = root / rel
            text = path.read_text(encoding="utf-8") if path.exists() else ""
            for pattern in patterns:
                if not re.search(pattern, text):
                    findings.append(
                        finding(
                            "stale-model",
                            rel,
                            1,
                            f"{what}: no longer matches /{pattern}/; update check_ci_name_limits.py to the new shape",
                        )
                    )
        harness = root / "thoradm/tests/common/harness.rs"
        roles = []
        if harness.exists():
            match = re.search(r"pub const BUCKET_ROLES: &\[&str\] = &\[(.*?)\];", harness.read_text(encoding="utf-8"), re.S)
            if match:
                roles = re.findall(r'"([^"]+)"', match.group(1))
        self.thoradm_roles = roles or ["reaction_cache"]
        self.roles = {"api": max(map(len, API_BUCKET_ROLES)), "thoradm": max(map(len, self.thoradm_roles))}
        self.roles["thorctl"] = self.roles["api"]
        self.binaries = thoradm_binaries(root)
        self.longest_binary = max(self.binaries.values(), key=len, default="")

    def thoradm_namespace(self, binary):
        """Return a thoradm harness namespace in CI for a test binary.

        # Arguments

        * `binary` - The test binary's name
        """
        return f"testing_{CI_THORADM_RUN}_{binary}"

    def file_binary(self, rel):
        """Return the thoradm test binary a file belongs to, the longest for shared modules.

        # Arguments

        * `rel` - The file's path relative to the root
        """
        parts = Path(rel).relative_to("thoradm/tests").parts
        if len(parts) == 1:
            return self.binaries.get(rel, Path(rel).stem)
        if parts[0] == "common":
            return self.longest_binary
        return parts[0]

    def limit(self, kind):
        """Return the limit of a kind of name.

        # Arguments

        * `kind` - `keyspace` or `bucket`
        """
        return self.keyspace_max if kind == "keyspace" else BUCKET_MAX


def thoradm_binaries(root):
    """Return thoradm's integration test targets as file -> binary name.

    Every top-level file of thoradm/tests is a target named for its stem, unless thoradm's
    Cargo.toml declares a `[[test]]` for it with another name.

    # Arguments

    * `root` - The repository root
    """
    tests = root / "thoradm/tests"
    binaries = {}
    if tests.is_dir():
        for path in sorted(tests.glob("*.rs")):
            binaries[f"thoradm/tests/{path.name}"] = path.stem
    manifest = root / "thoradm/Cargo.toml"
    if manifest.exists():
        for block in manifest.read_text(encoding="utf-8").split("[[test]]")[1:]:
            block = block.split("\n[")[0]
            name = re.search(r'^name\s*=\s*"([^"]+)"', block, re.M)
            path = re.search(r'^path\s*=\s*"([^"]+)"', block, re.M)
            if name:
                rel = f"thoradm/{path.group(1)}" if path else f"thoradm/tests/{name.group(1)}.rs"
                binaries[rel] = name.group(1)
    return binaries


def finding(rule, path, line, message):
    """Build one finding.

    # Arguments

    * `rule` - The rule broken, a key of RULES
    * `path` - The file, relative to the root
    * `line` - The 1-based line
    * `message` - What is wrong and how long the name is
    """
    return {"rule": rule, "level": RULES[rule][0], "path": path, "line": line, "message": message}


def check_binaries(model, findings):
    """Measure the keyspace and buckets thoradm's harness names for every test binary.

    # Arguments

    * `model` - The CI name model
    * `findings` - The list to add findings to
    """
    rows = []
    longest_role = max(model.thoradm_roles, key=len)
    # the longest binary name both its keyspace and its buckets have room for
    keyspace_room = model.keyspace_max - len(model.thoradm_namespace(""))
    bucket_room = BUCKET_MAX - len(f"testing-{CI_THORADM_RUN}--{longest_role}")
    room = min(keyspace_room, bucket_room)
    for rel, binary in sorted(model.binaries.items()):
        name = binary.lower()
        if not re.fullmatch(r"[a-z0-9_]+", name):
            findings.append(
                finding("binary-name", rel, 1, f"test target {binary!r} must be lowercase letters, digits, and underscores")
            )
            continue
        keyspace = model.thoradm_namespace(name)
        bucket = f"testing-{CI_THORADM_RUN}-{name}-{longest_role}".replace("_", "-")
        rows.append((rel, binary, len(keyspace), len(bucket)))
        for kind, derived, limit in (("keyspace", keyspace, model.keyspace_max), ("bucket", bucket, BUCKET_MAX)):
            if len(derived) > limit:
                findings.append(
                    finding(
                        f"{kind}-too-long",
                        rel,
                        1,
                        f"thoradm test binary {binary!r} makes the {kind} {derived} in CI: {len(derived)} characters, "
                        f"over {limit}; shorten the test target's name to at most {room} characters",
                    )
                )
    return rows


def placeholders(template):
    """Split a format string into literal text and placeholder names, in order.

    Returns a list of ("lit", text) and ("arg", name) parts, where a positional placeholder's
    name is its index as an int.

    # Arguments

    * `template` - The format string
    """
    parts = []
    position = 0
    text = []
    i = 0
    while i < len(template):
        char = template[i]
        if template.startswith("{{", i) or template.startswith("}}", i):
            text.append(char)
            i += 2
            continue
        if char == "{":
            end = template.find("}", i)
            if end < 0:
                break
            if text:
                parts.append(("lit", "".join(text)))
                text = []
            name = template[i + 1:end].split(":")[0].strip()
            if name == "":
                name = position
                position += 1
            elif name.isdigit():
                name = int(name)
            parts.append(("arg", name))
            i = end + 1
            continue
        text.append(char)
        i += 1
    if text:
        parts.append(("lit", "".join(text)))
    return parts


class Scanner:
    """Finds the names test code builds from the CI namespace and measures them."""

    def __init__(self, model, sources, findings):
        """Index the sources for the scan.

        # Arguments

        * `model` - The CI name model
        * `sources` - Every test source, keyed by path
        * `findings` - The list to add findings to
        """
        self.model = model
        self.sources = sources
        self.findings = findings
        # helper functions whose parameter ends up in a derived name, as
        # (path, function name, owner) -> {param index: [sink, ...]}
        self.helpers = {}
        # the names measured, for the notes: (path, line, kind, length, limit, sample)
        self.measured = []

    def base_length(self, source, function, expr, derived):
        """Return the CI length of a namespace-derived expression, or None if it isn't one.

        # Arguments

        * `source` - The file
        * `function` - The enclosing function, or None
        * `expr` - The expression's masked text
        * `derived` - Lengths of names derived earlier in the function, by variable
        """
        return self._base_length(source, function, expr, derived, set())

    def _base_length(self, source, function, expr, derived, seen):
        """Follow `base_length` through `let` bindings.

        # Arguments

        * `source` - The file
        * `function` - The enclosing function, or None
        * `expr` - The expression's masked text
        * `derived` - Lengths of names derived earlier in the function, by variable
        * `seen` - Variables already followed, to stop on a cycle
        """
        expr = expr.strip()
        root = IDENT_RE.search(expr.lstrip("&* "))
        if root and root.group(0) in derived:
            return derived[root.group(0)]
        if "base_namespace" in expr:
            return len(CI_NAMESPACE)
        if re.search(r"\bnamespace\b", expr) and not re.search(r"\bnamespace\s*\(", expr):
            if source.crate == "thoradm":
                return len(self.model.thoradm_namespace(self.model.file_binary(source.rel)))
            return len(CI_NAMESPACE)
        if function is not None and root and root.group(0) == expr.lstrip("&* ") and root.group(0) not in seen:
            seen.add(root.group(0))
            value = self.let_value(source, function, root.group(0))
            if value is not None:
                return self._base_length(source, function, value, derived, seen)
        return None

    def let_value(self, source, function, name):
        """Return the masked expression a function's `let` binds to a name, or None.

        # Arguments

        * `source` - The file
        * `function` - The function to look in
        * `name` - The variable
        """
        body = source.masked[function["start"]:function["end"]]
        for match in LET_RE.finditer(body):
            if match.group(1) == name:
                end = body.find(";", match.end())
                return body[match.end():end]
        return None

    def scan(self):
        """Measure every derived name, then follow helper parameters to their callers."""
        for source in self.sources.values():
            self.scan_formats(source)
        # a helper that passes its own parameter on to another helper is a helper too
        changed = True
        while changed:
            changed = False
            for key, params in list(self.helpers.items()):
                for index, sinks in list(params.items()):
                    for site in self.call_sites(key, index):
                        source, offset, arg_start, arg_end, function = site
                        arg = source.masked[arg_start:arg_end].strip().lstrip("&")
                        if function and IDENT_RE.fullmatch(arg) and arg in function["params"]:
                            outer = (source.rel, function["name"], function["owner"])
                            outer_index = function["params"].index(arg)
                            known = self.helpers.setdefault(outer, {}).setdefault(outer_index, [])
                            for sink in sinks:
                                if sink not in known:
                                    known.append(sink)
                                    changed = True
        for key, params in self.helpers.items():
            for index, sinks in params.items():
                for site in self.call_sites(key, index):
                    self.measure_call(site, sinks)

    def scan_formats(self, source):
        """Measure each `format!` in a file that builds a keyspace or bucket from the namespace.

        # Arguments

        * `source` - The file
        """
        derived_by_function = {}
        for match in re.finditer(r"\bformat!\s*\(", source.masked):
            paren = match.end() - 1
            close = matching(source.masked, paren) - 1
            args = split_args(source.masked, paren + 1, close)
            if not args:
                continue
            template = source.literal(*args[0])
            if template is None:
                continue
            parts = placeholders(template)
            if len(parts) < 2 or parts[0][0] != "arg" or parts[1][0] != "lit" or parts[1][1][:1] not in "_-":
                continue
            # a keyspace or bucket holds only name characters, so a Redis key, path, or query isn't one
            if any(kind == "lit" and not NAME_TEXT_RE.fullmatch(value) for kind, value in parts):
                continue
            function = source.enclosing(match.start())
            derived = derived_by_function.setdefault(id(function), {})
            named = {}
            positional = []
            for start, end in args[1:]:
                text = source.masked[start:end]
                assign = re.match(r"\s*([A-Za-z_]\w*)\s*=(?!=)", text)
                if assign:
                    named[assign.group(1)] = text[assign.end():]
                else:
                    positional.append(text)

            def expression(name):
                """Return the masked expression a placeholder stands for."""
                if isinstance(name, int):
                    return positional[name] if name < len(positional) else ""
                return named.get(name, name)

            base = self.base_length(source, function, expression(parts[0][1]), derived)
            if base is None:
                continue
            kind = "keyspace" if parts[1][1][0] == "_" else "bucket"
            line = source.line(match.start())
            fixed = base
            params = {}
            unmeasured = []
            for part_kind, value in parts[1:]:
                if part_kind == "lit":
                    fixed += len(value)
                    continue
                expr = expression(value).strip()
                slice_match = SLICE_RE.search(expr)
                if function and isinstance(value, str) and expr in function["params"]:
                    params[function["params"].index(expr)] = expr
                elif slice_match:
                    fixed += int(slice_match.group(1))
                elif expr == "role":
                    fixed += self.model.roles[source.crate]
                else:
                    unmeasured.append(expr or str(value))
            if unmeasured:
                self.findings.append(
                    finding(
                        "unmeasured",
                        source.rel,
                        line,
                        f"a {kind} built from the CI namespace holds {', '.join(unmeasured)}, which "
                        "check_ci_name_limits.py can't measure; use a literal or a fixed-length slice",
                    )
                )
                continue
            limit = self.model.limit(kind)
            # a name assigned to a variable may be the base of a later one
            assigned = re.search(r"\blet\s+(?:mut\s+)?([A-Za-z_]\w*)\s*(?::[^=;]+)?=\s*$", source.masked[:match.start()])
            if not params:
                self.record(source.rel, line, kind, fixed, limit, f"{template!r}")
                if assigned and source.masked[assigned.start():match.start()].count("\n") == 0:
                    derived[assigned.group(1)] = fixed
                continue
            if len(params) > 1:
                self.findings.append(
                    finding("unmeasured", source.rel, line, f"a {kind} holds several parameters ({', '.join(params.values())})")
                )
                continue
            index = next(iter(params))
            sink = {"kind": kind, "fixed": fixed, "limit": limit, "origin": f"{source.rel}:{line}", "template": template}
            key = (source.rel, function["name"], function["owner"])
            self.helpers.setdefault(key, {}).setdefault(index, []).append(sink)

    def record(self, rel, line, kind, length, limit, sample):
        """Keep a measured name and report it when it is over its limit.

        # Arguments

        * `rel` - The file the name is made in
        * `line` - The line it is made on
        * `kind` - `keyspace` or `bucket`
        * `length` - Its length in CI
        * `limit` - Its limit
        * `sample` - What makes it, for the message
        """
        self.measured.append((rel, line, kind, length, limit, sample))
        if length > limit:
            self.findings.append(
                finding(
                    f"{kind}-too-long",
                    rel,
                    line,
                    f"the {kind} {sample} is {length} characters in CI, over its limit of {limit}; shorten it by "
                    f"{length - limit}",
                )
            )

    def call_sites(self, key, index):
        """Yield every call of a helper as (source, offset, argument start, argument end, caller).

        A helper in a shared module (a `common` directory) is called from any file of its crate's
        tests; any other helper only from its own file.

        # Arguments

        * `key` - The helper as (path, function name, owner)
        * `index` - The parameter to return the argument of
        """
        rel, name, owner = key
        crate_dir = next(directory for directory in TEST_DIRS if rel.startswith(directory + "/"))
        shared = "/common/" in rel or rel.endswith("/common.rs")
        files = [
            source for path, source in self.sources.items() if path == rel or (shared and path.startswith(crate_dir + "/"))
        ]
        if owner:
            pattern = re.compile(rf"\b(?:{re.escape(owner)}|Self)::{re.escape(name)}\s*\(")
        else:
            pattern = re.compile(rf"(?<![\w.]){re.escape(name)}\s*\(")
        for source in files:
            for match in pattern.finditer(source.masked):
                before = source.masked[max(0, match.start() - 3):match.start()]
                if before.endswith("fn "):
                    continue
                if owner and match.group(0).startswith("Self::") and source.rel != rel:
                    continue
                paren = match.end() - 1
                close = matching(source.masked, paren) - 1
                args = split_args(source.masked, paren + 1, close)
                if index >= len(args):
                    continue
                yield source, match.start(), args[index][0], args[index][1], source.enclosing(match.start())

    def argument_values(self, source, function, start, end, depth=0):
        """Return the lengths an argument can have, or None when they can't be found.

        An argument is followed through string and integer literals, integer constant
        expressions, `let` bindings, the arrays `for` loops walk, `format!` strings, and (a few
        calls deep) the callers of the function whose parameter it is. A parameter passed on
        bare is measured at the callers instead, and gives no lengths here.

        # Arguments

        * `source` - The file of the call
        * `function` - The calling function, or None
        * `start` - The argument's start
        * `end` - The argument's end
        * `depth` - How many callers have been followed to get here
        """
        value = source.literal(start, end)
        if value is not None:
            return [len(value)]
        expr = source.masked[start:end].strip()
        while expr.startswith("&"):
            expr = expr[1:].strip()
        for suffix in (".to_owned()", ".to_string()", ".into()", ".as_str()"):
            if expr.endswith(suffix):
                expr = expr[: -len(suffix)]
        number = self.integer(source, expr)
        if number is not None:
            return [len(str(number))]
        offset = start + source.masked[start:end].find(expr)
        nested = re.match(r"format!\s*\(", expr)
        if nested:
            return self.format_lengths(source, function, offset + nested.end() - 1, depth)
        if function is None or not IDENT_RE.fullmatch(expr):
            return None
        if expr in function["params"]:
            if depth == 0:
                # a parameter passed on bare is measured at the forwarding function's callers
                return []
            return self.parameter_lengths(source, function, function["params"].index(expr), depth)
        bound = self.let_value(source, function, expr)
        if bound is not None:
            begin = function["start"] + source.masked[function["start"]:function["end"]].find(bound)
            return self.argument_values(source, function, begin, begin + len(bound), depth + 1)
        for match in FOR_RE.finditer(source.masked, function["start"], function["end"]):
            names = [part.strip() for part in match.group(1).strip("() ").split(",")]
            if expr not in names:
                continue
            position = names.index(expr)
            bracket = match.end() - 1
            close = matching(source.masked, bracket) - 1
            lengths = []
            for item_start, item_end in split_args(source.masked, bracket + 1, close):
                item = source.masked[item_start:item_end].strip()
                if len(names) > 1 or item.startswith("("):
                    paren = source.masked.index("(", item_start)
                    fields = split_args(source.masked, paren + 1, matching(source.masked, paren) - 1)
                    if position >= len(fields):
                        return None
                    item_start, item_end = fields[position]
                found = self.argument_values(source, function, item_start, item_end, depth + 1)
                if found is None:
                    return None
                lengths.extend(found)
            return lengths
        return None

    def parameter_lengths(self, source, function, index, depth):
        """Return the lengths a function's parameter takes at its callers, or None.

        # Arguments

        * `source` - The file of the function
        * `function` - The function
        * `index` - The parameter
        * `depth` - How many callers have been followed to get here
        """
        if depth > 3:
            return None
        lengths = []
        key = (source.rel, function["name"], function["owner"])
        for caller, _, start, end, outer in self.call_sites(key, index):
            found = self.argument_values(caller, outer, start, end, depth + 1)
            if found is None:
                return None
            lengths.extend(found)
        return lengths or None

    def format_lengths(self, source, function, paren, depth):
        """Return the longest length a `format!` string can have, as a one-item list, or None.

        # Arguments

        * `source` - The file
        * `function` - The enclosing function, or None
        * `paren` - The offset of the `format!` call's opening parenthesis
        * `depth` - How many callers have been followed to get here
        """
        close = matching(source.masked, paren) - 1
        args = split_args(source.masked, paren + 1, close)
        template = source.literal(*args[0]) if args else None
        if template is None:
            return None
        positional = []
        named = {}
        for arg_start, arg_end in args[1:]:
            assign = re.match(r"\s*([A-Za-z_]\w*)\s*=(?!=)", source.masked[arg_start:arg_end])
            if assign:
                named[assign.group(1)] = (arg_start + assign.end(), arg_end)
            else:
                positional.append((arg_start, arg_end))
        total = 0
        for kind, value in placeholders(template):
            if kind == "lit":
                total += len(value)
                continue
            if isinstance(value, int):
                if value >= len(positional):
                    return None
                span = positional[value]
            elif value in named:
                span = named[value]
            else:
                # an inline capture names a variable of the enclosing function
                body = source.masked[function["start"]:function["end"]] if function else ""
                found = re.search(rf"\b{re.escape(value)}\b", body)
                if found is None:
                    return None
                begin = function["start"] + found.start()
                span = (begin, begin + len(value))
            lengths = self.argument_values(source, function, span[0], span[1], depth + 1)
            if not lengths:
                return None
            total += max(lengths)
        return [total]

    def integer(self, source, expr):
        """Return the value of an integer constant expression, or None.

        Integer literals, the file's integer `const`s, parentheses, and `+ - *` are understood.

        # Arguments

        * `source` - The file
        * `expr` - The expression's masked text
        """
        consts = dict(re.findall(r"\bconst\s+([A-Z_][A-Z0-9_]*)\s*:\s*[iu](?:8|16|32|64|128|size)\s*=\s*([^;]+);", source.masked))
        text = expr
        for _ in range(5):
            replaced = re.sub(r"\b[A-Z_][A-Z0-9_]*\b", lambda m: f"({consts[m.group(0)]})" if m.group(0) in consts else m.group(0), text)
            if replaced == text:
                break
            text = replaced
        text = re.sub(r"(\d)_?(?:[iu](?:8|16|32|64|128|size))\b", r"\1", text).replace("_", "")
        if not text.strip() or not re.fullmatch(r"[\d\s+\-*()]+", text):
            return None
        try:
            tree = ast.parse(text, mode="eval")
        except SyntaxError:
            return None
        allowed = (ast.Expression, ast.BinOp, ast.UnaryOp, ast.Constant, ast.Add, ast.Sub, ast.Mult, ast.USub)
        if not all(isinstance(node, allowed) for node in ast.walk(tree)):
            return None
        return int(eval(compile(tree, "<const>", "eval"), {"__builtins__": {}}))  # noqa: S307

    def measure_call(self, site, sinks):
        """Measure the names one call of a helper makes.

        # Arguments

        * `site` - The call, as `call_sites` yields it
        * `sinks` - The names the helper builds from the argument
        """
        source, offset, start, end, function = site
        values = self.argument_values(source, function, start, end)
        line = source.line(offset)
        arg = " ".join(source.text[start:end].split())
        if values is None:
            self.findings.append(
                finding(
                    "unmeasured",
                    source.rel,
                    line,
                    f"{arg} goes into the name built at {sinks[0]['origin']}, and check_ci_name_limits.py can't "
                    "find its value; pass a string literal, a `let` of one, or a `for` over an array of them",
                )
            )
            return
        for length in values:
            for sink in sinks:
                self.record(
                    source.rel,
                    line,
                    sink["kind"],
                    sink["fixed"] + length,
                    sink["limit"],
                    f"{sink['template']!r} with {arg} (built at {sink['origin']})",
                )


def collect_sources(root):
    """Read every Rust file of the integration test directories.

    # Arguments

    * `root` - The repository root
    """
    sources = {}
    for directory, crate in TEST_DIRS.items():
        base = root / directory
        if not base.is_dir():
            continue
        for path in sorted(base.rglob("*.rs")):
            rel = path.relative_to(root).as_posix()
            sources[rel] = Source(root, rel, crate)
    return sources


def check(root):
    """Run every check on a checkout, returning (findings, binary rows, measured names).

    # Arguments

    * `root` - The repository root
    """
    findings = []
    model = Model(root, findings)
    rows = check_binaries(model, findings)
    scanner = Scanner(model, collect_sources(root), findings)
    scanner.scan()
    findings.sort(key=lambda item: (item["path"], item["line"], item["rule"], item["message"]))
    unique = []
    for item in findings:
        if not unique or unique[-1] != item:
            unique.append(item)
    return unique, rows, sorted(set(scanner.measured))


def to_sarif(findings):
    """Build a SARIF log of the findings.

    # Arguments

    * `findings` - The findings
    """
    rules = [
        {"id": rule, "shortDescription": {"text": description}, "defaultConfiguration": {"level": level}}
        for rule, (level, description) in RULES.items()
    ]
    results = [
        {
            "ruleId": item["rule"],
            "level": item["level"],
            "message": {"text": item["message"]},
            "locations": [
                {"physicalLocation": {"artifactLocation": {"uri": item["path"]}, "region": {"startLine": item["line"]}}}
            ],
        }
        for item in findings
    ]
    return {
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{"tool": {"driver": {"name": "ci-name-limits", "rules": rules}}, "results": results}],
    }


def main(argv=None):
    """Check the checkout, print the findings, and optionally write them as SARIF.

    # Arguments

    * `argv` - The command line arguments, or None for sys.argv
    """
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT, help="the checkout to check")
    parser.add_argument("--sarif", type=Path, help="where to write the findings as SARIF")
    parser.add_argument("--list", action="store_true", help="also print every measured name and its length")
    args = parser.parse_args(argv)
    root = args.root.resolve()
    findings, rows, measured = check(root)
    if args.list:
        for rel, binary, keyspace, bucket in rows:
            print(f"binary {binary}: keyspace {keyspace}, bucket {bucket} ({rel})")
        for rel, line, kind, length, limit, sample in measured:
            print(f"{rel}:{line}: {kind} {length}/{limit} {sample}")
    for item in findings:
        print(f"{item['path']}:{item['line']}: {item['level']}[{item['rule']}]: {item['message']}")
    if args.sarif:
        args.sarif.parent.mkdir(parents=True, exist_ok=True)
        args.sarif.write_text(json.dumps(to_sarif(findings), indent=2) + "\n", encoding="utf-8")
    errors = sum(item["level"] == "error" for item in findings)
    print(
        f"checked {len(rows)} thoradm test binaries and {len(measured)} derived names; "
        f"{len(findings)} findings, {errors} errors"
    )
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
