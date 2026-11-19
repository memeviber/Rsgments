import re
import subprocess
import sys

import pytest
from pygments.lexer import RegexLexer, bygroups
from pygments.lexers import PythonLexer
from pygments.token import Error, Name, Operator, Punctuation, String, Text

import rsgments


def assert_parity(pattern, source, *, flags=re.MULTILINE, token=Name):
    class CaseLexer(RegexLexer):
        pass

    CaseLexer.flags = flags
    CaseLexer.tokens = {
        "root": [
            (pattern, token),
            (r"[\s\S]", Text),
        ]
    }
    lexer = CaseLexer()
    with rsgments.unpatched():
        expected = list(lexer.get_tokens_unprocessed(source))
    actual = rsgments.lex_tokens(lexer, source)
    assert actual == expected


@pytest.mark.parametrize(
    "pattern,source,flags",
    [
        # Named Unicode escapes (\N{...}) must fall back to Python instead of
        # being misparsed by fancy-regex (where \N means [^\n]).
        (r"\N{LATIN SMALL LETTER C}af\N{LATIN SMALL LETTER E WITH ACUTE}", "café cafe", re.UNICODE),
        (r"[\N{DIGIT ZERO}-\N{DIGIT NINE}]+", "abc 123 def", re.UNICODE),
        # Unicode shorthand and boundary classes. Python's \w excludes combining marks;
        # Python's \s includes several separators not consistently classified by Rust.
        (r"\w+", "café a\u0301 _ ٣", re.UNICODE),
        (r"\W+", "a\u0301! b", re.UNICODE),
        (r"\bfoo\b", "foo food xfoo foo_ foo\u0301", re.UNICODE),
        (r"\Boo\B", "food xfoo foobar", re.UNICODE),
        (r"\s+", "a\t\n\u001c\u0085\u00a0\u2003b", re.UNICODE),
        (r"\S+", "a\u00a0b\u001cc", re.UNICODE),
        (r"\d+", "12 ٣ ๙ ²", re.UNICODE),
        (r"\D+", "12 ٣ ²x", re.UNICODE),
        # ASCII mode, both as lexer flags and scoped inline flags.
        (r"\w+", "café a_2", re.ASCII),
        (r"(?a:\w+)", "café a_2", re.UNICODE),
        (r"(?ai:kelvin)", "kelvin KELVIN", re.UNICODE),
        # Lookahead/lookbehind, positive and negative; matches still consume text.
        (r"foo(?=bar)", "foobar foobaz", re.UNICODE),
        (r"foo(?!bar)", "foobar foobaz", re.UNICODE),
        (r"(?<=x)foo", "xfoo yfoo", re.UNICODE),
        (r"(?<!x)foo", "xfoo yfoo", re.UNICODE),
        (r"(?<!\.)\bname\b(?=\s*\()", "name() obj.name() name (x)", re.UNICODE),
        # Backreferences and capture-group edge cases.
        (r"(['\"])(.*?)\1", "'one' \"two\" 'mismatch\"", re.UNICODE),
        (r"(ab|cd)\1", "abab cdcd abcd", re.UNICODE),
        (r"(a)?b\1?", "b ab aaab", re.UNICODE),
        # Inline/global flags, anchors, multiline and dot behavior.
        (r"(?i:hello)", "HELLO hello HeLLo", re.UNICODE),
        (r"^foo$", "foo\nbar\nfoo!\n", re.MULTILINE),
        (r"foo\Z", "foo\nfoo", re.UNICODE),
        (r"a.b", "a\nb a\tb", re.UNICODE),
        (r"a.b", "a\nb a\tb", re.DOTALL),
        (r"a+?", "aaaa baaa", re.UNICODE),
        (r"(?:ab|a)b", "ab abb aab", re.UNICODE),
        # VERBOSE syntax, including literals inside character classes and comments.
        (r"[a #]+", "a # b", re.VERBOSE),
        (r"[a #]+", "a # b", re.UNICODE | re.VERBOSE),
        (r"(?x: a [#] \d+ )", "a#12 a #34", re.UNICODE),
        (r"\#\s+", "#  x #\tx", re.VERBOSE),
        # Python-specific word-boundary spellings translated/fallbacked by Rsgments.
        (r"\<word\>", "word sword word!", re.UNICODE),
        # Character-class double operators and literal '[' treated literally in Python re.
        (r"[a-z--_]+", "abc--_xyz!", re.UNICODE),
        (r"[a&b&&c]+", "a&&b!c", re.UNICODE),
        (r"[a~b~~c]+", "a~~b!c", re.UNICODE),
        (r"[a[b]+", "a[b]c", re.UNICODE),
    ],
)
def test_regex_pattern_parity(pattern, source, flags):
    assert_parity(pattern, source, flags=flags)


def test_bygroups_optional_and_empty_capture_parity():
    class GroupsLexer(RegexLexer):
        tokens = {
            "root": [
                (r"(a)?(b)(c*)", bygroups(Name, Operator, String)),
                (r"[\s\S]", Text),
            ]
        }

    lexer = GroupsLexer()
    source = "bc abccc b"
    with rsgments.unpatched():
        expected = list(lexer.get_tokens_unprocessed(source))
    assert rsgments.lex_tokens(lexer, source) == expected


def test_named_group_backreference_parity():
    assert_parity(
        r"(?P<q>['\"])(?P<body>.*?)(?P=q)", "'hi' \"there\"", flags=re.UNICODE
    )


def test_lookaround_false_positive_keeps_original_rule_order():
    class OrderedLookaroundLexer(RegexLexer):
        tokens = {
            "root": [
                (r"foo(?=bar)", Name),
                (r"foo", Operator),
                (r"[\s\S]", Text),
            ]
        }

    lexer = OrderedLookaroundLexer()
    source = "foobar foobaz"
    with rsgments.unpatched():
        expected = list(lexer.get_tokens_unprocessed(source))
    assert rsgments.lex_tokens(lexer, source) == expected


def test_regex_state_transition_with_lookbehind_and_unicode():
    class StatefulRegexLexer(RegexLexer):
        tokens = {
            "root": [
                (r"<", Punctuation, "body"),
                (r"[^<]+", Text),
            ],
            "body": [
                (r"(?<=<)/", Punctuation, "#pop"),
                (r"\w+", Name),
                (r"\s+", Text),
                (r"[\s\S]", Error),
            ],
        }

    lexer = StatefulRegexLexer()
    source = "<café> /fin\n"
    with rsgments.unpatched():
        expected = list(lexer.get_tokens_unprocessed(source))
    assert rsgments.lex_tokens(lexer, source) == expected


def test_shorthand_rules_stay_in_multidfa_not_pyonly():
    class ShorthandLexer(RegexLexer):
        tokens = {
            "root": [
                (r"\w+", Name),
                (r"\s+", Text),
                (r"\bfoo\b", Operator),
                (r"[a-z--_]+", String),
                (r"[\s\S]", Error),
            ]
        }

    compiled = rsgments.compile_lexer(ShorthandLexer())
    _states, total, auto, _fancy, unidx, py_fb = compiled.stats()
    assert total == 5
    assert auto == 5
    assert unidx == 0
    assert py_fb is False

    py_compiled = rsgments.compile_lexer(PythonLexer())
    _ps, _pt, py_auto, _pf, py_unidx, _pfb = py_compiled.stats()
    assert py_auto >= 400
    assert py_unidx <= 12


def test_redos_shorthand_runs_linear_in_subprocess():
    script = """
import time
from pygments.lexer import RegexLexer
from pygments.token import Name, Text
import rsgments

class EvilWordLexer(RegexLexer):
    tokens = {"root": [(r"(\\w+)+$", Name), (r"[\\s\\S]", Text)]}

lx = EvilWordLexer()
assert not rsgments.compile_lexer(lx).stats()[5]
t0 = time.perf_counter()
res = rsgments.lex_tokens(lx, "w" * 2000 + "!")
dt = time.perf_counter() - t0
assert dt < 0.25, f"Too slow: {dt:.3f}s"
assert len(res) == 2001
"""
    proc = subprocess.run(
        [sys.executable, "-c", script],
        capture_output=True,
        text=True,
        timeout=2.0,
    )
    assert proc.returncode == 0, proc.stderr


def test_redos_fancy_backtracking_budget_aborts_cleanly_in_subprocess():
    script = """
import time
from pygments.lexer import RegexLexer
from pygments.token import Name, Text
import rsgments

class EvilBackrefLexer(RegexLexer):
    tokens = {"root": [(r"(a|aa)+\\1$", Name), (r"[\\s\\S]", Text)]}

lx = EvilBackrefLexer()
t0 = time.perf_counter()
try:
    rsgments.lex_tokens(lx, "a" * 50 + "!")
except TimeoutError:
    dt = time.perf_counter() - t0
    assert dt < 0.5, f"Budget abort took too long: {dt:.3f}s"
else:
    raise AssertionError("Expected TimeoutError on catastrophic backtracking")
"""
    proc = subprocess.run(
        [sys.executable, "-c", script],
        capture_output=True,
        text=True,
        timeout=2.0,
    )
    assert proc.returncode == 0, proc.stderr
