"""Reference-backed source parsing and range overlay checks."""

import random
import struct
import unittest

from generate_adobe_cmaps import compile_data, normalize_ranges, parse


class MappingGenerationTests(unittest.TestCase):
    def test_four_byte_legacy_directory_buckets_preserve_empty_high_bytes(self):
        rows = [(4, (high << 24) | (prefix << 8) | 0x36,
                 (high << 24) | (prefix << 8) | 0x36, 1 + (high - 0x81) * 70 + prefix)
                for high in (0x81, 0x82) for prefix in range(70)]
        resources = {
            "Adobe-GB1-UCS2": {"unicode": {cid: chr(0x4E00 + cid) for cid in range(1, 141)}, "ranges": []},
            "Bucket-H": {"ranges": rows, "ordering": "GB1", "wmode": 0,
                         "codespaces": [(4, 0x81000000, 0x82FFFFFF)]},
        }
        binary, _, index = compile_data(resources)
        group = index["maps"]["Bucket-H"]["groups"][3]
        self.assertEqual(group["count"], 140)
        self.assertGreater(group["bucket"], 0)
        base = group["bucket"] - 1
        bound = lambda value: struct.unpack_from("<I", binary, base + value * 4)[0]
        self.assertEqual([bound(value) for value in [0, 0x80, 0x81, 0x82, 0x83, 256]], [0, 0, 0, 70, 140, 140])

    def test_lengths_inheritance_metadata_and_four_byte_codes_remain_distinct(self):
        result = parse(b"""/GBK2K-H usecmap /Ordering (GB1) def /WMode 1 def
            2 begincodespacerange <00> <7f> <81308130> <fe39fe39> endcodespacerange
            2 begincidchar <41> 34 <0041> 999 endcidchar
            1 begincidrange <81308436> <81308438> 22354 endcidrange""")
        self.assertEqual(result["base"], "GBK2K-H")
        self.assertEqual(result["wmode"], 1)
        self.assertEqual(result["ordering"], "GB1")
        self.assertEqual(result["ranges"], [(1, 65, 65, 34), (2, 65, 65, 999),
                                             (4, 0x81308436, 0x81308438, 22354)])

    def test_unicode_surrogates_ligatures_arrays_and_ranges(self):
        result = parse(b"""2 beginbfchar <0001> <D840DC00> <0002> <00660069> endbfchar
            2 beginbfrange <0003> <0004> [<4e2d> <4e2e>] <0005> <0006> <D840DC01> endbfrange""")
        self.assertEqual(result["unicode"], {1: "\U00020000", 2: "fi", 3: "中", 4: "\u4e2e",
                                             5: "\U00020001", 6: "\U00020002"})

    def test_overlay_prefers_later_ranges_and_preserves_unaffected_boundaries(self):
        rows = [(2, 1, 10, 100), (2, 4, 6, 700), (2, 5, 5, 900), (1, 5, 5, 10)]
        self.assertEqual(normalize_ranges(rows), [(1, 5, 5, 10), (2, 1, 3, 100),
            (2, 4, 4, 700), (2, 5, 5, 900), (2, 6, 6, 702), (2, 7, 10, 106)])

    def test_normalization_matches_independent_expansion(self):
        randomizer = random.Random(47)
        for _ in range(50):
            rows, expected = [], {}
            for _ in range(100):
                length = randomizer.choice((1, 2, 4))
                low = randomizer.randrange(50)
                high = low + randomizer.randrange(8)
                first = randomizer.randrange(1000)
                rows.append((length, low, high, first))
                for code in range(low, high + 1):
                    expected[length, code] = first + code - low
            actual = {}
            for length, low, high, first in normalize_ranges(rows):
                for code in range(low, high + 1):
                    self.assertNotIn((length, code), actual)
                    actual[length, code] = first + code - low
            self.assertEqual(actual, expected)

    def test_binary_keeps_cid_and_unicode_namespaces_and_multi_scalar_text(self):
        resources = {
            "Adobe-GB1-UCS2": {"unicode": {42: "中", 43: "fi"}, "ranges": []},
            "Test-H": {"ranges": [(2, 100, 101, 42)], "ordering": "GB1",
                       "wmode": 0, "codespaces": [(2, 0, 65535)]},
        }
        binary, text, index = compile_data(resources)
        group = index["maps"]["Test-H"]["groups"][1]
        self.assertTrue(group["direct"])
        descriptor = struct.unpack_from("<I", binary, group["offset"])[0]
        self.assertEqual(descriptor & 0xC0000000, 0x80000000)
        page = descriptor & 0x3FFFFFFF
        self.assertEqual(struct.unpack_from("<H", binary, page)[0], 2)
        first = struct.unpack_from("<BHI", binary, page + 2)
        second = struct.unpack_from("<BHI", binary, page + 9)
        self.assertEqual(first, (100, 42, 0x4E2D))
        self.assertEqual(second[:2], (101, 43))
        offset = second[2] & 0x7FFFFFFF
        size = struct.unpack_from("<H", text, offset)[0]
        self.assertEqual(text[offset + 2:offset + 2 + size].decode(), "fi")

    def test_dense_unicode_pages_are_shared_without_duplicating_source_unicode(self):
        resource = {"ranges": [(2, 0x4E00, 0x4EFF, 1000)], "ordering": "GB1",
                    "wmode": 0, "codespaces": [(2, 0, 65535)]}
        resources = {"Adobe-GB1-UCS2": {"unicode": {1000: "一"}, "ranges": []},
                     "UniGB-UCS2-H": resource, "UniGB-UTF16-H": resource}
        binary, _, index = compile_data(resources)
        first = index["maps"]["UniGB-UCS2-H"]["groups"][1]
        second = index["maps"]["UniGB-UTF16-H"]["groups"][1]
        self.assertEqual(first, second)
        self.assertTrue(first["direct"])
        descriptor = struct.unpack_from("<I", binary, first["offset"] + 0x4E * 4)[0]
        self.assertEqual(descriptor & 0xC0000000, 0x40000000)
        self.assertEqual(struct.unpack_from("<H", binary, (descriptor & 0x3FFFFFFF) + 255 * 2)[0], 1255)

    def test_invalid_ranges_or_unicode_fail_generation(self):
        for source in [b"1 begincidrange <0005> <0001> 4 endcidrange",
                       b"1 begincidrange <0001> <0003> 65535 endcidrange",
                       b"1 beginbfchar <0001> <D840> endbfchar",
                       b"1 beginbfrange <0001> <0003> [<0041>] endbfrange",
                       b"1 begincidchar <0011223344> 1 endcidchar"]:
            with self.subTest(source=source), self.assertRaises((ValueError, IndexError)):
                parse(source)


if __name__ == "__main__":
    unittest.main()
