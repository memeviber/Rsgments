from __future__ import annotations

from contextlib import contextmanager
import re
import threading
import weakref
from typing import Iterable, Iterator, List, Optional, Tuple

from pygments.lexer import ExtendedRegexLexer, RegexLexer
from pygments.token import Error, Text, Whitespace, _TokenType

from ._rsgments import CompiledLexer, CompiledLexerBuilder, LexProfile

__all__ = [
    "__version__",
    "patch_pygments",
    "unpatch_pygments",
    "patched",
    "unpatched",
    "is_patched",
    "is_supported",
    "compile_lexer",
    "clear_cache",
    "lex_tokens",
    "lex_spans",
    "lex_raw_spans",
    "lex_flat_spans",
    "lex_flat_byte_spans",
    "lex_flat_spans_profiled",
    "CompiledLexer",
    "LexProfile",
]

__version__ = "0.3.1"

_SKIP_TOKEN = 0xFFFFFFFF
_orig_get_tokens_unprocessed = RegexLexer.get_tokens_unprocessed

_EOL_TOKEN = (
    Whitespace
    if "Whitespace" in _orig_get_tokens_unprocessed.__code__.co_names
    else Text
)

_compile_lock = threading.Lock()
_patch_cond = threading.Condition()
_tls = threading.local()

_patched = False
_draining = False
_in_flight = 0

_class_cache: weakref.WeakKeyDictionary[type, CompiledLexer] = (
    weakref.WeakKeyDictionary()
)
_instance_cache: weakref.WeakKeyDictionary[object, CompiledLexer] = (
    weakref.WeakKeyDictionary()
)


def is_supported(lexer: object) -> bool:
    """
    Check whether a lexer is compatible with Rsgments.
    ExtendedRegexLexer subclasses manage LexerContext themselves and are unsupported.
    """
    if not isinstance(lexer, RegexLexer) or isinstance(lexer, ExtendedRegexLexer):
        return False
    return type(lexer).get_tokens_unprocessed in (
        _orig_get_tokens_unprocessed,
        _rs_get_tokens_unprocessed,
    )


def _ensure_supported(lexer: object) -> None:
    if isinstance(lexer, ExtendedRegexLexer):
        raise TypeError(
            f"{type(lexer).__name__} inherits ExtendedRegexLexer (uses LexerContext), "
            "which is not supported directly by Rsgments."
        )
    if not isinstance(lexer, RegexLexer):
        raise TypeError(f"Expected a Pygments RegexLexer, got {type(lexer).__name__}.")


def _register_token(
    tok: Optional[_TokenType],
    id_to_token: list[_TokenType],
    token_to_id: dict[_TokenType, int],
) -> int:
    if tok is None:
        return _SKIP_TOKEN
    tid = token_to_id.get(tok)
    if tid is None:
        tid = len(id_to_token)
        id_to_token.append(tok)
        token_to_id[tok] = tid
    return tid


def _extract_bygroups_args(action) -> Optional[tuple]:
    qual = getattr(action, "__qualname__", "")
    if not qual.startswith("bygroups."):
        return None
    closure = getattr(action, "__closure__", None)
    freevars = getattr(getattr(action, "__code__", None), "co_freevars", ())
    if not closure or "args" not in freevars:
        return None
    args = closure[freevars.index("args")].cell_contents
    if not isinstance(args, tuple):
        return None
    for item in args:
        if item is not None and type(item) is not _TokenType:
            return None
    return args


def _compile_state_ops(new_state, state_to_id: dict[str, int]) -> list[tuple[str, int]]:
    if new_state is None:
        return []
    ops: list[tuple[str, int]] = []
    if isinstance(new_state, int):
        if new_state < 0:
            ops.append(("pop", -new_state))
        return ops
    if isinstance(new_state, str):
        if new_state == "#push":
            ops.append(("push_current", 0))
        elif new_state == "#pop":
            ops.append(("pop", 1))
        elif new_state.startswith("#pop:"):
            ops.append(("pop", int(new_state.split(":", 1)[1])))
        else:
            ops.append(("push", state_to_id[new_state]))
        return ops
    if isinstance(new_state, (tuple, list)):
        for st in new_state:
            if isinstance(st, int):
                if st < 0:
                    ops.append(("pop", -st))
            elif st == "#pop":
                ops.append(("pop", 1))
            elif isinstance(st, str) and st.startswith("#pop:"):
                ops.append(("pop", int(st.split(":", 1)[1])))
            elif st == "#push":
                ops.append(("push_current", 0))
            else:
                ops.append(("push", state_to_id[st]))
        return ops
    raise TypeError(f"Unsupported Pygments new_state: {new_state!r}")


def _build_compiled_lexer(lexer: RegexLexer) -> CompiledLexer:
    tokendefs = lexer._tokens
    state_names = list(tokendefs.keys())
    state_to_id = {name: i for i, name in enumerate(state_names)}

    id_to_token: list[_TokenType] = []
    token_to_id: dict[_TokenType, int] = {}

    eol_id = _register_token(_EOL_TOKEN, id_to_token, token_to_id)
    err_id = _register_token(Error, id_to_token, token_to_id)
    reg_lock = threading.RLock()

    builder = CompiledLexerBuilder(
        state_names,
        "root",
        eol_id,
        err_id,
        id_to_token,
        token_to_id,
        reg_lock,
    )

    for state_name, rules in tokendefs.items():
        for rexmatch, action, new_state in rules:
            pat_obj = getattr(rexmatch, "__self__", None)
            if isinstance(pat_obj, re.Pattern):
                pattern = pat_obj.pattern
                flags = pat_obj.flags
                ignore_case = bool(flags & re.IGNORECASE)
                multi_line = bool(flags & re.MULTILINE)
                dot_all = bool(flags & re.DOTALL)
                verbose = bool(flags & re.VERBOSE)
                ascii_flag = bool(flags & re.ASCII)
                locale_flag = bool(flags & re.LOCALE)
                try:
                    pattern.encode("utf-8")
                except UnicodeEncodeError:
                    # Python's `re` allows surrogates U+D800..U+DFFF in patterns,
                    # but Rust `&str` requires valid UTF-8 -> fall back to PyOnly.
                    pattern = None
            else:
                pattern = None
                ignore_case = multi_line = dot_all = verbose = ascii_flag = (
                    locale_flag
                ) = False

            ops = _compile_state_ops(new_state, state_to_id)

            if action is None:
                builder.add_rule(
                    state_name,
                    pattern,
                    ignore_case,
                    multi_line,
                    dot_all,
                    verbose,
                    ascii_flag,
                    locale_flag,
                    "none",
                    0,
                    [],
                    ops,
                    rexmatch,
                    None,
                )
            elif type(action) is _TokenType:
                tid = _register_token(action, id_to_token, token_to_id)
                builder.add_rule(
                    state_name,
                    pattern,
                    ignore_case,
                    multi_line,
                    dot_all,
                    verbose,
                    ascii_flag,
                    locale_flag,
                    "token",
                    tid,
                    [],
                    ops,
                    rexmatch,
                    None,
                )
            else:
                bg_args = _extract_bygroups_args(action)
                if bg_args is not None:
                    group_ids = [
                        _register_token(t, id_to_token, token_to_id) for t in bg_args
                    ]
                    builder.add_rule(
                        state_name,
                        pattern,
                        ignore_case,
                        multi_line,
                        dot_all,
                        verbose,
                        ascii_flag,
                        locale_flag,
                        "bygroups",
                        0,
                        group_ids,
                        ops,
                        rexmatch,
                        action,
                    )
                else:
                    builder.add_rule(
                        state_name,
                        pattern,
                        ignore_case,
                        multi_line,
                        dot_all,
                        verbose,
                        ascii_flag,
                        locale_flag,
                        "py",
                        0,
                        [],
                        ops,
                        rexmatch,
                        action,
                    )

    return builder.build()


def compile_lexer(lexer: RegexLexer) -> CompiledLexer:
    """
    Compile and cache a CompiledLexer for a Pygments RegexLexer class or instance.
    The cache is independent of the global patch state.
    """
    _ensure_supported(lexer)
    is_instance_custom = "_tokens" in lexer.__dict__

    if is_instance_custom:
        cached = _instance_cache.get(lexer)
        if cached is not None:
            return cached
    else:
        cached = _class_cache.get(type(lexer))
        if cached is not None:
            return cached

    with _compile_lock:
        if is_instance_custom:
            cached = _instance_cache.get(lexer)
        else:
            cached = _class_cache.get(type(lexer))
        if cached is not None:
            return cached

        compiled = _build_compiled_lexer(lexer)
        if is_instance_custom:
            _instance_cache[lexer] = compiled
        else:
            _class_cache[type(lexer)] = compiled
        return compiled


def clear_cache() -> None:
    """Clear the CompiledLexer cache without changing the global patch state."""
    with _compile_lock:
        _class_cache.clear()
        _instance_cache.clear()


def _rs_get_tokens_unprocessed(self, text: str, stack=("root",)):
    global _in_flight
    depth = getattr(_tls, "depth", 0)

    with _patch_cond:
        # If the lexer is unsupported by Rsgments (e.g. it inherits from ExtendedRegexLexer)
        # or is unpatched / draining -> fall back directly to vanilla Pygments.
        if (depth == 0 and (not _patched or _draining)) or not is_supported(self):
            use_orig = True
        else:
            use_orig = False
            _in_flight += 1
            _tls.depth = depth + 1

    if use_orig:
        return _orig_get_tokens_unprocessed(self, text, stack)

    fallback_orig = False
    try:
        compiled = compile_lexer(self)
        tokens = compiled.lex_py_tokens(text, self, list(stack))
        return iter(tokens)
    except UnicodeEncodeError:
        fallback_orig = True
    finally:
        with _patch_cond:
            _tls.depth = depth
            _in_flight -= 1
            if _in_flight == 0:
                _patch_cond.notify_all()

    if fallback_orig:
        return _orig_get_tokens_unprocessed(self, text, stack)

def lex_tokens(
    lexer: RegexLexer,
    text: str,
    stack: Iterable[str] = ("root",),
) -> List[Tuple[int, _TokenType, str]]:
    """Tokenize directly with Rust, independently of the global patch state."""
    compiled = compile_lexer(lexer)
    return compiled.lex_py_tokens(text, lexer, list(stack))


def lex_spans(
    lexer: RegexLexer,
    text: str,
    stack: Iterable[str] = ("root",),
) -> Tuple[List[Tuple[int, int, _TokenType]], List[str]]:
    """Return ([(start_char, length_char, token_type), ...], final_state_stack)."""
    compiled = compile_lexer(lexer)
    raw_spans, table, final_stack = compiled.lex_spans(text, lexer, list(stack))
    return [(s, l, table[tid]) for s, l, tid in raw_spans], final_stack


def lex_raw_spans(
    lexer: RegexLexer,
    text: str,
    stack: Iterable[str] = ("root",),
) -> Tuple[List[Tuple[int, int, int]], Tuple[_TokenType, ...], List[str]]:
    """Return ([(start_char, length_char, tid), ...], token_table, final_state_stack)."""
    compiled = compile_lexer(lexer)
    return compiled.lex_spans(text, lexer, list(stack))


def lex_flat_spans(
    lexer: RegexLexer,
    text: str,
    stack: Iterable[str] = ("root",),
) -> Tuple[bytes, Tuple[_TokenType, ...], List[str]]:
    """
    Return a flat buffer for code editors such as PyQt5 QSyntaxHighlighter.
    The result is (bytes_buf, token_table, final_state_stack).
    Read it with `mv = memoryview(bytes_buf).cast("I")`:
    `start, length, tid = mv[i], mv[i + 1], mv[i + 2]`.
    """
    compiled = compile_lexer(lexer)
    return compiled.lex_flat_spans(text, lexer, list(stack))


def lex_flat_byte_spans(
    lexer: RegexLexer,
    text: str,
    stack: Iterable[str] = ("root",),
    start_byte: int = 0,
    end_byte: Optional[int] = None,
) -> Tuple[bytes, Tuple[_TokenType, ...], List[str]]:
    """
    Return flat UTF-8 byte-offset spans for QScintilla.
    Only tokens intersecting `[start_byte, end_byte)` are emitted, and lexing stops at
    `end_byte`. The result is (bytes_buf, token_table, final_state_stack).
    Read the buffer with `memoryview(bytes_buf).cast("I")` as triples of
    `(byte_start, byte_length, token_id)`.
    """
    compiled = compile_lexer(lexer)
    return compiled.lex_flat_byte_spans(text, lexer, list(stack), start_byte, end_byte)


def lex_flat_spans_profiled(
    lexer: RegexLexer,
    text: str,
    stack: Iterable[str] = ("root",),
) -> Tuple[bytes, Tuple[_TokenType, ...], List[str], LexProfile]:
    """Lex a text and return flat spans with profiling information."""
    compiled = compile_lexer(lexer)
    return compiled.lex_flat_spans_profiled(text, lexer, list(stack))


def patch_pygments() -> None:
    """Enable Rust acceleration for Pygments RegexLexer instances."""
    global _patched
    with _patch_cond:
        while _draining:
            _patch_cond.wait()
        if not _patched:
            RegexLexer.get_tokens_unprocessed = _rs_get_tokens_unprocessed
            _patched = True


def unpatch_pygments() -> None:
    """
    Wait for active Rust lexing calls to finish, then restore Pygments.
    The compiled lexer cache is preserved.
    """
    global _patched, _draining
    with _patch_cond:
        while _draining and getattr(_tls, "depth", 0) == 0:
            _patch_cond.wait()
        if not _patched:
            return
        _draining = True
        try:
            while _in_flight > 0 and getattr(_tls, "depth", 0) == 0:
                _patch_cond.wait()
            RegexLexer.get_tokens_unprocessed = _orig_get_tokens_unprocessed
            _patched = False
        finally:
            _draining = False
            _patch_cond.notify_all()


def is_patched() -> bool:
    with _patch_cond:
        return _patched and not _draining


@contextmanager
def patched() -> Iterator[None]:
    """Temporarily enable Rsgments within a `with` block."""
    was_patched = is_patched()
    if not was_patched:
        patch_pygments()
    try:
        yield
    finally:
        if not was_patched:
            unpatch_pygments()


@contextmanager
def unpatched() -> Iterator[None]:
    """Temporarily disable Rsgments within a `with` block."""
    was_patched = is_patched()
    if was_patched:
        unpatch_pygments()
    try:
        yield
    finally:
        if was_patched:
            patch_pygments()
