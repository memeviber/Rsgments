# Rsgments

Rsgments is a Rust-accelerated state machine engine for [Pygments](https://pygments.org/), built with PyO3 and maturin.

It compiles Pygments `RegexLexer` rules into native Rust state machines and regex engines whenever possible, while transparently falling back to Python for unsupported patterns and callbacks. Rsgments provides standard Pygments token streams, character-span buffers, and UTF-8 byte-span APIs designed for code editors and syntax-highlighting integrations.

## Installation

Install a prebuilt wheel directly from PyPI:

```bash
pip install rsgments
```

Prebuilt wheels are available for Python 3.9 through 3.14 across Linux (`manylinux` and `musllinux`), macOS, and Windows, including free-threaded CPython `3.14t` and `3.13t` builds.

To upgrade to the latest release:

```bash
pip install --upgrade rsgments
```

## Requirements

The following dependencies are required for source builds and local development:

- Python 3.9 or newer, including free-threaded CPython 3.13t and 3.14t
- Rust toolchain
- `maturin` >= 1.9.4, < 2.0
- `Pygments` >= 2.14

## Building from Source

Install the development dependencies and build the package in editable mode:

```powershell
python -m pip install maturin
python -m pip install -e ".[dev]"
```

Alternatively, build a release wheel directly:

```powershell
maturin build --release
```

The resulting wheel is written to `target/wheels/`.

## Usage

### Global and Scoped Patching

Patch Pygments `RegexLexer` globally:

```python
import rsgments

rsgments.patch_pygments()
```

Alternatively, apply the patch only within a context manager:

```python
with rsgments.patched():
    tokens = list(lexer.get_tokens_unprocessed(source))
```

Call `rsgments.unpatch_pygments()` to restore the original Pygments implementation.

Active lexing calls, including nested `using(...)` sub-lexers, are allowed to finish before the original method is restored. Compiled lexer caches remain intact across patch and unpatch operations.

### Direct Editor APIs

The direct APIs can be used independently of the global patch state:

```python
import rsgments

from pygments.lexers import PythonLexer

lexer = PythonLexer()

buf, token_table, final_stack = rsgments.lex_flat_spans(lexer, source)
```

`lex_flat_spans` returns a flat `bytes` buffer containing little-endian `u32` triples:

```text
(character_start, character_length, token_id)
```

The buffer can be accessed without allocating individual Python token objects:

```python
mv = memoryview(buf).cast("I")
```

For byte-oriented editors such as QScintilla, use `lex_flat_byte_spans` to tokenize a specific UTF-8 byte range `[start_byte, end_byte)`:

```python
buf, token_table, final_stack = rsgments.lex_flat_byte_spans(
    lexer,
    source,
    stack=("root",),
    start_byte=0,
    end_byte=256,
)
```

This API returns little-endian `u32` triples containing:

```text
(byte_start, byte_length, token_id)
```

Spans are clipped to the requested range, and lexing stops as soon as `byte_pos >= end_byte`. Pass the returned `final_stack` to subsequent calls to continue lexing from the previous state.

Use `rsgments.is_supported(lexer)` to check whether a lexer is supported by the direct Rsgments APIs. `ExtendedRegexLexer` subclasses, which rely on `LexerContext`, are intentionally handled by the original Pygments implementation.

### Profiling API

The profiling API exposes engine routing statistics, superset false-positive rates, and Python fallback timing:

```python
buf, token_table, final_stack, profile = rsgments.lex_flat_spans_profiled(
    lexer,
    source,
)

print(
    profile.dfa_searches,
    profile.fancy_verify_calls,
    profile.superset_false_positives,
    profile.py_regex_calls,
    profile.py_fallback_ns,
)
```

Profiling uses compile-time monomorphization through `const PROFILE: bool` in Rust. Normal `lex_*` calls therefore incur no counter or timer overhead.

## Architecture

### Pygments Compatibility Layer

Rsgments does not treat Pygments lexer definitions as simple collections of regular expressions. Instead, it reproduces the commonly used parts of Pygments `RegexLexer` semantics in Rust, allowing lexer definitions to be compiled and executed natively without invoking Python for every rule.

The compatibility layer translates supported Pygments constructs into Rust-side representations while preserving their execution semantics:

- **State transitions:** Native handling of state operations such as `#push`, `#pop`, `#push(state)`, and dynamically resolved state transitions.
- **Rule composition:** Support for commonly used composition mechanisms, including `include()`, `inherit`, and state-level rule organization.
- **Token emission:** Native token identification and emission, including token types and their hierarchical relationships.
- **Regex compatibility:** Translation of Python-specific regex syntax and semantics before compilation into Rust regex engines.
- **Lexer execution semantics:** Preservation of rule ordering, first-match priority, state-stack behavior, and token position tracking.

Rather than reimplementing every Pygments feature, Rsgments focuses on constructs commonly used across the installed lexer ecosystem. Unsupported constructs and Python-dependent callbacks remain compatible through the Python fallback layer.

This separates Pygments compatibility from regex execution: the compatibility layer interprets lexer definitions, while the native execution engine processes the resulting compiled rules.

### Three-Tier Regex Execution Engine

Rsgments compiles live Pygments `_tokens` state tables into an immutable `#[pyclass(frozen)] CompiledLexer` using a three-tier execution model.

#### Tier 1 - Multi-Pattern DFA

**Components:** `regex-automata` (`HybridDFA` and `meta::Regex`)

Compatible rules in each state are compiled into a single anchored multi-pattern DFA using `MatchKind::LeftmostFirst`.

`HybridDFA` handles ASCII fast paths directly. If a token encounters non-ASCII boundaries that cause the lazy DFA to quit, only that token falls back to `meta::Regex` before execution resumes through `HybridDFA`.

#### Tier 2 - Lookaround Superset Prefiltering and Fancy Regex

**Components:** `fancy-regex` and suffix DFAs

Rules containing lookaheads or lookbehinds (`(?=...)`, `(?!...)`, `(?<=...)`, and `(?<!...)`) are transformed into valid superset regexes by removing their lookaround assertions. These supersets are then indexed in the state's Multi-DFA.

`fancy-regex`, anchored with `\G`, is invoked only when a superset matches. If verification fails, execution continues through a prebuilt suffix DFA starting at `win_idx + 1`.

Backreferences remain unindexed and are evaluated through `fancy-regex` only when no higher-priority DFA rule matches.

#### Tier 3 - Python Fallback

**Components:** `PyOnly` and `PyCallback`

Patterns requiring Python-specific behavior are routed back to Python through PyO3. Examples include:

- Inline `re.ASCII` mode (`(?a)`) combined with `\w`, `\d`, or `\s`.
- `re.LOCALE`.
- Unsupported regex syntax.
- Python callbacks such as `using(...)` and custom tokenizers.

Unindexed `PyOnly` rules are evaluated according to their original rule ordering:

- Rules placed before the first DFA rule are checked before invoking the DFA. This avoids unnecessary DFA searches when an earlier Python-only rule matches.
- Rules placed after DFA rules are skipped whenever a higher-priority DFA match wins (`u_idx >= limit_idx`).

### UTF-8 Offset Handling

UTF-8 byte offsets are mapped to Python character offsets using a zero-allocation sliding cursor (`FastCharCursor`) with SIMD-accelerated ASCII fast paths.

Byte-span calls through `lex_flat_byte_spans` bypass character offset calculation entirely.

## Ecosystem Coverage

Rsgments has been tested against the full Pygments 2.19.2 lexer ecosystem, successfully compiling all 422 compatible `RegexLexer` classes.

- **422/422 (100%)** compatible `RegexLexer` classes compile successfully with exact token-level parity.
- **314/422 (74.4%)** run entirely in Rust without Python fallback.
- **24,395/24,927 (97.9%)** regex rules are indexed by the native MultiDFA engine.

Unsupported lexer types, including `ExtendedRegexLexer` and custom Python-based lexers, safely fall back to the original Pygments implementation.

## Tests and Benchmarks

Install the development dependencies and run the test suite and benchmarks:

```powershell
python -m pip install -e ".[dev]"

python -m pytest -v

python bench.py

python bench_adversarial.py
```

### Standard PythonLexer Benchmark

The following results are trimmed means of 14 runs. The fastest and slowest runs were discarded, and the remaining 12 were averaged.

The benchmark processes approximately 5,995 lines and 81,750 tokens.

| Implementation | Average time | Speedup |
|---|---:|---:|
| Original Pygments | 653.34 ms | 1.00x |
| Rsgments (`get_tokens`) | 52.46 ms | 12.50x |
| Rsgments (`lex_raw_spans`) | 61.56 ms | 10.60x |
| Rsgments (`lex_flat_spans`) | 39.57 ms | 16.50x |

Compared with original Pygments:

- `get_tokens`: 92.0% less processing time.
- `lex_raw_spans`: 90.6% less processing time.
- `lex_flat_spans`: 93.9% less processing time.

### Adversarial and Fallback Overhead Benchmarks

`bench_adversarial.py` measures median execution times across 7 runs using four synthetic stress workloads and a fallback-rate sweep.

The workloads include a 24-rule lexer with 10 `PyOnly` rules intentionally placed at the beginning of the rule list.

| Workload | Tokens | Pygments | Rsgments (`get_tokens`) | Rsgments (`lex_flat_spans`) | Flat throughput | Key telemetry |
|---|---:|---:|---:|---:|---:|---|
| **1. Native-heavy** (15 rules) | 48,800 | 89.44 ms | 17.72 ms (5.05x) | **3.89 ms (22.99x)** | 12,538.9 k tok/s (29.99 MB/s) | 47,201 `HybridDFA` hits, 0 meta/Python fallbacks |
| **2. Fancy-heavy** (lookaround + backreference) | 92,400 | 168.58 ms | 39.75 ms (4.24x) | **18.04 ms (9.34x)** | 5,120.6 k tok/s (8.75 MB/s) | 99,601 `Fancy` calls (0.117 us average), 33.7% superset false positives |
| **3A. PyOnly-bottom** (10 `PyOnly` rules at the bottom) | 16,160 | 19.97 ms | 3.85 ms (5.19x) | **2.69 ms (7.42x)** | 6,004.1 k tok/s (17.27 MB/s) | Only 1,838 `py_regex_calls` due to `u_idx < limit_idx` skipping |
| **3B. PyOnly-top** (10 `PyOnly` rules at the top, 50% hit rate) | 16,000 | 47.73 ms | 30.64 ms (1.56x) | **29.42 ms (1.62x)** | 543.9 k tok/s (2.39 MB/s) | 139,203 `py_regex_calls` (0.220 us average fallback) |
| **4. Mixed adversarial** (Native + Fancy + `PyOnly` + `using` + UTF-8) | 50,000 | 113.05 ms | 48.90 ms (2.31x) | **44.13 ms (2.56x)** | 1,132.9 k tok/s (2.90 MB/s) | 42,001 `HybridDFA` / 2,000 `meta` calls (UTF-8), 1,000 `using()` callbacks |

#### Fallback Rate Sweep

**Workload:** `PyOnlyTopLexer`, with 10 `PyOnly` rules at the beginning of a 24-rule lexer and 16,000 tokens.

| Word-item fallback rate | Pygments | Rsgments (`get_tokens`) | Rsgments (`lex_flat_spans`) | `py_regex_calls` (hits) | Average fallback latency |
|---:|---:|---:|---:|---:|---:|
| ~10% | 58.75 ms | 33.38 ms (1.76x) | **32.10 ms (1.83x)** | 155,743 (800) | 0.213 us |
| ~50% | 47.19 ms | 30.66 ms (1.54x) | **29.54 ms (1.60x)** | 139,203 (4,000) | 0.219 us |
| ~90% | 36.64 ms | 27.99 ms (1.31x) | **26.62 ms (1.38x)** | 122,668 (7,200) | 0.221 us |
| ~99% | 33.36 ms | 27.04 ms (1.23x) | **26.15 ms (1.28x)** | 118,986 (7,920) | 0.223 us |

In these workloads, whitespace and newline separators between the 8,000 word items still match the DFA path. Consequently, approximately half of all 16,000 tokens are processed by the DFA.

Even with more than 118,000 to 155,000 Python regex FFI calls, average fallback latency remains stable at approximately 0.22 us.

With consecutive `PyOnly` tokens and no whitespace separators, approaching 100% total token fallback, throughput converges toward native Python `re` performance rather than suffering catastrophic degradation.

Benchmark results depend on the hardware, Python version, and workload. Run `python bench.py` and `python bench_adversarial.py` to measure performance in your own environment.

## Rsgments vs. rustgments

[Rsgments](https://github.com/memeviber/Rsgments) and [rustgments](https://pypi.org/project/rustgments/) are independent Rust-based projects that accelerate Pygments. Despite their similar names and goals, they use fundamentally different architectures.

> **Note:** `rustgments` is an independently developed third-party package published on PyPI. It is not an official Pygments package and is not maintained by the Pygments project.

### Architectural Differences

| Aspect | Rsgments | rustgments |
|---|---|---|
| Lexer table handling | Compiles live Pygments lexer definitions at runtime | Generates and packages processed lexer tables at build time |
| Lexer discovery | Uses the installed lexer's actual `_tokens` definitions | Uses a predefined set of generated lexer tables |
| Custom lexers | Can potentially support dynamically registered `RegexLexer` classes | Generally falls back to Pygments for unsupported or unbundled lexers |
| Pygments version handling | Uses runtime compilation rather than a single generated table set | Generated tables are tied to a specific Pygments release |
| Runtime compilation | Compiles lexer rules when needed and caches the result | Avoids compiling Pygments lexer tables at runtime |
| Python regex compatibility | Uses `transpile_python_regex` to translate Python-specific constructs (`\<`, `\>`, `\Z`, and `[\b]`) and escape literal `#` characters and whitespace inside character classes under `VERBOSE` mode | Relies on regex patterns processed during build-time table generation |
| Low-level output | Provides character-span and UTF-8 byte-span APIs | Primarily focuses on Pygments-compatible tokenization and highlighting |
| Main design priority | Runtime flexibility and editor integration | Reproducibility, verified coverage, and predictable compatibility |

### Why Rsgments Uses Runtime Compilation

Rsgments compiles the lexer definitions available in the running Python environment.

Instead of shipping a fixed snapshot of processed Pygments lexer tables, it reads the live `_tokens` definitions and translates supported rules into native Rust execution.

This approach provides several potential advantages:

- **Runtime flexibility:** Dynamically registered `RegexLexer` implementations can potentially be compiled without regenerating and shipping a new set of lexer tables.
- **Reduced dependence on generated snapshots:** Rsgments does not need to package a separate pre-generated table set for every supported Pygments release.
- **Custom environment support:** Applications that register their own lexers can use the same runtime compilation mechanism, provided their lexer rules are supported.
- **Editor-oriented APIs:** Flat span buffers and UTF-8 byte offsets allow editors and syntax-highlighting integrations to map tokens directly to document positions.
- **Incremental processing:** The byte-span API accepts a state stack and byte range, allowing callers to continue lexing from a previous state and process bounded regions of source text.

### Why rustgments Uses Build-Time Generation

`rustgments` takes a different approach. It processes Pygments lexer definitions during its build process and ships the resulting tables with the package.

This provides several practical benefits:

- Lexer tables are already processed before runtime.
- Each bundled lexer can be tested against Pygments, with native execution enabled only when its parity checks pass.
- There is no need to implement general runtime translation of Pygments lexer definitions.
- The supported native lexer set and compatibility conditions are explicitly documented.

The trade-off is that generated tables are tied to specific Pygments and Python Unicode versions. New lexer definitions or changes to Pygments may require regenerating the tables and publishing an updated release.

### Different Priorities, Different Trade-offs

Neither architecture is universally better. They optimize for different engineering requirements:

- **Rsgments** prioritizes runtime compilation, flexibility, and low-level interfaces for editor integrations.
- **rustgments** prioritizes a predefined, extensively verified set of native lexers and reproducible behavior across supported configurations.

Rsgments is particularly useful when an application needs to work with live Pygments lexer definitions, custom lexer registrations, or byte-oriented source-processing workflows.

Actual compatibility still depends on the lexer constructs supported by Rsgments. Unsupported patterns and callbacks must use the appropriate fallback path. Compatibility should be established through parity tests rather than assumed.

Although the two projects share a similar goal, their architectures address different engineering problems.

## License

Rsgments is licensed under the Apache License, Version 2.0.

See [LICENSE](https://github.com/memeviber/Rsgments/blob/main/LICENSE).
