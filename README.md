# Rsgments

Rsgments is a Rust-accelerated state machine engine for [Pygments](https://pygments.org/), built with PyO3 and maturin.

It compiles Pygments `RegexLexer` rules into Rust regex engines where possible and falls back to Python for unsupported patterns and callbacks. The package includes token, character-span, and UTF-8 byte-span APIs.

## Requirements

- Python 3.9 or newer
- Rust toolchain
- maturin 1.9.4 or newer, below 2.0
- Pygments 2.14 or newer

## Build and install

Install the package in the active Python environment:

```powershell
python -m pip install maturin
python -m pip install -e ".[dev]"
```

Or build a release wheel:

```powershell
maturin build --release
```

The wheel is written to `target/wheels/`.

## Usage

Patch Pygments globally:

```python
import rsgments

rsgments.patch_pygments()
```

The patch can be scoped with a context manager:

```python
with rsgments.patched():
    tokens = list(lexer.get_tokens_unprocessed(source))
```

Direct APIs are also available without changing Pygments' global behavior:

```python
import rsgments
from pygments.lexers import PythonLexer

lexer = PythonLexer()
spans, token_table, final_stack = rsgments.lex_flat_spans(lexer, source)
```

`lex_flat_spans` returns a `bytes` buffer containing little-endian `u32` triples:
`(character_start, character_length, token_id)`. Read it with
`memoryview(buffer).cast("I")`.

For byte-oriented editor integrations, request only a UTF-8 byte range:

```python
buffer, token_table, final_stack = rsgments.lex_flat_byte_spans(
    lexer,
    source,
    stack=("root",),
    start_byte=0,
    end_byte=256,
)
```

This returns little-endian `u32` triples of
`(byte_start, byte_length, token_id)`, clipped to the requested range. Byte offsets
refer to the UTF-8 encoding of `source`. Pass the final state stack when lexing
subsequent editor blocks.

Use `rsgments.unpatch_pygments()` to restore the original Pygments implementation.
`rsgments.is_supported(lexer)` checks whether a lexer is supported by the direct
Rsgments APIs; `ExtendedRegexLexer` is not supported directly.

## Tests and benchmark

Install the development dependency and run the test suite:

```powershell
python -m pip install -e ".[dev]"
python -m pytest -v
python bench.py
```

## License

Rsgments is licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
