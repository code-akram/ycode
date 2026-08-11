#!/usr/bin/env python3
"""Shell launcher for `just` recipes.

This keeps recipe bodies as normal shell snippets while giving the justfile one
portable placeholder, `{args}`, for forwarding variadic recipe arguments.
"""

import os
import sys


ARGS_TOKEN = "{args}"
STDERR_NULL_TOKEN = "{stderr-null}"
SH_ARGS = '"$@"'
SH_STDERR_NULL = "2>/dev/null"


def main() -> int:
    if len(sys.argv) < 2:
        print("just shell adapter expected a recipe command.", file=sys.stderr)
        return 1

    command = sys.argv[1]
    recipe_name = sys.argv[2] if len(sys.argv) > 2 else ""
    recipe_args = sys.argv[3:]

    return run_sh(command, recipe_name, recipe_args)


def run_sh(command: str, recipe_name: str, recipe_args: list[str]) -> int:
    command = command.replace(ARGS_TOKEN, SH_ARGS)
    command = command.replace(STDERR_NULL_TOKEN, SH_STDERR_NULL)
    os.execvp("sh", ["sh", "-cu", command, recipe_name, *recipe_args])


if __name__ == "__main__":
    raise SystemExit(main())
