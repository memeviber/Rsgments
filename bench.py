import sys
import time

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8")

from pygments.lexers import PythonLexer
import rsgments


def make_large_python_code(lines: int = 6000) -> str:
    chunk = '''
# Metrics function with a Unicode comment and emoji 🚀💡
def compute_metrics(items: list[dict], factor: float = 1.5) -> dict[str, float]:
    """Multiline docstring
    with f-string and Unicode support: hello Lumos Editor!
    """
    total = 0xFF_00 + 3.14159e-2
    result = {f"key_{i}": (x.get("val", 0) * factor) for i, x in enumerate(items) if x}
    if len(result) > 10 and "alpha" in result:
        print(f"Tổng = {total}, tỷ lệ = {factor:.2f}")
    return result
'''
    repeats = max(1, lines // chunk.count("\n"))
    return chunk * repeats


def main():
    code = make_large_python_code(6000)
    lexer = PythonLexer()

    compiled = rsgments.compile_lexer(lexer)
    states, total, auto, fancy_idx, unidx, py_fb = compiled.stats()
    print(
        f"[PythonLexer stats] states={states}, rules={total}, "
        f"MultiDFA={auto}, Fancy+Superset={fancy_idx}, Unindexed={unidx}, py_fallback={py_fb}"
    )

    # Warm up both implementations.
    with rsgments.unpatched():
        _ = list(lexer.get_tokens_unprocessed(code[:500]))
    with rsgments.patched():
        _ = list(lexer.get_tokens_unprocessed(code[:500]))

    with rsgments.unpatched():
        t0 = time.perf_counter()
        orig_tokens = list(lexer.get_tokens_unprocessed(code))
        t_orig = time.perf_counter() - t0

    with rsgments.patched():
        t0 = time.perf_counter()
        rs_tokens = list(lexer.get_tokens_unprocessed(code))
        t_rs = time.perf_counter() - t0

    t0 = time.perf_counter()
    raw_spans, _, _ = rsgments.lex_raw_spans(lexer, code)
    t_span = time.perf_counter() - t0

    t0 = time.perf_counter()
    flat_buf, _, _ = rsgments.lex_flat_spans(lexer, code)
    t_flat = time.perf_counter() - t0

    assert len(orig_tokens) == len(rs_tokens) == len(raw_spans) == (len(flat_buf) // 12)
    print(
        f"--- Benchmark (~{code.count(chr(10))} lines, {len(orig_tokens)} tokens) ---"
    )
    print(f"Original Pygments           : {t_orig * 1000:7.2f} ms (1.0x)")
    print(
        f"Rsgments (get_tokens)      : {t_rs * 1000:7.2f} ms ({t_orig / t_rs:.1f}x faster)"
    )
    print(
        f"Rsgments (lex_raw_spans)   : {t_span * 1000:7.2f} ms ({t_orig / t_span:.1f}x faster)"
    )
    print(
        f"Rsgments (lex_flat_spans)  : {t_flat * 1000:7.2f} ms ({t_orig / t_flat:.1f}x faster)"
    )


if __name__ == "__main__":
    main()
