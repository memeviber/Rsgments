# Rsgments

Rsgments is a Rust-accelerated state machine engine for [Pygments](https://pygments.org/), built with PyO3 and maturin.

It compiles Pygments `RegexLexer` rules into Rust regex engines where possible and falls back to Python for unsupported patterns and callbacks. The package includes token, character-span, and UTF-8 byte-span APIs.

## Installation

The easiest way to install Rsgments is directly from PyPI:

```bash
pip install Rsgments
```

This installs Rsgments and its required dependencies into your active Python environment.

To upgrade to the latest available version:

```bash
pip install --upgrade Rsgments
```

## Requirements

For source builds and development:

- Python 3.9 or newer
- Rust toolchain
- maturin 1.9.4 or newer, below 2.0
- Pygments 2.14 or newer

## Build from source

To build and install Rsgments from source, first install the build dependencies:

```powershell
python -m pip install maturin
python -m pip install -e ".[dev]"
```

Alternatively, build a release wheel:

```powershell
maturin build --release
```

The resulting wheel is written to `target/wheels/`.

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

`(character_start, character_length, token_id)`.

Read it with:

```python
memoryview(buffer).cast("I")
```

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

This returns little-endian `u32` triples of `(byte_start, byte_length, token_id)`, clipped to the requested range.

Byte offsets refer to the UTF-8 encoding of `source`. Pass the final state stack when lexing subsequent editor blocks.

Use `rsgments.unpatch_pygments()` to restore the original Pygments implementation.

`rsgments.is_supported(lexer)` checks whether a lexer is supported by the direct Rsgments APIs. `ExtendedRegexLexer` is not supported directly.

## Tests and benchmark

Install the development dependencies and run the test suite:

```powershell
python -m pip install -e ".[dev]"
python -m pytest -v
python bench.py
```

### Benchmark comparison

The following results are the trimmed means of 14 benchmark runs. The fastest and slowest runs were discarded, and the remaining 12 runs were averaged.

Each run processed approximately 5,995 lines and 81,750 tokens.

| Implementation | Average time | Speedup |
|---|---:|---:|
| Original Pygments | 570.58 ms | 1.00× |
| Rsgments (`get_tokens`) | 74.95 ms | 7.61× |
| Rsgments (`lex_raw_spans`) | 55.95 ms | 10.20× |
| Rsgments (`lex_flat_spans`) | 49.80 ms | 11.46× |

Compared with original Pygments, these averages represent approximately:

- `get_tokens`: 86.9% less processing time
- `lex_raw_spans`: 90.2% less processing time
- `lex_flat_spans`: 91.3% less processing time

`lex_flat_spans` averaged 49.80 ms for this workload.

Results are specific to the benchmark machine and workload. Run `python bench.py` to measure performance in your own environment.

## Rsgments vs. rustgments

[Rsgments](https://github.com/memeviber/rsgments) and [rustgments](https://pypi.org/project/rustgments/) are independent projects that use Rust to accelerate Pygments. Despite their similar names and goals, they take fundamentally different approaches.

**Note:** `rustgments` is an independently developed third-party package published on PyPI. It is not an official Pygments package or an implementation maintained by the Pygments project.

### Architectural differences

| Aspect | Rsgments | rustgments |
|---|---|---|
| Lexer table handling | Compiles live Pygments lexer definitions at runtime | Generates and packages processed lexer tables at build time |
| Lexer discovery | Uses the installed lexer's actual `_tokens` definitions | Uses a predefined set of generated lexer tables |
| Custom lexers | Can potentially support dynamically registered `RegexLexer` classes | Generally falls back to Pygments for unsupported or unbundled lexers |
| Pygments version handling | Designed around runtime compilation rather than a single generated table set | Generated tables are tied to a specific Pygments release |
| Runtime compilation | Compiles lexer rules when needed and caches the result | Avoids compiling Pygments lexer tables at runtime |
| Low-level output | Provides character-span and UTF-8 byte-span APIs | Primarily focuses on Pygments-compatible tokenization and highlighting |
| Main design priority | Runtime flexibility and editor integration | Reproducibility, verified coverage, and predictable compatibility |

### Why Rsgments takes a runtime approach

Rsgments compiles the lexer definitions that are actually present in the running Python environment.

Instead of shipping a fixed snapshot of processed Pygments lexer tables, it reads the live `_tokens` definitions and translates supported rules into native Rust execution.

This approach has several potential advantages:

- **Runtime flexibility:** New or dynamically registered `RegexLexer` implementations can potentially be compiled without regenerating and shipping a new set of lexer tables.
- **Less dependence on generated snapshots:** Rsgments does not need to package a separate pre-generated table set for every supported Pygments release.
- **Better integration with custom environments:** Applications that register their own lexers can benefit from the same runtime compilation mechanism, provided their lexer rules are supported.
- **Editor-oriented APIs:** Rsgments exposes flat span buffers, including UTF-8 byte offsets. This is particularly useful for editors and syntax-highlighting integrations that need to map tokens directly to document positions.
- **Incremental processing potential:** The byte-span API accepts a stack and byte range, allowing callers to continue lexing from a previous state and process bounded regions of source text.

### Why rustgments takes a build-time approach

rustgments uses a different trade-off. It processes Pygments lexer definitions during its build process and ships the resulting tables with the package.

This has practical benefits:

- Lexer tables are already processed before runtime.
- The project can test each bundled lexer against Pygments and only enable native execution when its parity checks pass.
- It avoids the need to implement general runtime translation of Pygments lexer definitions.
- Its supported native lexer set and compatibility conditions are explicitly documented.

The trade-off is that its generated tables are tied to particular Pygments and Python Unicode versions. New lexer definitions or changes to Pygments may require regenerating the tables and publishing an updated release.

### Different priorities, not interchangeable implementations

Neither architecture is universally better.

- **Rsgments** prioritizes runtime compilation, flexibility, and low-level interfaces suitable for editor integrations.
- **rustgments** prioritizes a predefined, extensively verified set of native lexers and reproducible behavior for supported configurations.

Rsgments' runtime approach is particularly useful when an application needs to work with live Pygments lexer definitions, custom lexer registrations, or byte-oriented source-processing workflows.

Actual compatibility still depends on the lexer constructs supported by Rsgments. Unsupported patterns and callbacks must use the appropriate fallback path, and compatibility should be established through parity tests rather than assumed.

The two projects share a similar goal, but their architectures solve different engineering problems.

## License

Rsgments is licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
