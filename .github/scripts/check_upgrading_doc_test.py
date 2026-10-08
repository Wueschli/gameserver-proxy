"""Tests for check_upgrading_doc.py.

Run: python3 .github/scripts/check_upgrading_doc_test.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from check_upgrading_doc import check, parse_constants  # noqa: E402

CONSTS = {
    "protocol": "1.1",
    "config_schema": "1",
    "store_format": "1",
    "abi": "0.1",
    "product": "0.2.0",
}

DOC = """# Upgrading

## Version table

| Release | Protocol | Config schema | Store format | Sniffer ABI |
| ------- | -------- | ------------- | ------------ | ----------- |
| 0.2.0   | 1.1      | 1             | 1            | 0.1         |
| 0.1.0   | 1.0      | 1             | 1            | 0.1         |

## Next section
"""


class CheckTest(unittest.TestCase):
    def test_passes_when_table_matches_constants(self):
        errors, warnings = check(DOC, CONSTS)
        self.assertEqual((errors, warnings), ([], []))

    def test_fails_when_protocol_row_is_stale(self):
        errors, _ = check(DOC, {**CONSTS, "protocol": "1.2"})
        self.assertEqual(len(errors), 1)
        self.assertIn("protocol", errors[0])
        self.assertIn("1.2", errors[0])

    def test_fails_when_config_store_or_abi_is_stale(self):
        for key, value in [("config_schema", "2"), ("store_format", "2"), ("abi", "0.2")]:
            errors, _ = check(DOC, {**CONSTS, key: value})
            self.assertEqual(len(errors), 1, key)

    def test_fails_when_no_table(self):
        errors, _ = check("# Upgrading\n\nno table here\n", CONSTS)
        self.assertEqual(len(errors), 1)
        self.assertIn("version table", errors[0])

    def test_warns_when_the_product_version_is_not_the_newest_row(self):
        errors, warnings = check(DOC, {**CONSTS, "product": "0.3.0"})
        self.assertEqual(errors, [])
        self.assertEqual(len(warnings), 1)
        self.assertIn("0.3.0", warnings[0])

    def test_an_unreleased_row_does_not_warn_about_the_version(self):
        doc = DOC.replace("| 0.2.0   |", "| unreleased |")
        self.assertEqual(check(doc, CONSTS), ([], []))

    def test_only_the_newest_row_is_compared(self):
        # The old 0.1.0 row says protocol 1.0 and must not be flagged.
        errors, _ = check(DOC, CONSTS)
        self.assertEqual(errors, [])


class ParseConstantsTest(unittest.TestCase):
    def test_reads_pub_consts_from_rust_source(self):
        src = {
            "protocol": "pub const PROTOCOL_MAJOR: u16 = 1;\n/// doc\npub const PROTOCOL_MINOR: u16 = 3;\n",
            "config": "pub const CONFIG_SCHEMA_VERSION: u32 = 4;\n",
            "store": "pub const STORE_FORMAT: u32 = 2;\n",
            "abi": "pub const ABI_MAJOR: u16 = 0;\npub const ABI_MINOR: u16 = 1;\n",
            "cargo": '[workspace.package]\nversion = "0.2.0"\n',
        }
        self.assertEqual(
            parse_constants(src),
            {
                "protocol": "1.3",
                "config_schema": "4",
                "store_format": "2",
                "abi": "0.1",
                "product": "0.2.0",
            },
        )

    def test_a_missing_constant_is_an_error(self):
        with self.assertRaises(ValueError) as ctx:
            parse_constants(
                {"protocol": "", "config": "", "store": "", "abi": "", "cargo": '[workspace.package]\nversion = "0.2.0"\n'}
            )
        self.assertIn("PROTOCOL_MAJOR", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
