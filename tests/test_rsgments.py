from concurrent.futures import ThreadPoolExecutor

import pytest
from pygments.lexer import ExtendedRegexLexer, RegexLexer, bygroups, words
from pygments.lexers import (
    BashLexer,
    CppLexer,
    CssLexer,
    GoLexer,
    HtmlLexer,
    JavascriptLexer,
    JsonLexer,
    MarkdownLexer,
    PhpLexer,
    PythonLexer,
    RubyLexer,
    RustLexer,
    SqlLexer,
    TypeScriptLexer,
)
from pygments.token import Keyword, Name, Number, Operator, String, Text

import rsgments

PYTHON_EDGE_CASES = '''\
# Unicode comment with emoji 🚀💡
def f(x: int = 0) -> str:
    match x:
        case 0:
            pass
        case _:
            match = 42
            case = "không phải soft-keyword"
    self_len = self.len(x)
    real_len = len("hello 🎉")
    s = f"giá trị: {real_len:.2f}"
    raw = r"""multiple lines
    preserve the state"""
    unclosed = "chuỗi chưa đóng
    next_line = 123_456 + 0xFF
    return s
'''


@pytest.mark.parametrize(
    "lexer,code",
    [
        (PythonLexer(), PYTHON_EDGE_CASES),
        (RustLexer(), 'fn main() { let s = r#"chào Rust 🦀"#; println!("{s}"); }\n'),
        (CppLexer(), "#include <vector>\n// A comment\nint main() { return 0; }\n"),
        (JavascriptLexer(), 'const fn = (a, b = "việt") => `res: ${a + b}`;\n'),
        (
            TypeScriptLexer(),
            'interface User { id: number; name: string; }\nconst u: User = { id: 1, name: "A" };\n',
        ),
        (
            HtmlLexer(),
            '<html><style>body{color:#fff}</style><script>let x="hi";</script></html>\n',
        ),
        (CssLexer(), ".btn { color: #ff00aa; margin: 10px 20px; }\n"),
        (JsonLexer(), '{"tiếng_việt": [1, 2.5, true, null, "🎉"]}\n'),
        (MarkdownLexer(), '# Tiêu đề\n\n```python\ndef hi(): return "ok"\n```\n'),
        (
            SqlLexer(),
            "SELECT id, name FROM users WHERE name LIKE '%Việt%' AND active = 1;\n",
        ),
        (BashLexer(), '#!/bin/bash\nfor f in *.py; do echo "File: $f"; done\n'),
        (
            GoLexer(),
            'package main\nimport "fmt"\nfunc main() { fmt.Println("Xin chào") }\n',
        ),
    ],
)
def test_parity_with_pygments(lexer, code):
    with rsgments.unpatched():
        expected = list(lexer.get_tokens_unprocessed(code))

    with rsgments.patched():
        actual = list(lexer.get_tokens_unprocessed(code))

    assert actual == expected


def test_empty_and_newline_inputs():
    lexer = PythonLexer()
    for code in ["", "\n", "\n\n\n", "   ", "🚀\n💡"]:
        with rsgments.unpatched():
            expected = list(lexer.get_tokens_unprocessed(code))
        with rsgments.patched():
            actual = list(lexer.get_tokens_unprocessed(code))
        assert actual == expected


def test_backreference_and_lookaround_custom_lexer():
    class CustomBackrefLexer(RegexLexer):
        tokens = {
            "root": [
                (r"([\'\"])(.*?)\1", bygroups(String.Delimiter, String)),
                (r"(?<!\.)\bmagic\b(?=\s*\()", Keyword),
                (words(("alpha", "beta"), suffix=r"\b"), Name.Builtin),
                (r"\d+", Number),
                (r"[a-zA-Z_.()]+", Name),
                (r"\s+", Text),
            ]
        }

    code = "magic(x) self.magic(x) 'hello' \"world\" 'mismatch\" alpha 123\n"
    lexer = CustomBackrefLexer()
    with rsgments.unpatched():
        expected = list(lexer.get_tokens_unprocessed(code))
    with rsgments.patched():
        actual = list(lexer.get_tokens_unprocessed(code))
    assert actual == expected


def test_callback_out_of_bounds_position_does_not_corrupt_offsets():
    def callback(lexer, match):
        return [(100, Name, "outside")]

    class CustomCallbackLexer(RegexLexer):
        tokens = {
            "root": [
                (r"x", callback),
                (r"y", Name),
            ]
        }

    lexer = CustomCallbackLexer()
    with rsgments.unpatched():
        expected = list(lexer.get_tokens_unprocessed("xy"))
    assert rsgments.lex_tokens(lexer, "xy") == expected


def test_empty_callback_token_at_start_is_preserved():
    def callback(lexer, match):
        return [(match.start(), Name, "")]

    class EmptyCallbackLexer(RegexLexer):
        tokens = {"root": [(r"a", callback)]}

    lexer = EmptyCallbackLexer()
    with rsgments.unpatched():
        expected = list(lexer.get_tokens_unprocessed("a"))
    assert rsgments.lex_tokens(lexer, "a") == expected


def test_extended_regex_lexer_safety():
    php = PhpLexer()
    ruby = RubyLexer()
    assert not rsgments.is_supported(php)
    assert not rsgments.is_supported(ruby)

    class CustomExtendedLexer(ExtendedRegexLexer):
        tokens = {"root": [(r".+", Text)]}

    with pytest.raises(TypeError):
        rsgments.lex_flat_spans(CustomExtendedLexer(), "extended")

    # ExtendedRegexLexer must continue using the original Pygments implementation.
    with rsgments.patched():
        php_tokens = list(php.get_tokens_unprocessed("<?php echo 'hi'; ?>"))
        ruby_tokens = list(ruby.get_tokens_unprocessed("puts 'hi'"))
    assert len(php_tokens) > 0
    assert len(ruby_tokens) > 0


def test_stateful_line_by_line_highlighting():
    lexer = PythonLexer()
    lines = [
        'x = """start multiline',
        'still inside string "quote" def fake():',
        'end""" + 123',
        "def real_fn(): pass",
    ]
    stack = ("root",)
    all_line_tokens = []
    for line in lines:
        buf, table, final_stack = rsgments.lex_flat_spans(
            lexer, line + "\n", stack=stack
        )
        mv = memoryview(buf).cast("I")
        tokens = [
            (mv[i], mv[i + 1], table[mv[i + 2]])
            for i in range(0, len(mv), 3)
            if mv[i] < len(line)
        ]
        all_line_tokens.append(tokens)
        stack = tuple(final_stack)

    # The second line is inside a multiline string, so it must not contain `def`.
    assert all(tok not in Keyword for _, _, tok in all_line_tokens[1])
    # The fourth line returns to the root state and must contain `def`.
    assert any(tok in Keyword for _, _, tok in all_line_tokens[3])
    assert stack == ("root",)


def test_cache_independent_from_patch_state():
    rsgments.unpatch_pygments()
    lexer = PythonLexer()

    # Compile and lex directly even when the global Pygments patch is disabled.
    buf1, table1, _ = rsgments.lex_flat_spans(lexer, PYTHON_EDGE_CASES)
    assert not rsgments.is_patched()

    rsgments.patch_pygments()
    assert rsgments.is_patched()
    buf2, table2, _ = rsgments.lex_flat_spans(lexer, PYTHON_EDGE_CASES)

    rsgments.unpatch_pygments()
    assert not rsgments.is_patched()
    buf3, table3, _ = rsgments.lex_flat_spans(lexer, PYTHON_EDGE_CASES)

    assert buf1 == buf2 == buf3
    assert table1 == table2 == table3


def test_flat_byte_spans_clip_unicode_ranges():
    lexer = PythonLexer()
    text = 'prefix = "é🚀"; suffix = "xin chào"\n'
    encoded = text.encode("utf-8")
    start_byte = encoded.index("é".encode("utf-8")) + 1
    end_byte = encoded.index(b"suffix") + 3

    full_buf, table, full_stack = rsgments.lex_flat_byte_spans(lexer, text)
    full_values = memoryview(full_buf).cast("I")
    full_spans = [tuple(full_values[i : i + 3]) for i in range(0, len(full_values), 3)]

    range_buf, range_table, range_stack = rsgments.lex_flat_byte_spans(
        lexer, text, start_byte=start_byte, end_byte=end_byte
    )
    range_values = memoryview(range_buf).cast("I")
    range_spans = [
        tuple(range_values[i : i + 3]) for i in range(0, len(range_values), 3)
    ]
    expected = []
    for token_start, token_length, token_id in full_spans:
        token_end = token_start + token_length
        clipped_start = max(token_start, start_byte)
        clipped_end = min(token_end, end_byte)
        if clipped_start < clipped_end:
            expected.append((clipped_start, clipped_end - clipped_start, token_id))

    assert range_spans == expected
    assert all(
        start_byte <= pos < end_byte and pos + length <= end_byte
        for pos, length, _ in range_spans
    )
    assert range_table == table
    assert range_stack == ["root"]
    assert full_stack == ["root"]


def test_flat_byte_span_slices_reconstruct_vietnamese_crlf_text():
    lexer = PythonLexer()
    text = 'message = "Xin chào thế giới"\r\nresult = "đúng"\r\n'
    encoded = text.encode("utf-8")
    cuts = [
        0,
        encoded.index("chào".encode("utf-8")) + 2,
        encoded.index(b"\r\n") + 1,
        encoded.index("đúng".encode("utf-8")) + 1,
        len(encoded),
    ]

    def byte_token_ids(buffer, byte_length):
        values = memoryview(buffer).cast("I")
        result = [None] * byte_length
        for index in range(0, len(values), 3):
            start, length, token_id = values[index : index + 3]
            for byte_pos in range(start, start + length):
                assert result[byte_pos] is None
                result[byte_pos] = token_id
        return result

    full_buf, table, full_stack = rsgments.lex_flat_byte_spans(lexer, text)
    full_ids = byte_token_ids(full_buf, len(encoded))
    sliced_ids = [None] * len(encoded)

    for start_byte, end_byte in zip(cuts, cuts[1:]):
        sliced_buf, sliced_table, _ = rsgments.lex_flat_byte_spans(
            lexer, text, start_byte=start_byte, end_byte=end_byte
        )
        assert sliced_table == table
        slice_ids = byte_token_ids(sliced_buf, len(encoded))
        for byte_pos in range(start_byte, end_byte):
            assert sliced_ids[byte_pos] is None
            sliced_ids[byte_pos] = slice_ids[byte_pos]

    assert sliced_ids == full_ids
    assert all(token_id is not None for token_id in full_ids)
    assert full_stack == ["root"]


def test_flat_byte_spans_stop_at_end_byte():
    class StatefulLexer(RegexLexer):
        tokens = {
            "root": [(r"a", Name), (r"b", Name, "inner"), (r".", Text)],
            "inner": [(r".", String)],
        }

    buf, table, final_stack = rsgments.lex_flat_byte_spans(
        StatefulLexer(), "ab", end_byte=1
    )
    values = memoryview(buf).cast("I")
    spans = [tuple(values[i : i + 3]) for i in range(0, len(values), 3)]

    assert spans == [(0, 1, table.index(Name))]
    assert final_stack == ["root"]


def test_multithreaded_patch_unpatch_consistency():
    py_lexer = PythonLexer()
    html_lexer = HtmlLexer()
    py_code = PYTHON_EDGE_CASES * 25
    html_code = (
        "<html><style>body{color:#fff}</style><script>const x=1;</script></html>\n" * 25
    )

    with rsgments.unpatched():
        expected_py = list(py_lexer.get_tokens_unprocessed(py_code))
        expected_html = list(html_lexer.get_tokens_unprocessed(html_code))

    def worker(idx: int):
        for _ in range(15):
            if idx % 2 == 0:
                res = list(py_lexer.get_tokens_unprocessed(py_code))
                assert res == expected_py
            else:
                res = list(html_lexer.get_tokens_unprocessed(html_code))
                assert res == expected_html

    def toggler():
        for _ in range(25):
            rsgments.patch_pygments()
            rsgments.unpatch_pygments()

    rsgments.patch_pygments()
    try:
        with ThreadPoolExecutor(max_workers=8) as pool:
            futures = [pool.submit(worker, i) for i in range(6)]
            futures.append(pool.submit(toggler))
            futures.append(pool.submit(toggler))
            for f in futures:
                f.result()
    finally:
        rsgments.unpatch_pygments()
