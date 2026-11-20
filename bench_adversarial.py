import statistics
import sys
import time

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8")

from pygments.lexer import RegexLexer, bygroups, using
from pygments.token import (
    Comment,
    Keyword,
    Name,
    Number,
    Operator,
    Punctuation,
    String,
    Text,
)

import rsgments


# Sub-lexer used to test PyCallback (`using(...)`).
class EmbeddedExprLexer(RegexLexer):
    tokens = {
        "root": [
            (r"\d+", Number),
            (r"[A-Za-z_]\w*", Name),
            (r"[+\-*/]", Operator),
            (r"\s+", Text),
        ]
    }


# 1. Native-heavy Lexer (~15 rules, 95%+ MultiDFA)
class NativeHeavyLexer(RegexLexer):
    tokens = {
        "root": [
            (r"[ \t\r\n]+", Text),
            (r"#[^\n]*", Comment.Single),
            (r'"[^"\n]*"', String),
            (
                r"\b(?:fn|let|mut|if|else|for|while|return|struct|enum|match|type)\b",
                Keyword,
            ),
            (r"\b(?:true|false|None|Some|Ok|Err)\b", Keyword.Constant),
            (r"0x[0-9a-fA-F_]+", Number.Hex),
            (r"\d+\.\d+(?:e[+-]?\d+)?", Number.Float),
            (r"\d+", Number.Integer),
            (r"(?:->|=>|::|==|!=|<=|>=|&&|\|\|)", Operator),
            (r"[+\-*/%=<>!&|^~]", Operator),
            (r"[()\[\]{},.;:]", Punctuation),
            (r"(?<!\.)\b(?:len|print|range|zip|map)\b", Name.Builtin),
            (r"([A-Za-z_]\w*)(\s*)(\()", bygroups(Name.Function, Text, Punctuation)),
            (r"[A-Za-z_]\w*", Name),
            (
                r"(@)?NEVER_HIT(?(1)\(\d+\)|:\w+)",
                Keyword.Pseudo,
            ),  # Genuine PyOnly rule at the bottom.
        ]
    }


# 2. Fancy-heavy Lexer (Lookbehind, Lookahead, Backreference, False Positives)
class FancyHeavyLexer(RegexLexer):
    tokens = {
        "root": [
            (r"[ \t\r\n]+", Text),
            # Fixed-width lookbehind (valid in Python's re).
            (r"(?<=call )([A-Za-z_]\w*)", Name.Function),
            # Lookbehind + lookahead for builtins -> causes false positives when preceded by '.' or missing '('.
            (
                r"(?<!\.)\b(?:len|sum|min|max|abs|zip|map|filter)\b(?=\s*\()",
                Name.Builtin,
            ),
            # Soft-keyword lookahead can fail when followed by '=', ':', or '.'.
            (r"(?<![.\w])\b(?:match|case|when|where)\b(?![ \t]*[:=.,])", Keyword),
            # Backreference (unindexed -> excluded from MultiDFA, always handled by Fancy).
            (r"(['\"])([^'\"\n]*)\1", bygroups(String.Delimiter, String)),
            (r"\b(?:def|return|if|else|call)\b", Keyword),
            (r"\d+", Number),
            (r"[A-Za-z_]\w*", Name),
            (r"[()\[\]{}.,:=+\-*/'\"]", Punctuation),
        ]
    }


def make_pyonly_rules(prefix: str, count: int = 10):
    first_ch = prefix[0]
    rest = prefix[1:]
    escaped_first = rf"\N{{LATIN SMALL LETTER {first_ch.upper()}}}"
    rules = []
    for i in range(count):
        if i % 3 == 0:
            rules.append(
                (rf"@?{escaped_first}{rest}{i}(?:\(\d+\)|:\w+)", Keyword.Reserved)
            )
        elif i % 3 == 1:
            rules.append((rf"!?{escaped_first}{rest}{i}_[a-z]+\d*", Name.Decorator))
        else:
            rules.append((rf"{escaped_first}{rest}{i}\w+", Name.Tag))
    return rules


# 3A. PyOnly-bottom Lexer: 10 PyOnly rules placed at the END of the list
# (rarely checked).
class PyOnlyBottomLexer(RegexLexer):
    tokens = {
        "root": [
            (r"[ \t\r\n]+", Text),
            (r"#[^\n]*", Comment.Single),
            (r'"[^"\n]*"', String),
            (r"\b(?:def|let|if|else|return|for|while|break|continue)\b", Keyword),
            (r"\d+", Number),
            (r"[+\-*/%=<>!&|]+", Operator),
            (r"[()\[\]{},.;:!@]", Punctuation),
            # Match any identifier except `pyrule*`, allowing `pyrule*` to fall through to the 10 PyOnly rules at the bottom.
            (r"(?!pyrule\d)[A-Za-z_]\w*", Name),
            *make_pyonly_rules("pyrule", 10),
            (r"[A-Za-z_]\w*", Name),
        ]
    }


# 3B. PyOnly-top (Adversarial 25 rules): 10 PyOnly rules placed at the VERY
# BEGINNING of the list!
class PyOnlyTopLexer(RegexLexer):
    tokens = {
        "root": [
            *make_pyonly_rules("pyrule", 10),
            (r"[ \t\r\n]+", Text),
            (r"#[^\n]*", Comment.Single),
            (r'"[^"\n]*"', String),
            (r"\b(?:def|let|if|else|return|for|while|break|continue)\b", Keyword),
            (r"\b(?:true|false|null|none)\b", Keyword.Constant),
            (r"0x[0-9a-fA-F]+", Number.Hex),
            (r"\d+\.\d+", Number.Float),
            (r"\d+", Number.Integer),
            (r"(?:==|!=|<=|>=|=>|->)", Operator),
            (r"[+\-*/%=<>!&|]", Operator),
            (r"[()\[\]{},.;:!@]", Punctuation),
            (r"([A-Za-z_]\w*)(\s*)(\()", bygroups(Name.Function, Text, Punctuation)),
            (r"[A-Za-z_]\w*", Name),
            (r"\$\{([^}\n]+)\}", bygroups(using(EmbeddedExprLexer))),
        ]
    }


# 4. Mixed Adversarial Lexer: Native + Fancy + PyOnly + using() + UTF-8
class MixedAdversarialLexer(RegexLexer):
    tokens = {
        "root": [
            (r"@?\N{LATIN SMALL LETTER T}ag(?:\(\d+\)|:\w+)", Keyword.Pseudo),
            (r"(?<!\.)\b(?:len|print|eval|exec|hash)\b(?=\s*\()", Name.Builtin),
            (r"(?<=fn )([A-Za-z_]\w*)", Name.Function),
            (r"(['\"])([^'\"\n]*)\1", bygroups(String.Delimiter, String)),
            (
                r"(\{\{)([^}\n]+)(\}\})",
                bygroups(Punctuation, using(EmbeddedExprLexer), Punctuation),
            ),
            (r"(?a)asc_\w+", Name.Variable),
            (r"#[^\n]*", Comment.Single),
            (r"\b(?:fn|let|return|if|else)\b", Keyword),
            (r"\d+", Number),
            (r"[A-Za-z_]\w*", Name),
            (r"[ \t\r\n]+", Text),
            (r"[()\[\]{}.,:=@+\-*/'\"]", Punctuation),
        ]
    }


# Generate input data for each scenario and fallback ratio.
def gen_native_text(repeats: int = 800) -> str:
    chunk = (
        "fn compute(alpha, beta) {\n"
        '    let msg = "hello native";\n'
        "    if alpha >= 10 && beta != 0xFF {\n"
        "        return len(alpha) + compute(beta, 3.14e-1);\n"
        "    }\n"
        "}\n"
    )
    return chunk * repeats


def gen_fancy_text(repeats: int = 1200) -> str:
    # Intentionally introduce ~50% verification failures:
    # self.len(x), obj.sum, match = 1, and case.ok.
    chunk = (
        "call my_func(len(x), self.len(y), sum(z), obj.sum)\n"
        "match x: case 1: call   run_it(abs(a), b.abs(a))\n"
        "match = 10; case = 'ok' \"quoted\" 'bad\n"
    )
    return chunk * repeats


def gen_pyonly_text(pct_fallback: int, total_items: int = 8000) -> str:
    py_samples = [
        "@pyrule0(12)",
        "pyrule0:abc",
        "!pyrule1_test99",
        "pyrule1_other",
        "pyrule2fast",
        "@pyrule3(7)",
        "!pyrule4_val8",
        "pyrule5xyz",
        "pyrule6:item",
        "pyrule7_ok1",
        "pyrule8run",
        "@pyrule9(5)",
    ]
    native_samples = [
        "alpha",
        "beta",
        "gamma",
        "delta",
        "12345",
        "return",
        "value",
        "999",
    ]

    parts = []
    for i in range(total_items):
        if (i % 100) < pct_fallback:
            parts.append(py_samples[i % len(py_samples)])
        else:
            parts.append(native_samples[i % len(native_samples)])
        parts.append("\n" if i % 10 == 9 else " ")
    return "".join(parts)


def gen_mixed_text(repeats: int = 1000) -> str:
    chunk = (
        "# Ghi chú tiếng Việt có dấu 🚀\n"
        "@tag(42) tag:cfg {{ 10 + alpha * 2 }} asc_val1\n"
        'fn solve(x) return len(x) + obj.len(x) + "ok"\n'
    )
    return chunk * repeats


# Benchmark runner + 100% parity verification.
def run_case(
    title: str,
    lexer: RegexLexer,
    text: str,
    warmup: int = 2,
    runs: int = 7,
    require_py_fallback: bool = False,
):
    with rsgments.unpatched():
        expected = list(lexer.get_tokens_unprocessed(text))
    with rsgments.patched():
        actual = list(lexer.get_tokens_unprocessed(text))
    assert actual == expected, f"Token parity FAILED on {title}!"

    compiled = rsgments.compile_lexer(lexer)
    states, rules, auto, fancy_idx, unidx, py_fb = compiled.stats()
    if require_py_fallback:
        assert py_fb and unidx >= 10, (
            f"Expected >=10 PyOnly unindexed rules, got {unidx}, py_fb={py_fb}"
        )

    for _ in range(warmup):
        with rsgments.unpatched():
            _ = list(lexer.get_tokens_unprocessed(text))
        with rsgments.patched():
            _ = list(lexer.get_tokens_unprocessed(text))
        _ = rsgments.lex_flat_spans(lexer, text)

    py_times = []
    for _ in range(runs):
        with rsgments.unpatched():
            t0 = time.perf_counter()
            _ = list(lexer.get_tokens_unprocessed(text))
            py_times.append(time.perf_counter() - t0)

    rs_gt_times = []
    for _ in range(runs):
        with rsgments.patched():
            t0 = time.perf_counter()
            _ = list(lexer.get_tokens_unprocessed(text))
            rs_gt_times.append(time.perf_counter() - t0)

    rs_flat_times = []
    for _ in range(runs):
        t0 = time.perf_counter()
        _ = rsgments.lex_flat_spans(lexer, text)
        rs_flat_times.append(time.perf_counter() - t0)

    _, _, _, prof = rsgments.lex_flat_spans_profiled(lexer, text)

    t_py = statistics.median(py_times)
    t_gt = statistics.median(rs_gt_times)
    t_flat = statistics.median(rs_flat_times)

    tok_count = len(expected)
    mb_size = len(text.encode("utf-8")) / (1024 * 1024)
    py_total_calls = prof.py_regex_calls
    avg_py_ns = (prof.py_fallback_ns / py_total_calls) if py_total_calls else 0.0
    avg_fancy_ns = (
        (prof.fancy_ns / prof.fancy_verify_calls) if prof.fancy_verify_calls else 0.0
    )
    fp_rate = (
        (prof.superset_false_positives / prof.fancy_verify_calls * 100.0)
        if prof.fancy_verify_calls
        else 0.0
    )

    print(f"\n=== {title} ===")
    print(
        f"  Rules: total={rules} (MultiDFA={auto}, Fancy+Superset={fancy_idx}, "
        f"Unindexed={unidx}, py_fb={py_fb})"
    )
    print(
        f"  Input: {mb_size:.2f} MB, {tok_count:,} tokens | "
        f"Speed: Pygments={t_py * 1000:.2f}ms | "
        f"Rsgments(gt)={t_gt * 1000:.2f}ms ({t_py / t_gt:.2f}x) | "
        f"Rsgments(flat)={t_flat * 1000:.2f}ms ({t_py / t_flat:.2f}x)"
    )
    print(
        f"  Throughput(flat): {tok_count / t_flat / 1e3:,.1f} k tok/s "
        f"({mb_size / t_flat:.2f} MB/s)"
    )
    print(
        f"  Telemetry: dfa_searches={prof.dfa_searches:,} "
        f"(hybrid={prof.dfa_hybrid_hits:,}, meta={prof.dfa_meta_fallbacks:,})"
    )
    print(
        f"             fancy_calls={prof.fancy_verify_calls:,} "
        f"(avg={avg_fancy_ns / 1e3:.3f} us, "
        f"superset_fp={prof.superset_false_positives:,} [{fp_rate:.1f}%])"
    )
    print(
        f"             py_regex_calls={prof.py_regex_calls:,} "
        f"(hits={prof.py_regex_hits:,}, "
        f"callbacks={prof.py_callback_calls:,}, "
        f"avg_fallback={avg_py_ns / 1e3:.3f} us)"
    )


def main():
    print("Running Rsgments Adversarial & Fallback Overhead Benchmark...")

    run_case("1. Native-heavy", NativeHeavyLexer(), gen_native_text())
    run_case(
        "2. Fancy-heavy (lookaround + false positives + backref)",
        FancyHeavyLexer(),
        gen_fancy_text(),
    )
    run_case(
        "2. Fancy-heavy (lookaround + false positives + backref)",
        FancyHeavyLexer(),
        gen_fancy_text(),
    )

    run_case(
        "3A. PyOnly-bottom (10 PyOnly rules at the bottom, rarely checked)",
        PyOnlyBottomLexer(),
        gen_pyonly_text(5, 8000),
        require_py_fallback=True,
    )
    run_case(
        "3B. PyOnly-top (10 PyOnly rules at the TOP of 25 rules, 50% hit rate)",
        PyOnlyTopLexer(),
        gen_pyonly_text(50, 8000),
        require_py_fallback=True,
    )

    run_case(
        "4. Mixed Adversarial (Native + Fancy + PyOnly + using + UTF-8)",
        MixedAdversarialLexer(),
        gen_mixed_text(),
    )

    print("\n" + "=" * 72)
    print("FALLBACK SWEEP (PyOnlyTopLexer: 10 PyOnly rules at the TOP of 25 rules)")
    print("=" * 72)
    for pct in (10, 50, 90, 99):
        run_case(
            f"Sweep ~{pct}% PyOnly hit rate",
            PyOnlyTopLexer(),
            gen_pyonly_text(pct, 8000),
            require_py_fallback=True,
        )


if __name__ == "__main__":
    main()
