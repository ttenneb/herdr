from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from scripts.docs_translation_parity import (
    check_docs_translation_parity,
    heading_outline,
    socket_method_inventory,
)


class DocsTranslationParityTests(unittest.TestCase):
    def test_heading_outline_ignores_fenced_code_blocks(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "doc.mdx"
            path.write_text(
                "# Title\n\n```md\n## Not a heading\n```\n\n## Real section\n",
                encoding="utf-8",
            )

            self.assertEqual(heading_outline(path), [1, 2])

    def test_socket_method_inventory_detects_table_without_server_stop(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "socket-api.mdx"
            path.write_text(
                "`pane.prose.only` is mentioned here.\n\n"
                "| Area | Methods |\n"
                "| --- | --- |\n"
                "| Server | `ping`, `server.reload_config` |\n"
                "| Pane | `pane.list`, `pane.input.set` |\n",
                encoding="utf-8",
            )

            self.assertEqual(
                socket_method_inventory(path),
                {"ping", "server.reload_config", "pane.list", "pane.input.set"},
            )

    def test_parity_accepts_translated_heading_text_with_same_shape(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "ja").mkdir()
            (root / "zh-cn").mkdir()
            (root / "guide.mdx").write_text("# Guide\n\n## Install\n\n### Verify\n", encoding="utf-8")
            (root / "ja" / "guide.mdx").write_text(
                "# ガイド\n\n## インストール\n\n### 確認\n",
                encoding="utf-8",
            )
            (root / "zh-cn" / "guide.mdx").write_text(
                "# 指南\n\n## 安装\n\n### 验证\n",
                encoding="utf-8",
            )

            self.assertEqual(check_docs_translation_parity(root), [])

    def test_parity_reports_missing_heading_sections(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "ja").mkdir()
            (root / "zh-cn").mkdir()
            (root / "cli-reference.mdx").write_text(
                "# CLI reference\n\n## Launch\n\n## Shell completions\n",
                encoding="utf-8",
            )
            (root / "ja" / "cli-reference.mdx").write_text(
                "# CLI リファレンス\n\n## 起動\n",
                encoding="utf-8",
            )
            (root / "zh-cn" / "cli-reference.mdx").write_text(
                "# CLI 参考\n\n## 启动\n\n## Shell 补全\n",
                encoding="utf-8",
            )

            errors = check_docs_translation_parity(root)

            self.assertEqual(len(errors), 1)
            self.assertIn(str(Path("ja") / "cli-reference.mdx"), errors[0])
            self.assertIn("heading outline differs", errors[0])

    def test_parity_reports_socket_method_inventory_drift(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "ja").mkdir()
            (root / "zh-cn").mkdir()
            source = (
                "# Socket API\n\n"
                "| Area | Methods |\n| --- | --- |\n"
                "| Server | `ping`, `server.stop` |\n"
                "| Pane | `pane.list`, `pane.input.set` |\n"
            )
            translated = (
                "# Socket API\n\n"
                "| 領域 | メソッド |\n| --- | --- |\n"
                "| サーバー | `ping`, `server.stop` |\n"
                "| ペイン | `pane.list` |\n"
            )
            (root / "socket-api.mdx").write_text(source, encoding="utf-8")
            (root / "ja" / "socket-api.mdx").write_text(translated, encoding="utf-8")
            (root / "zh-cn" / "socket-api.mdx").write_text(source, encoding="utf-8")

            errors = check_docs_translation_parity(root)

            self.assertEqual(len(errors), 1)
            self.assertIn("socket method inventory differs", errors[0])
            self.assertIn("missing pane.input.set", errors[0])

    def test_parity_reports_missing_and_stale_files(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "ja").mkdir()
            (root / "zh-cn").mkdir()
            (root / "guide.mdx").write_text("# Guide\n", encoding="utf-8")
            (root / "ja" / "old.mdx").write_text("# Old\n", encoding="utf-8")
            (root / "zh-cn" / "guide.mdx").write_text("# 指南\n", encoding="utf-8")

            errors = check_docs_translation_parity(root)

            self.assertIn(f"{root / 'ja' / 'guide.mdx'}: missing translation file", errors)
            self.assertIn(f"{root / 'ja' / 'old.mdx'}: no matching English doc", errors)


if __name__ == "__main__":
    unittest.main()
