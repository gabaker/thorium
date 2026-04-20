#!/usr/bin/env python3

import argparse
import logging
import os
import re
import shutil
import sys
import tempfile
from collections import Counter, defaultdict
from collections.abc import Sequence
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import NoReturn

DEFAULT_FORGE_RULE_DIR = Path("/app/rules/yara-forge")
DEFAULT_CUSTOM_RULE_DIR = Path("/app/rules/custom")

# The script uses a flat file discovery model so validation aligns with a simple
# `yarac /path/to/forge/* /path/to/custom/*` compilation approach.
SUPPORTED_RULE_SUFFIXES = {".yar", ".yara"}

CUSTOM_RENAME_SUFFIX = "_duplicate"

IDENTIFIER_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")

# Rule declarations define the names that must be unique across the combined
# ruleset. The prefix and suffix captures allow declaration rewrites without
# changing modifiers or local formatting around the rule name.
RULE_DECL_RE = re.compile(
    r"^(?P<prefix>[ \t]*(?:(?:private|global)[ \t]+)*rule[ \t]+)"
    r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)"
    r"(?P<suffix>\b)",
    re.MULTILINE,
)

# Identifiers with these prefixes are YARA string variables, string counts,
# string offsets, string lengths, or object members rather than standalone rule
# dependencies.
REFERENCE_SKIP_PREFIX_CHARS = frozenset(".$#@!")

logger = logging.getLogger(__name__)


@dataclass(frozen=True)
class RuleCollection:
    """Rule files, source text, and declarations from one rule source."""

    label: str
    directory: Path
    files: list[Path]
    text_by_path: dict[Path, str]
    counts: Counter[str]
    origins: defaultdict[str, list[Path]]

    @property
    def names(self) -> set[str]:
        """Return the unique rule names in the collection."""
        return set(self.counts)


def configure_logging(verbose: bool = False) -> None:
    """Configure logging for command-line execution.

    Concise output is appropriate for normal build logs. Debug output provides
    file-discovery, per-rule validation, collision, and rewrite details.
    """
    level = logging.DEBUG if verbose else logging.INFO

    logging.basicConfig(
        level=level,
        format="[%(levelname)s] %(message)s",
        force=True,
    )


def fail(message: str) -> NoReturn:
    """Report an unrecoverable validation error and exit with failure status.

    Rule preparation stops when the script cannot safely produce a name-unique
    custom ruleset for later compilation.
    """
    logger.error(message)
    sys.exit(1)


def read_text(path: Path) -> str:
    """Read a rule file as UTF-8 text.

    Strict decoding prevents malformed bytes from being silently changed before
    parsing or rewrite decisions are made.
    """
    try:
        return path.read_text(encoding="utf-8")
    except UnicodeDecodeError as exc:
        fail(f"Could not decode {path} as UTF-8: {exc}")
    except OSError as exc:
        fail(f"Could not read {path}: {exc}")


def validate_rename_suffix(suffix: str) -> None:
    """Validate that generated rule names remain valid YARA identifiers.

    The suffix is appended to existing valid identifiers, so it only needs
    characters that are legal after the first identifier character.
    """
    if not suffix:
        fail("Rename suffix must not be empty.")

    if not re.fullmatch(r"[A-Za-z0-9_]+", suffix):
        fail(
            "Rename suffix may only contain ASCII letters, digits, and underscores: "
            f"{suffix}"
        )

    logger.debug("Rename suffix validated: %s", suffix)


def find_yara_files(rule_dir: Path) -> list[Path]:
    """Find flat-layout YARA rule files in a directory.

    Only immediate `.yar` and `.yara` files are returned so validation matches a
    flat directory compilation model.
    """
    return sorted(
        path
        for path in rule_dir.iterdir()
        if path.is_file() and path.suffix.lower() in SUPPORTED_RULE_SUFFIXES
    )


def log_discovered_files(collection_label: str, files: Sequence[Path]) -> None:
    """Log discovered rule files for verbose troubleshooting.

    File-level visibility helps identify layout problems before rule-level
    validation messages are interpreted.
    """
    for path in files:
        logger.debug("Discovered %s rule file: %s", collection_label, path)


def validate_rule_directory(path: Path, label: str) -> None:
    """Validate that a rule directory exists and is usable.

    Early directory validation produces clear diagnostics before parsing starts.
    """
    if not path.exists():
        fail(f"{label} rule directory does not exist: {path}")

    if not path.is_dir():
        fail(f"{label} rule path is not a directory: {path}")


def ensure_distinct_rule_directories(
    forge_rule_dir: Path,
    custom_rule_dir: Path,
) -> None:
    """Ensure forge and custom rules are loaded from different directories.

    Distinct directories keep upstream ownership and custom rewrite ownership
    unambiguous.
    """
    forge_resolved = forge_rule_dir.resolve()
    custom_resolved = custom_rule_dir.resolve()

    if forge_resolved == custom_resolved:
        fail(
            "YARA-Forge and custom rule directories must be different paths: "
            f"{forge_resolved}"
        )


def load_rule_texts(paths: Sequence[Path]) -> dict[Path, str]:
    """Read rule files once so validation and rewrite planning share input text.

    Reusing the same in-memory contents avoids inconsistent decisions if files
    change during a run.
    """
    return {path: read_text(path) for path in paths}


def extract_rule_names_from_text(text: str) -> list[str]:
    """Extract YARA rule names from text.

    Duplicate names are preserved so validation can reject ambiguous inputs
    before planning any rewrite.
    """
    return [match.group("name") for match in RULE_DECL_RE.finditer(text)]


def collect_rule_names(
    text_by_path: dict[Path, str],
    collection_label: str,
) -> tuple[Counter[str], defaultdict[str, list[Path]]]:
    """Collect rule-name counts and source paths from rule text.

    Counts identify duplicate declarations. Source paths make collision and
    duplicate diagnostics actionable. Debug logging records each declaration
    discovered for validation.
    """
    counts: Counter[str] = Counter()
    origins: defaultdict[str, list[Path]] = defaultdict(list)

    for path in sorted(text_by_path):
        rule_names = extract_rule_names_from_text(text_by_path[path])

        if not rule_names:
            logger.debug(
                "No %s rule declarations found in %s",
                collection_label,
                path,
            )

        for name in rule_names:
            counts[name] += 1
            origins[name].append(path)

            logger.debug(
                "Discovered %s rule declaration for validation: %s in %s",
                collection_label,
                name,
                path,
            )

    return counts, origins


def format_origins(name: str, origins: defaultdict[str, list[Path]]) -> str:
    """Format rule source paths for diagnostic messages.

    Multiple declarations in the same file are reported distinctly so the
    maintainer can identify whether the problem is isolated to one file.
    """
    occurrence_counts = Counter(origins[name])
    formatted_origins: list[str] = []

    for path, count in sorted(occurrence_counts.items(), key=lambda item: str(item[0])):
        if count == 1:
            formatted_origins.append(str(path))
        else:
            formatted_origins.append(f"{path} ({count} declarations)")

    return ", ".join(formatted_origins)


def fail_on_duplicate_rules(
    label: str,
    counts: Counter[str],
    origins: defaultdict[str, list[Path]],
) -> None:
    """Fail if a rule collection contains duplicate rule names.

    Dependency rewriting requires every rule name in a collection to have one
    defining declaration.
    """
    duplicate_rules = {
        name: count
        for name, count in counts.items()
        if count > 1
    }

    if not duplicate_rules:
        logger.info("No duplicate rule names found in %s rules.", label)
        return

    details = "; ".join(
        f"{name} appears {count} times in {format_origins(name, origins)}"
        for name, count in sorted(duplicate_rules.items())
    )

    fail(f"Duplicate rule names found in {label} rules: {details}")


def log_validated_unique_rules(
    collection_label: str,
    origins: defaultdict[str, list[Path]],
) -> None:
    """Log rule names that have passed collection-level uniqueness validation.

    These messages are emitted after duplicate checks so verbose output can
    distinguish discovered declarations from rule names accepted as unique.
    """
    for rule_name in sorted(origins):
        logger.debug(
            "Validated unique %s rule: %s in %s",
            collection_label,
            rule_name,
            format_origins(rule_name, origins),
        )


def collect_rule_collection(label: str, directory: Path) -> RuleCollection:
    """Discover, read, and validate one flat rule directory.

    Grouping discovery and duplicate validation keeps the main workflow focused
    on cross-collection collision handling.
    """
    validate_rule_directory(directory, label)

    files = find_yara_files(directory)
    log_discovered_files(label, files)

    if not files:
        suffixes = ", ".join(sorted(SUPPORTED_RULE_SUFFIXES))
        fail(
            f"No supported rule files ({suffixes}) found in "
            f"{label} directory: {directory}"
        )

    logger.info("Found %d %s rule file(s).", len(files), label)

    text_by_path = load_rule_texts(files)
    counts, origins = collect_rule_names(text_by_path, collection_label=label)

    logger.info(
        "Found %d %s rule declaration(s) across %d unique rule name(s).",
        sum(counts.values()),
        label,
        len(counts),
    )

    if not counts:
        suffixes = ", ".join(sorted(SUPPORTED_RULE_SUFFIXES))
        fail(f"No YARA rules found in {label} rule files ({suffixes}).")

    fail_on_duplicate_rules(label, counts, origins)
    log_validated_unique_rules(label, origins)

    return RuleCollection(
        label=label,
        directory=directory,
        files=files,
        text_by_path=text_by_path,
        counts=counts,
        origins=origins,
    )


def log_collision_details(
    collisions: set[str],
    custom_origins: defaultdict[str, list[Path]],
    forge_origins: defaultdict[str, list[Path]],
) -> None:
    """Log collision source details for auditability.

    Collision diagnostics include both sides of each conflict so maintainers can
    identify whether the conflict comes from an upstream addition or local rule
    naming.
    """
    for rule_name in sorted(collisions):
        logger.info(
            "Collision detail for %s: custom=%s; YARA-Forge=%s",
            rule_name,
            format_origins(rule_name, custom_origins),
            format_origins(rule_name, forge_origins),
        )


def is_identifier_start(char: str) -> bool:
    """Return whether a character can begin a YARA identifier."""
    return char == "_" or char.isalpha()


def is_identifier_part(char: str) -> bool:
    """Return whether a character can appear within a YARA identifier."""
    return char == "_" or char.isalnum()


def skip_line_comment(text: str, start: int) -> int:
    """Return the first index after a line comment.

    Comments are excluded from reference rewrites so documentation and disabled
    examples remain unchanged.
    """
    newline_index = text.find("\n", start)

    if newline_index == -1:
        return len(text)

    return newline_index


def skip_block_comment(text: str, start: int) -> int:
    """Return the first index after a block comment.

    Unterminated comments are left for the later YARA compiler step to diagnose.
    """
    end_index = text.find("*/", start + 2)

    if end_index == -1:
        return len(text)

    return end_index + 2


def skip_quoted_string(text: str, start: int, quote_char: str) -> int:
    """Return the first index after a quoted string.

    Escaped characters are honored so embedded quote characters do not terminate
    the skipped region prematurely.
    """
    index = start + 1

    while index < len(text):
        char = text[index]

        if char == "\\":
            index += 2
            continue

        if char == quote_char:
            return index + 1

        index += 1

    return len(text)


def skip_regex_literal(text: str, start: int) -> int:
    """Return the first index after a slash-delimited regex literal.

    Regex bodies can contain text that resembles rule names, braces, or syntax
    delimiters, so they are skipped by structural scanning and reference rewrite
    logic.
    """
    index = start + 1
    in_character_class = False

    while index < len(text):
        char = text[index]

        if char == "\\":
            index += 2
            continue

        if char == "[":
            in_character_class = True
            index += 1
            continue

        if char == "]":
            in_character_class = False
            index += 1
            continue

        if char == "/" and not in_character_class:
            index += 1

            while index < len(text) and text[index].isalpha():
                index += 1

            return index

        index += 1

    return len(text)


def regex_can_follow(previous_significant_token: str | None) -> bool:
    """Return whether a slash is likely to begin a YARA regex literal.

    The heuristic covers common YARA regex contexts without treating every slash
    as a regex delimiter.
    """
    return previous_significant_token in {"=", "matches"}


def skip_ignored_region(
    text: str,
    index: int,
    previous_significant_token: str | None = None,
    *,
    allow_regex: bool = True,
) -> int | None:
    """Return the end of a region that should not be structurally interpreted.

    Comments, quoted strings, and likely regex literals can contain text that
    resembles YARA syntax but should not affect rule-body parsing or dependency
    rewriting.
    """
    char = text[index]
    next_char = text[index + 1] if index + 1 < len(text) else ""

    if char == "/" and next_char == "/":
        return skip_line_comment(text, index)

    if char == "/" and next_char == "*":
        return skip_block_comment(text, index)

    if char in {'"', "'"}:
        return skip_quoted_string(text, index, char)

    if allow_regex and char == "/" and regex_can_follow(previous_significant_token):
        return skip_regex_literal(text, index)

    return None


def find_rule_open_brace(text: str, start: int) -> int | None:
    """Find the structural opening brace for a rule body.

    Comments and strings between the declaration and body are ignored so braces
    in non-structural text are not mistaken for the rule body boundary.
    """
    index = start

    while index < len(text):
        skipped_index = skip_ignored_region(
            text,
            index,
            allow_regex=False,
        )

        if skipped_index is not None:
            index = skipped_index
            continue

        if text[index] == "{":
            return index

        index += 1

    return None


def find_matching_brace(text: str, open_brace_index: int) -> int | None:
    """Find the structural closing brace for a YARA rule body.

    Comments, quoted strings, and common regex literals are skipped so braces in
    non-structural text do not terminate the rule body incorrectly. Nested brace
    pairs inside the rule body are balanced by depth tracking.
    """
    depth = 0
    index = open_brace_index
    previous_significant_token: str | None = None

    while index < len(text):
        skipped_index = skip_ignored_region(
            text,
            index,
            previous_significant_token,
        )

        if skipped_index is not None:
            index = skipped_index
            previous_significant_token = None
            continue

        char = text[index]

        if is_identifier_start(char):
            end_index = index + 1

            while end_index < len(text) and is_identifier_part(text[end_index]):
                end_index += 1

            previous_significant_token = text[index:end_index]
            index = end_index
            continue

        if char == "{":
            depth += 1
            previous_significant_token = "{"
        elif char == "}":
            depth -= 1

            if depth == 0:
                return index

            previous_significant_token = "}"
        elif char.isspace():
            pass
        else:
            previous_significant_token = char

        index += 1

    return None


def update_line_start_state(
    text: str,
    start: int,
    end: int,
    current_state: bool,
) -> bool:
    """Update line-start state after a skipped region.

    Condition headings are only recognized at line starts, so skipped comments
    and strings still need to preserve enough newline context for later scanning.
    """
    skipped_text = text[start:end]

    if "\n" not in skipped_text:
        return current_state

    trailing_text = skipped_text.rsplit("\n", maxsplit=1)[1]
    return all(char in {" ", "\t"} for char in trailing_text)


def find_condition_heading(rule_body: str, rule_name: str) -> tuple[int, int]:
    """Find the single condition heading in a rule body.

    Dependency rewriting requires a precise condition region. Comments, quoted
    strings, and common regex literals are ignored so explanatory text and
    literals do not define the rewrite boundary.
    """
    matches: list[tuple[int, int]] = []
    index = 0
    at_line_start = True
    previous_significant_token: str | None = None

    while index < len(rule_body):
        skipped_index = skip_ignored_region(
            rule_body,
            index,
            previous_significant_token,
        )

        if skipped_index is not None:
            at_line_start = update_line_start_state(
                rule_body,
                index,
                skipped_index,
                at_line_start,
            )
            index = skipped_index
            previous_significant_token = None
            continue

        char = rule_body[index]

        if at_line_start:
            candidate_index = index

            while (
                candidate_index < len(rule_body)
                and rule_body[candidate_index] in {" ", "\t"}
            ):
                candidate_index += 1

            if rule_body.startswith("condition", candidate_index):
                after_keyword = candidate_index + len("condition")

                while (
                    after_keyword < len(rule_body)
                    and rule_body[after_keyword] in {" ", "\t"}
                ):
                    after_keyword += 1

                if after_keyword < len(rule_body) and rule_body[after_keyword] == ":":
                    matches.append((candidate_index, after_keyword + 1))

        if is_identifier_start(char):
            end_index = index + 1

            while end_index < len(rule_body) and is_identifier_part(rule_body[end_index]):
                end_index += 1

            previous_significant_token = rule_body[index:end_index]
            at_line_start = False
            index = end_index
            continue

        if char == "\n":
            at_line_start = True
            previous_significant_token = None
        elif char in {" ", "\t"}:
            pass
        else:
            at_line_start = False
            previous_significant_token = char

        index += 1

    if len(matches) != 1:
        fail(
            f"Expected exactly one condition section in rule {rule_name}, "
            f"found {len(matches)}."
        )

    return matches[0]


def iter_condition_body_spans(text: str) -> list[tuple[int, int]]:
    """Return condition-body spans for all YARA rules in a file.

    Dependency names are rewritten only in condition bodies because rule-to-rule
    dependencies are expressed there.
    """
    spans: list[tuple[int, int]] = []

    for rule_match in RULE_DECL_RE.finditer(text):
        rule_name = rule_match.group("name")
        open_brace_index = find_rule_open_brace(text, rule_match.end())

        if open_brace_index is None:
            fail(f"Could not find opening brace for rule declaration: {rule_name}")

        close_brace_index = find_matching_brace(text, open_brace_index)

        if close_brace_index is None:
            fail(f"Could not find closing brace for rule declaration: {rule_name}")

        rule_body_start = open_brace_index + 1
        rule_body = text[rule_body_start:close_brace_index]
        _, condition_heading_end = find_condition_heading(rule_body, rule_name)

        spans.append((rule_body_start + condition_heading_end, close_brace_index))

        logger.debug(
            "Identified condition body for dependency rewrite: rule=%s span=%s:%s",
            rule_name,
            rule_body_start + condition_heading_end,
            close_brace_index,
        )

    return spans


def should_replace_identifier(segment: str, start: int, end: int) -> bool:
    """Return whether an identifier token may be a rule dependency.

    The surrounding-character checks avoid rewriting YARA string references,
    string counters, string offsets, string lengths, and module/object fields.
    """
    previous_char = segment[start - 1] if start > 0 else ""
    next_char = segment[end] if end < len(segment) else ""

    if previous_char in REFERENCE_SKIP_PREFIX_CHARS:
        return False

    if next_char == ".":
        return False

    return True


def replace_rule_identifiers(
    segment: str,
    rename_map: dict[str, str],
    file_path: Path,
) -> str:
    """Replace renamed rule references inside a condition-body segment.

    Comments, quoted strings, and common regex literals are preserved so literal
    values and explanatory text are not treated as dependencies.
    """
    output: list[str] = []
    index = 0
    previous_significant_token: str | None = None

    while index < len(segment):
        skipped_index = skip_ignored_region(
            segment,
            index,
            previous_significant_token,
        )

        if skipped_index is not None:
            output.append(segment[index:skipped_index])
            index = skipped_index
            previous_significant_token = None
            continue

        char = segment[index]

        if is_identifier_start(char):
            end_index = index + 1

            while end_index < len(segment) and is_identifier_part(segment[end_index]):
                end_index += 1

            identifier = segment[index:end_index]

            if identifier in rename_map and should_replace_identifier(
                segment,
                index,
                end_index,
            ):
                logger.debug(
                    "Rewriting custom rule dependency in %s: %s -> %s",
                    file_path,
                    identifier,
                    rename_map[identifier],
                )
                output.append(rename_map[identifier])
            else:
                output.append(identifier)

            previous_significant_token = identifier
            index = end_index
            continue

        output.append(char)

        if not char.isspace():
            previous_significant_token = char

        index += 1

    return "".join(output)


def rename_rule_references_in_conditions(
    text: str,
    rename_map: dict[str, str],
    file_path: Path,
) -> str:
    """Rewrite renamed-rule dependencies in every custom rule condition.

    Replacement spans are applied from the end of the file toward the beginning
    so earlier edits do not shift offsets for condition spans that have not yet
    been processed.
    """
    updated_text = text

    for start, end in reversed(iter_condition_body_spans(text)):
        updated_segment = replace_rule_identifiers(
            updated_text[start:end],
            rename_map,
            file_path,
        )
        updated_text = updated_text[:start] + updated_segment + updated_text[end:]

    return updated_text


def build_unique_renamed_rule_name(
    rule_name: str,
    suffix: str,
    reserved_names: set[str],
) -> str:
    """Create a unique replacement name for a colliding custom rule.

    Existing names from both rule collections and earlier planned replacements
    are reserved so the rename plan resolves all collisions in one pass.
    """
    base_name = f"{rule_name}{suffix}"
    candidate = base_name
    counter = 2

    while candidate in reserved_names:
        logger.debug(
            "Rename candidate already reserved for %s: %s",
            rule_name,
            candidate,
        )
        candidate = f"{base_name}_{counter}"
        counter += 1

    if not IDENTIFIER_RE.fullmatch(candidate):
        fail(f"Generated invalid YARA rule name for {rule_name}: {candidate}")

    return candidate


def rename_custom_rule_declaration(
    custom_text: str,
    old_rule_name: str,
    new_rule_name: str,
) -> str:
    """Rename exactly one custom YARA rule declaration.

    The single-replacement requirement protects against ambiguous edits when
    validation assumptions and file contents disagree.
    """
    declaration_re = re.compile(
        r"^(?P<prefix>[ \t]*(?:(?:private|global)[ \t]+)*rule[ \t]+)"
        + re.escape(old_rule_name)
        + r"(?P<suffix>\b)",
        re.MULTILINE,
    )

    def replacement(match: re.Match[str]) -> str:
        """Preserve declaration formatting while replacing the rule name."""
        return f"{match.group('prefix')}{new_rule_name}{match.group('suffix')}"

    updated_text, replacement_count = declaration_re.subn(
        replacement,
        custom_text,
        count=1,
    )

    if replacement_count != 1:
        fail(
            f"Expected to rename exactly one declaration for {old_rule_name}, "
            f"but renamed {replacement_count}."
        )

    return updated_text


def atomic_write_text(path: Path, text: str) -> None:
    """Write text using a temporary file followed by atomic replacement.

    Atomic replacement reduces the risk of leaving a partially written custom
    rule file if the process is interrupted during an update.
    """
    fd, temp_name = tempfile.mkstemp(
        prefix=f".{path.name}.",
        suffix=".tmp",
        dir=path.parent,
    )

    temp_path = Path(temp_name)

    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(text)
            handle.flush()
            os.fsync(handle.fileno())

        # Collision resolution should not unexpectedly change file permissions
        # or other metadata that may affect later pipeline steps.
        shutil.copystat(path, temp_path)

        os.replace(temp_path, path)

    except OSError:
        # Cleanup should not mask the write failure that explains why the update
        # could not be completed.
        try:
            temp_path.unlink(missing_ok=True)
        except OSError:
            logger.exception(
                "Failed to remove temporary file after write failure: %s",
                temp_path,
            )

        raise


def make_backup(path: Path) -> Path:
    """Create a timestamped backup of a custom rule file.

    A unique backup per modified file preserves the exact pre-update content for
    troubleshooting or rollback.
    """
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    backup_path = path.with_name(f"{path.name}.{timestamp}.bak")

    try:
        shutil.copy2(path, backup_path)
    except OSError as exc:
        fail(f"Could not create backup file {backup_path}: {exc}")

    return backup_path


def get_unique_origin(
    rule_name: str,
    origins: defaultdict[str, list[Path]],
) -> Path:
    """Return the single custom file that defines a rule.

    Duplicate validation ensures this lookup has one valid answer during rewrite
    planning.
    """
    origin_paths = origins[rule_name]

    if len(origin_paths) != 1:
        fail(
            f"Expected exactly one custom origin for {rule_name}, "
            f"but found {len(origin_paths)}."
        )

    return origin_paths[0]


def build_rename_map(
    collisions: set[str],
    custom_rules: set[str],
    forge_rules: set[str],
    rename_suffix: str,
) -> dict[str, str]:
    """Create replacement names for all colliding custom rules.

    All custom rules are eligible for renaming. Replacement names are generated
    from a shared reservation set so the final combined ruleset remains
    name-unique.
    """
    reserved_names = set(custom_rules) | set(forge_rules)
    rename_map: dict[str, str] = {}

    for old_rule_name in sorted(collisions):
        new_rule_name = build_unique_renamed_rule_name(
            old_rule_name,
            rename_suffix,
            reserved_names,
        )

        rename_map[old_rule_name] = new_rule_name
        reserved_names.add(new_rule_name)

        logger.debug(
            "Reserved replacement rule name: %s -> %s",
            old_rule_name,
            new_rule_name,
        )

    return rename_map


def plan_custom_rewrites(
    custom_collection: RuleCollection,
    rename_map: dict[str, str],
) -> dict[Path, str]:
    """Prepare updated custom rule-file contents.

    Colliding declarations are renamed in their defining files, and all custom
    rule condition sections are updated so dependencies follow the new names.
    """
    original_text_by_path = custom_collection.text_by_path
    updated_text_by_path = dict(original_text_by_path)

    for old_rule_name, new_rule_name in sorted(rename_map.items()):
        custom_rule_file = get_unique_origin(old_rule_name, custom_collection.origins)

        logger.info(
            "Renaming custom rule declaration in %s: %s -> %s",
            custom_rule_file,
            old_rule_name,
            new_rule_name,
        )

        updated_text_by_path[custom_rule_file] = rename_custom_rule_declaration(
            updated_text_by_path[custom_rule_file],
            old_rule_name,
            new_rule_name,
        )

    for path in custom_collection.files:
        logger.debug("Checking custom rule dependencies for rewrite in %s", path)

        updated_text_by_path[path] = rename_rule_references_in_conditions(
            updated_text_by_path[path],
            rename_map,
            path,
        )

    files_to_update = {
        path: updated_text
        for path, updated_text in updated_text_by_path.items()
        if updated_text != original_text_by_path[path]
    }

    logger.info("Prepared updates for %d custom rule file(s).", len(files_to_update))

    for path in sorted(files_to_update):
        logger.debug("Custom rule file planned for update: %s", path)

    return files_to_update


def write_updated_custom_files(updated_text_by_path: dict[Path, str]) -> None:
    """Back up and write modified custom rule files.

    Backups are created immediately before each write so each backup represents
    the exact pre-update state for that path.
    """
    for path, updated_text in sorted(
        updated_text_by_path.items(),
        key=lambda item: str(item[0]),
    ):
        backup_path = make_backup(path)

        try:
            atomic_write_text(path, updated_text)
        except OSError as exc:
            fail(f"Could not write updated custom rule file {path}: {exc}")

        logger.info("Original custom rule file backed up to: %s", backup_path)
        logger.info("Updated custom rule file written to: %s", path)


def parse_args() -> argparse.Namespace:
    """Parse command-line options for rule validation.

    Defaults target the expected container layout, while arguments support local
    testing, CI jobs, and alternate deployment paths.
    """
    parser = argparse.ArgumentParser(
        description=(
            "Validate flat YARA-Forge and custom YARA rule directories, then "
            "rename custom rules that collide with upstream rule names."
        )
    )

    parser.add_argument(
        "--forge-rule-dir",
        type=Path,
        default=DEFAULT_FORGE_RULE_DIR,
        help=(
            "Flat directory containing YARA-Forge .yar/.yara files. "
            f"Default: {DEFAULT_FORGE_RULE_DIR}"
        ),
    )

    parser.add_argument(
        "--custom-rule-dir",
        type=Path,
        default=DEFAULT_CUSTOM_RULE_DIR,
        help=(
            "Flat directory containing custom .yar/.yara files. "
            f"Default: {DEFAULT_CUSTOM_RULE_DIR}"
        ),
    )

    parser.add_argument(
        "--rename-suffix",
        default=CUSTOM_RENAME_SUFFIX,
        help=f"Suffix to append to renamed custom rules. Default: {CUSTOM_RENAME_SUFFIX}",
    )

    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Report planned actions without modifying files.",
    )

    parser.add_argument(
        "--verbose",
        action="store_true",
        help=(
            "Enable debug logging, including discovered files, validated rule "
            "names, collision origins, and rewrite details."
        ),
    )

    return parser.parse_args()


def main() -> None:
    """Validate flat rule collections and rewrite custom collisions when needed.

    YARA-Forge files are treated as immutable upstream input. Custom files are
    rewritten when their rule names collide with upstream names, and custom rule
    dependencies are updated within condition sections.
    """
    args = parse_args()
    configure_logging(verbose=args.verbose)
    validate_rename_suffix(args.rename_suffix)

    forge_rule_dir = args.forge_rule_dir
    custom_rule_dir = args.custom_rule_dir

    logger.info("YARA-Forge rule directory: %s", forge_rule_dir)
    logger.info("Custom rule directory: %s", custom_rule_dir)
    logger.info(
        "Supported rule file extensions: %s",
        ", ".join(sorted(SUPPORTED_RULE_SUFFIXES)),
    )

    ensure_distinct_rule_directories(forge_rule_dir, custom_rule_dir)

    forge_collection = collect_rule_collection("YARA-Forge", forge_rule_dir)
    custom_collection = collect_rule_collection("custom", custom_rule_dir)

    collisions = custom_collection.names & forge_collection.names

    if not collisions:
        logger.info("Rule validation completed successfully; no custom rewrites required.")
        return

    logger.warning(
        "Found %d custom rule name collision(s) with YARA-Forge: %s",
        len(collisions),
        ", ".join(sorted(collisions)),
    )

    log_collision_details(
        collisions,
        custom_collection.origins,
        forge_collection.origins,
    )

    rename_map = build_rename_map(
        collisions=collisions,
        custom_rules=custom_collection.names,
        forge_rules=forge_collection.names,
        rename_suffix=args.rename_suffix,
    )

    for old_rule_name, new_rule_name in sorted(rename_map.items()):
        logger.info("Planned custom rule rename: %s -> %s", old_rule_name, new_rule_name)

    updated_text_by_path = plan_custom_rewrites(
        custom_collection=custom_collection,
        rename_map=rename_map,
    )

    if not updated_text_by_path:
        logger.info("Rule validation completed successfully; no custom files required updates.")
        return

    if args.dry_run:
        logger.info(
            "Dry run requested; %d custom rule file(s) would be modified.",
            len(updated_text_by_path),
        )

        for path in sorted(updated_text_by_path):
            logger.info("Dry run would update custom rule file: %s", path)

        return

    write_updated_custom_files(updated_text_by_path)

    logger.info(
        "Rule validation and custom rewrite completed successfully; updated %d file(s).",
        len(updated_text_by_path),
    )


if __name__ == "__main__":
    main()