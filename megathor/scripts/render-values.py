#!/usr/bin/env python3
"""Render a megathor template the way Ansible's template module would, without Ansible.

    scripts/render-values.py <template> <vars file>... [-e key=yaml-value]...

Vars files are merged in order (later files win), then each -e value. String values holding Jinja
are templated against the merged vars until nothing changes, like Ansible's lazy templating. Only
the Ansible filters and tests megathor's templates use are provided. CI uses this to render
roles/thorium/templates/values.yaml.j2 and roles/infra_operators/templates/values.yaml.j2 and check
the results against the thorium and infra-operators charts (see .github/workflows/deploy-tools.yml).
"""

import json
import re
import shlex
import sys

import jinja2
import yaml


def to_bool(value):
    """Ansible's bool filter"""
    if isinstance(value, bool):
        return value
    if isinstance(value, (int, float)):
        return value == 1
    return str(value).strip().lower() in ("yes", "on", "1", "true", "y", "t")


def make_env():
    """A Jinja environment configured like Ansible's template module, with its filters and tests"""
    # Ansible's template module trims blocks and fails on undefined variables
    env = jinja2.Environment(
        undefined=jinja2.StrictUndefined, keep_trailing_newline=True, trim_blocks=True
    )
    env.filters.update(
        bool=to_bool,
        to_json=json.dumps,
        to_nice_yaml=lambda value: yaml.safe_dump(value, default_flow_style=False),
        from_yaml=yaml.safe_load,
        quote=lambda value: shlex.quote(str(value)),
        mandatory=lambda value: value,
    )
    env.tests.update(
        match=lambda value, pattern: re.match(pattern, value) is not None,
        search=lambda value, pattern: re.search(pattern, value) is not None,
    )
    return env


def resolve(env, variables):
    """Template every string value that holds Jinja until none changes"""
    for _ in range(20):
        changed = False
        for key, value in list(variables.items()):
            if isinstance(value, str) and ("{{" in value or "{%" in value):
                rendered = env.from_string(value).render(**variables)
                if rendered != value:
                    variables[key] = rendered
                    changed = True
        if not changed:
            return
    sys.exit("the vars did not stop changing after 20 rounds of templating")


def main():
    """Render the template named on the command line to stdout"""
    if len(sys.argv) < 2 or sys.argv[1] in ("-h", "--help"):
        sys.exit(__doc__)
    template, rest = sys.argv[1], sys.argv[2:]
    variables = {}
    extra = {}
    i = 0
    while i < len(rest):
        # -e values are applied after every vars file, like ansible-playbook's extra vars
        if rest[i] == "-e":
            key, _, value = rest[i + 1].partition("=")
            extra[key] = yaml.safe_load(value)
            i += 2
            continue
        with open(rest[i], encoding="utf-8") as handle:
            variables.update(yaml.safe_load(handle) or {})
        i += 1
    variables.update(extra)
    env = make_env()
    resolve(env, variables)
    with open(template, encoding="utf-8") as handle:
        sys.stdout.write(env.from_string(handle.read()).render(**variables))


if __name__ == "__main__":
    main()
