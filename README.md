# Rsgments

Rsgments is a Rust-accelerated state machine engine for [Pygments](https://pygments.org/), built with PyO3 and maturin.

It compiles Pygments `RegexLexer` rules into native Rust state machines and regex engines where possible, while seamlessly falling back to Python for unsupported patterns and callbacks. The package provides standard Pygments token streams, character-span buffers, and UTF-8 byte-span APIs designed for code editors.

## Installation

Install prebuilt wheels directly from PyPI:

```bash
pip install rsgments
```

Prebuilt wheels are provided for Python 3.9 through 3.14 (including free-threaded `3.13t` and `3.14t` builds) across Linux (`manylinux` and `musllinux`), macOS, and Windows.

To upgrade to the latest release:

```bash
pip install --upgrade rsgments
```

## Requirements

For source builds and local development:

- Python 3.9 or newer (including free-threaded CPython 3.13t/3.14t)
- Rust toolchain
- `maturin` 1.9.4 or newer, below 2.0
- `Pygments` 2.14 or newer

## Build from source

Install the development dependencies and build in editable mode:

```powershell
python -m pip install maturin
python -m pip install -e ".[dev]"
```

Or build a release wheel directly:

```powershell
maturin build --release
```

The resulting wheel is written to `target/wheels/`.

## Usage

### Global or scoped patching

Patch Pygments `RegexLexer` globally:

```python
import rsgments

rsgments.patch_pygments()
```

Or scope the patch with a context manager:

```python
with rsgments.patched():
    tokens = list(lexer.get_tokens_unprocessed(source))
```

Use `rsgments.unpatch_pygments()` to restore the original Pygments implementation. Active in-flight lexing calls (including nested `using(...)` sub-lexers) drain consistently before the original method is restored, while compiled lexer caches remain intact across patch toggles.

### Direct editor APIs

Direct APIs can be called independently of the global patch state:

```python
import rsgments
from pygments.lexers import PythonLexer

lexer = PythonLexer()
buf, token_table, final_stack = rsgments.lex_flat_spans(lexer, source)
```

`lex_flat_spans` returns a flat `bytes` buffer of little-endian `u32` triples:

`(character_start, character_length, token_id)`

Read it without per-token Python object allocation using:

```python
mv = memoryview(buf).cast("I")
```

For byte-oriented editors (such as QScintilla), use `lex_flat_byte_spans` to tokenize only a specific UTF-8 byte window `[start_byte, end_byte)`:

```python
buf, token_table, final_stack = rsgments.lex_flat_byte_spans(
    lexer,
    source,
    stack=("root",),
    start_byte=0,
    end_byte=256,
)
```

This returns little-endian `u32` triples of `(byte_start, byte_length, token_id)`, clipped to the requested range, and stops lexing as soon as `byte_pos >= end_byte`. Pass `final_stack` back in when lexing subsequent blocks.

`rsgments.is_supported(lexer)` checks whether a lexer is supported by the direct Rsgments APIs. `ExtendedRegexLexer` subclasses (which rely on `LexerContext`) are intentionally left to Pygments' native implementation.

### Profiling API

To inspect engine routing, superset false-positive rates, and exact Python fallback timing:

```python
buf, token_table, final_stack, profile = rsgments.lex_flat_spans_profiled(lexer, source)
print(
    profile.dfa_searches,
    profile.fancy_verify_calls,
    profile.superset_false_positives,
    profile.py_regex_calls,
    profile.py_fallback_ns,
)
```

Profiling uses compile-time monomorphization (`const PROFILE: bool`) in Rust so normal `lex_*` calls incur zero counter or timer overhead.

## Architecture

Rsgments compiles live Pygments `_tokens` state tables into an immutable `#[pyclass(frozen)] CompiledLexer` using a three-tier execution model:

1. **Tier 1 — Multi-Pattern DFA (`regex-automata` `HybridDFA` + `meta::Regex`):**
   Compatible rules in each state are compiled into a single anchored multi-pattern DFA (`MatchKind::LeftmostFirst`). `HybridDFA` handles ASCII fast paths directly; if a specific token encounters non-ASCII boundaries that quit the lazy DFA, only that token falls back to `meta::Regex` before resuming `HybridDFA`.
2. **Tier 2 — Lookaround Superset Prefiltering + `fancy-regex` + Suffix DFAs:**
   Rules containing lookaheads or lookbehinds (`(?=...)`, `(?!...)`, `(?<=...)`, `(?<!...)`) are stripped of their lookarounds into valid superset regexes and indexed inside the state's Multi-DFA. `fancy-regex` (`\G`-anchored) is invoked only when a superset matches; if verification fails, execution jumps to a prebuilt suffix DFA starting at `win_idx + 1`. Backreferences remain unindexed and run via `fancy-regex` only when no higher-priority DFA rule matches.
3. **Tier 3 — Python Fallback (`PyOnly` / `PyCallback`):**
   Patterns using Python-specific constructs (such as inline `re.ASCII` `(?a)` with `\w`/`\d`/`\s`, `re.LOCALE`, or unsupported syntax) and Python callbacks (`using(...)`, custom tokenizers) call back into Python via PyO3. Unindexed `PyOnly` rules placed before the first DFA rule are checked prior to invoking the DFA to avoid wasted DFA searches when they match frequently; rules placed after DFA rules are skipped whenever a higher-priority DFA match wins (`u_idx >= limit_idx`).

UTF-8 byte offsets are mapped to Python character offsets via a zero-allocation sliding cursor (`FastCharCursor`) with SIMD ASCII fast paths, and byte-span calls (`lex_flat_byte_spans`) bypass character offset calculation entirely.

## Tests and benchmark

Install the development dependencies and run the test suite and benchmarks:

```powershell
python -m pip install -e ".[dev]"
python -m pytest -v
python bench.py
python bench_adversarial.py
```

### Standard PythonLexer benchmark

Results below are trimmed means of 14 runs (fastest and slowest discarded, remaining 12 averaged) on approximately 5,995 lines and 81,750 tokens:

| Implementation | Average time | Speedup |
|---|---:|---:|
| Original Pygments | 570.58 ms | 1.00× |
| Rsgments (`get_tokens`) | 74.95 ms | 7.61× |
| Rsgments (`lex_raw_spans`) | 55.95 ms | 10.20× |
| Rsgments (`lex_flat_spans`) | 49.80 ms | 11.46× |

Compared with original Pygments:

- `get_tokens`: 86.9% less processing time
- `lex_raw_spans`: 90.2% less processing time
- `lex_flat_spans`: 91.3% less processing time

### Adversarial and fallback overhead benchmarks

To verify behavior outside favorable Python workloads, `bench_adversarial.py` measures median times across 7 runs for four synthetic stress workloads and a fallback-rate sweep (including a 24-rule lexer with 10 `PyOnly` rules intentionally placed at the top of the rule list):

| Workload | Tokens | Pygments | Rsgments (`get_tokens`) | Rsgments (`lex_flat_spans`) | Throughput (`flat`) | Key telemetry |
|---|---:|---:|---:|---:|---:|---|
| **1. Native-heavy** (15 rules) | 48,800 | 140.76 ms | 24.82 ms (**5.67×**) | **5.78 ms (24.37×)** | 8,447.7 k tok/s (20.21 MB/s) | 47,201 `HybridDFA` hits, 0 meta/Py fallbacks |
| **2. Fancy-heavy** (lookaround + backref) | 92,400 | 256.56 ms | 57.32 ms (**4.48×**) | **28.24 ms (9.08×)** | 3,271.9 k tok/s (5.59 MB/s) | 99,601 `Fancy` calls (`0.197 µs` avg), 33.7% superset FP |
| **3A. PyOnly-bottom** (10 `PyOnly` at bottom) | 16,160 | 29.29 ms | 6.22 ms (**4.71×**) | **4.43 ms (6.62×)** | 3,651.0 k tok/s (10.50 MB/s) | Only 1,838 `py_regex_calls` (`u_idx < limit_idx` skipping) |
| **3B. PyOnly-top** (10 `PyOnly` at top, 50% hit) | 16,000 | 65.82 ms | 34.41 ms (**1.91×**) | **29.33 ms (2.24×)** | 545.5 k tok/s (2.39 MB/s) | 139,203 `py_regex_calls` (`0.214 µs` avg fallback) |
| **4. Mixed adversarial** (Native + Fancy + `PyOnly` + `using` + UTF-8) | 50,000 | 111.90 ms | 54.37 ms (**2.06×**) | **46.53 ms (2.40×)** | 1,074.5 k tok/s (2.75 MB/s) | 42,001 `HybridDFA` / 2,000 `meta` (UTF-8), 1,000 `using()` callbacks |

#### Fallback sweep (`PyOnlyTopLexer`: 10 `PyOnly` rules at the top of 24 rules, 16,000 tokens)

| Word item fallback rate | Pygments | Rsgments (`get_tokens`) | Rsgments (`lex_flat_spans`) | `py_regex_calls` (`hits`) | Avg fallback latency |
|---:|---:|---:|---:|---:|---:|
| ~10% | 55.07 ms | 29.69 ms (**1.86×**) | **28.61 ms (1.92×)** | 155,743 (800) | 0.197 µs |
| ~50% | 44.63 ms | 27.04 ms (**1.65×**) | **25.94 ms (1.72×)** | 139,203 (4,000) | 0.198 µs |
| ~90% | 33.66 ms | 24.61 ms (**1.37×**) | **23.71 ms (1.42×)** | 122,668 (7,200) | 0.202 µs |
| ~99% | 31.60 ms | 24.29 ms (**1.30×**) | **23.03 ms (1.37×)** | 118,986 (7,920) | 0.203 µs |

In the sweep inputs, whitespace/newline separators between the 8,000 word items still match the DFA path (so roughly half of all 16,000 tokens hit DFA). Even with >118,000–155,000 Python regex FFI calls, per-fallback latency stays flat at **~0.20 µs**. With consecutive `PyOnly` tokens without whitespace (approaching 100% total token fallback), throughput converges near Python `re` speed rather than suffering catastrophic degradation.

Results are specific to the benchmark machine and workload. Run `python bench.py` and `python bench_adversarial.py` to measure performance in your own environment.

## Rsgments vs. rustgments

[Rsgments](https://github.com/memeviber/Rsgments) and [rustgments](https://pypi.org/project/rustgments/) are independent projects that use Rust to accelerate Pygments. Despite their similar names and goals, they take fundamentally different approaches.

**Note:** `rustgments` is an independently developed third-party package published on PyPI. It is not an official Pygments package or an implementation maintained by the Pygments project.

### Architectural differences

| Aspect | Rsgments | rustgments |
|---|---|---|
| Lexer table handling | Compiles live Pygments lexer definitions at runtime | Generates and packages processed lexer tables at build time |
| Lexer discovery | Uses the installed lexer's actual `_tokens` definitions | Uses a predefined set of generated lexer tables |
| Custom lexers | Can potentially support dynamically registered `RegexLexer` classes | Generally falls back to Pygments for unsupported or unbundled lexers |
| Pygments version handling | Designed around runtime compilation rather than a single generated table set | Generated tables are tied to a specific Pygments release |
| Runtime compilation | Compiles lexer rules when needed and caches the result | Avoids compiling Pygments lexer tables at runtime |
| Python regex compatibility | Uses `transpile_python_regex` to translate Python-specific constructs (`\<`, `\>`, `\Z`, `[\b]`) and escape literal `#` and whitespace inside `[...]` character classes under `VERBOSE` mode before compilation | Relies on regex patterns processed during build-time table generation |
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

Rsgments is licensed under the Apache License, Version 2.0. See [LICENSE](https://github.com/memeviber/Rsgments/blob/main/LICENSE).
