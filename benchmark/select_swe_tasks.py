#!/usr/bin/env python3
"""Build a symlinked dataset of only the software-engineering tasks.

Harbor's local datasets are just directories of task dirs. This script scans a
downloaded dataset (e.g. ``terminal-bench``), reads each ``task.toml``, and
symlinks every task whose ``[metadata].category`` is in the selected set into a
new directory. The result can be passed to ``harbor run -p <dir>``.

Usage::

    python3 benchmark/select_swe_tasks.py \
        benchmark/tasks/terminal-bench \
        benchmark/tasks/terminal-bench-swe
"""

from __future__ import annotations

import argparse
import os
import shutil
import sys
import tomllib
from pathlib import Path

#: Metadata categories treated as "software engineering".
DEFAULT_CATEGORIES = ("Software",)


def category_of(task_toml: Path) -> str:
    try:
        data = tomllib.loads(task_toml.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as exc:
        print(f"warning: skipping {task_toml}: {exc}", file=sys.stderr)
        return ""
    return (data.get("metadata") or {}).get("category", "") or ""


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path, help="Downloaded dataset directory")
    parser.add_argument("output", type=Path, help="Filtered dataset directory to create")
    parser.add_argument(
        "--category",
        action="append",
        dest="categories",
        help=f"Category to include (repeatable). Default: {', '.join(DEFAULT_CATEGORIES)}",
    )
    parser.add_argument(
        "--limit",
        type=int,
        default=None,
        help="Only include the first N matching tasks (sorted by name).",
    )
    parser.add_argument("--overwrite", action="store_true")
    args = parser.parse_args()

    categories = set(args.categories or DEFAULT_CATEGORIES)

    if not args.source.is_dir():
        parser.error(f"source dataset not found: {args.source}")
    if args.output.exists():
        if not args.overwrite:
            parser.error(f"{args.output} already exists (use --overwrite)")
        shutil.rmtree(args.output)
    args.output.mkdir(parents=True)

    included = []
    matches = [
        task_toml.parent
        for task_toml in sorted(args.source.rglob("task.toml"))
        if category_of(task_toml) in categories
    ]
    if args.limit is not None:
        matches = matches[: args.limit]

    for task_dir in matches:
        link = args.output / task_dir.relative_to(args.source)
        link.parent.mkdir(parents=True, exist_ok=True)
        # Relative target so the dataset survives moving/renaming the repo.
        os.symlink(
            os.path.relpath(task_dir.resolve(), link.parent),
            link,
            target_is_directory=True,
        )
        included.append(task_dir.name)

    print(f"linked {len(included)} task(s) into {args.output}")
    for name in included:
        print(f"  {name}")
    if not included:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
