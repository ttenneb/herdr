from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path


DEFAULT_LOCALES = ("ja", "zh-cn")
SOCKET_METHOD_RE = re.compile(r"`([a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)*)`")
TABLE_DELIMITER_RE = re.compile(r"^:?-{3,}:?$")
METHOD_CELL_RE = re.compile(
    r"^\s*`[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)*`"
    r"(?:\s*[,、，]\s*`[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)*`)*\s*$"
)


def heading_outline(path: Path) -> list[int]:
    outline: list[int] = []
    in_fence = False

    for line in path.read_text(encoding="utf-8").splitlines():
        stripped = line.lstrip()
        if stripped.startswith("```") or stripped.startswith("~~~"):
            in_fence = not in_fence
            continue
        if in_fence or not stripped.startswith("#"):
            continue

        level = 0
        for char in stripped:
            if char != "#":
                break
            level += 1

        if level == 0 or level > 6:
            continue
        if len(stripped) > level and stripped[level] not in (" ", "\t"):
            continue

        outline.append(level)

    return outline


def markdown_table_cells(line: str) -> list[str] | None:
    stripped = line.strip()
    if not (stripped.startswith("|") and stripped.endswith("|")):
        return None
    return [cell.strip() for cell in stripped[1:-1].split("|")]


def socket_method_inventory(path: Path) -> set[str]:
    """Return identifiers from the socket method inventory table, not prose."""
    lines = path.read_text(encoding="utf-8").splitlines()
    candidates: list[set[str]] = []

    for index in range(len(lines) - 2):
        header = markdown_table_cells(lines[index])
        delimiter = markdown_table_cells(lines[index + 1])
        if (
            header is None
            or delimiter is None
            or len(header) != 2
            or len(delimiter) != 2
            or not all(TABLE_DELIMITER_RE.fullmatch(cell) for cell in delimiter)
        ):
            continue

        methods: set[str] = set()
        row_count = 0
        for line in lines[index + 2 :]:
            cells = markdown_table_cells(line)
            if cells is None:
                break
            if len(cells) != 2 or not METHOD_CELL_RE.fullmatch(cells[1]):
                methods.clear()
                break
            methods.update(SOCKET_METHOD_RE.findall(cells[1]))
            row_count += 1

        if row_count >= 2 and methods:
            candidates.append(methods)

    return max(candidates, key=len, default=set())


def english_docs(docs_root: Path) -> list[Path]:
    return sorted(
        path
        for path in docs_root.glob("*.mdx")
        if path.is_file()
    )


def locale_docs(docs_root: Path, locale: str) -> list[Path]:
    locale_root = docs_root / locale
    if not locale_root.exists():
        return []
    return sorted(path for path in locale_root.glob("*.mdx") if path.is_file())


def check_docs_translation_parity(docs_root: Path, locales: tuple[str, ...] = DEFAULT_LOCALES) -> list[str]:
    errors: list[str] = []
    english = english_docs(docs_root)
    english_names = {path.name for path in english}

    for locale in locales:
        translated_names = {path.name for path in locale_docs(docs_root, locale)}

        for missing in sorted(english_names - translated_names):
            errors.append(f"{docs_root / locale / missing}: missing translation file")

        for stale in sorted(translated_names - english_names):
            errors.append(f"{docs_root / locale / stale}: no matching English doc")

    for source in english:
        source_outline = heading_outline(source)

        for locale in locales:
            translated = docs_root / locale / source.name
            if not translated.exists():
                continue

            translated_outline = heading_outline(translated)
            if translated_outline != source_outline:
                errors.append(
                    format_outline_error(source, translated, source_outline, translated_outline)
                )

            source_methods = socket_method_inventory(source)
            translated_methods = socket_method_inventory(translated)
            if translated_methods != source_methods:
                errors.append(
                    format_method_inventory_error(
                        source, translated, source_methods, translated_methods
                    )
                )

    return errors


def format_outline_error(
    source: Path,
    translated: Path,
    source_outline: list[int],
    translated_outline: list[int],
) -> str:
    return (
        f"{translated}: heading outline differs from {source} "
        f"(English {format_counts(source_outline)}, translated {format_counts(translated_outline)})"
    )


def format_method_inventory_error(
    source: Path,
    translated: Path,
    source_methods: set[str],
    translated_methods: set[str],
) -> str:
    missing = sorted(source_methods - translated_methods)
    extra = sorted(translated_methods - source_methods)
    details = []
    if missing:
        details.append(f"missing {', '.join(missing)}")
    if extra:
        details.append(f"extra {', '.join(extra)}")
    return (
        f"{translated}: socket method inventory differs from {source} "
        f"({'; '.join(details)})"
    )


def format_counts(levels: list[int]) -> str:
    if not levels:
        return "0 headings"

    parts = []
    for level in range(1, 7):
        count = levels.count(level)
        if count:
            parts.append(f"h{level}={count}")
    return ", ".join(parts)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Check localized docs have the same heading outline and socket method "
            "inventory as English docs."
        )
    )
    parser.add_argument(
        "--docs-root",
        default="docs/next/website/src/content/docs",
        type=Path,
        help="Docs content root containing English .mdx files and locale subdirectories.",
    )
    parser.add_argument(
        "--locale",
        action="append",
        dest="locales",
        help="Locale subdirectory to check. Can be passed more than once.",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    locales = tuple(args.locales or DEFAULT_LOCALES)
    errors = check_docs_translation_parity(args.docs_root, locales)

    if errors:
        print("error: localized docs differ structurally from English docs", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
