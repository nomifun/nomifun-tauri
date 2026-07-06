#!/usr/bin/env python3
"""Search the bundled UI/UX Pro Max design database."""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any


DATA_PATH = Path(__file__).resolve().parents[1] / "data" / "catalog.json"


def load_catalog(path: Path) -> list[dict[str, Any]]:
    try:
        with path.open("r", encoding="utf-8") as handle:
            data = json.load(handle)
    except FileNotFoundError:
        raise SystemExit(f"catalog not found: {path}")
    except json.JSONDecodeError as exc:
        raise SystemExit(f"catalog is not valid JSON: {exc}") from exc

    items = data.get("items")
    if not isinstance(items, list):
        raise SystemExit("catalog must contain an items array")
    return [item for item in items if isinstance(item, dict)]


def flatten(value: Any) -> str:
    if isinstance(value, dict):
        return " ".join(flatten(v) for v in value.values())
    if isinstance(value, list):
        return " ".join(flatten(v) for v in value)
    if value is None:
        return ""
    return str(value)


def tokenize(query: str) -> list[str]:
    return [token for token in re.split(r"[^a-z0-9+#.-]+", query.lower()) if token]


def score_item(item: dict[str, Any], tokens: list[str], domain: str | None, stack: str | None) -> int | None:
    item_domain = str(item.get("domain", "")).lower()
    item_stack = str(item.get("stack", "")).lower()
    tags = [str(tag).lower() for tag in item.get("tags", []) if isinstance(tag, str)]
    haystack = flatten(item).lower()

    if domain and item_domain != domain.lower():
        return None

    if stack:
        stack_query = stack.lower()
        if stack_query not in item_stack and stack_query not in tags and stack_query not in haystack:
            return None

    score = 0
    if domain and item_domain == domain.lower():
        score += 10
    if stack and stack.lower() in item_stack:
        score += 10

    if not tokens:
        return score + 1

    title = str(item.get("title", "")).lower()
    for token in tokens:
        if token in title:
            score += 8
        if token in tags:
            score += 6
        if token in item_domain or token in item_stack:
            score += 5
        if token in haystack:
            score += 2

    return score if score > 0 else None


def format_item(item: dict[str, Any], index: int) -> str:
    tags = ", ".join(str(tag) for tag in item.get("tags", []))
    lines = [
        f"## {index}. {item.get('title', 'Untitled')}",
        f"Domain: `{item.get('domain', 'unknown')}`" + (f" | Tags: {tags}" if tags else ""),
    ]
    if item.get("stack"):
        lines.append(f"Stack: `{item['stack']}`")
    if item.get("summary"):
        lines.append(f"Summary: {item['summary']}")
    guidance = item.get("guidance")
    if isinstance(guidance, list) and guidance:
        lines.append("Guidance:")
        lines.extend(f"- {point}" for point in guidance)
    roles = item.get("roles")
    if isinstance(roles, dict) and roles:
        role_text = ", ".join(f"{key}: `{value}`" for key, value in roles.items())
        lines.append(f"Roles: {role_text}")
    return "\n".join(lines)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Search the UI/UX Pro Max local design database.")
    parser.add_argument("query", nargs="?", default="", help="Search keywords")
    parser.add_argument("--domain", choices=["product", "style", "typography", "color", "landing", "chart", "ux", "stack", "prompt"])
    parser.add_argument("--stack", help="Filter by stack, for example html-tailwind, react, nextjs, vue, svelte")
    parser.add_argument("-n", "--limit", type=int, default=5, help="Maximum number of results")
    parser.add_argument("--list-domains", action="store_true", help="List domains and result counts")
    args = parser.parse_args(argv)

    items = load_catalog(DATA_PATH)

    if args.list_domains:
        counts: dict[str, int] = {}
        for item in items:
            domain = str(item.get("domain", "unknown"))
            counts[domain] = counts.get(domain, 0) + 1
        for domain in sorted(counts):
            print(f"{domain}: {counts[domain]}")
        return 0

    tokens = tokenize(args.query)
    scored: list[tuple[int, dict[str, Any]]] = []
    for item in items:
        score = score_item(item, tokens, args.domain, args.stack)
        if score is not None:
            scored.append((score, item))

    scored.sort(key=lambda pair: (-pair[0], str(pair[1].get("title", ""))))
    results = [item for _, item in scored[: max(args.limit, 1)]]

    print("# UI/UX Pro Max Search Results")
    print(f"Query: `{args.query or '*'}`")
    if args.domain:
        print(f"Domain: `{args.domain}`")
    if args.stack:
        print(f"Stack: `{args.stack}`")
    print()

    if not results:
        print("No matching entries. Try broader terms or run with --list-domains.")
        return 0

    for index, item in enumerate(results, start=1):
        print(format_item(item, index))
        print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
