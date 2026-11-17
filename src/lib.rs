use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use fancy_regex::{Regex as FancyRegex, RegexBuilder as FancyBuilder};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyString, PyTuple};
use regex_automata::{
    hybrid::dfa::{Cache as HybridCache, DFA as HybridDFA},
    meta::{Cache as MetaCache, Regex as RaRegex},
    util::{captures::Captures, syntax},
    Anchored, Input,
};

const SKIP_TOKEN: u32 = u32::MAX;
const MAX_SUFFIX_DFAS_PER_STATE: usize = 12;
const HYBRID_CACHE_BYTES: usize = 16 * 1024 * 1024;

struct PyLockGuard<'a, 'py> {
    lock: &'a Bound<'py, PyAny>,
}

impl<'a, 'py> PyLockGuard<'a, 'py> {
    fn acquire(lock: &'a Bound<'py, PyAny>) -> PyResult<Self> {
        lock.call_method0("acquire")?;
        Ok(Self { lock })
    }
}

impl Drop for PyLockGuard<'_, '_> {
    fn drop(&mut self) {
        let _ = self.lock.call_method0("release");
    }
}

enum Engine {
    Automata(RaRegex),
    Fancy(FancyRegex),
    PyOnly,
}

enum ActionKind {
    None,
    Token(u32),
    ByGroups(Vec<u32>),
    PyCallback(Py<PyAny>),
}

#[derive(Clone)]
enum StateOp {
    Push(usize),
    PushCurrent,
    Pop(usize),
}

struct CompiledRule {
    engine: Engine,
    dfa_pattern: Option<String>,
    needs_fancy_verify: bool,
    action: ActionKind,
    ops: Vec<StateOp>,
    py_rexmatch: Option<Py<PyAny>>,
}

struct MultiDfaEntry {
    start_idx: usize,
    hybrid: Option<HybridDFA>,
    meta: RaRegex,
    pat_to_rule: Vec<usize>,
}
struct CompiledState {
    rules: Vec<CompiledRule>,
    unindexed_rules: Vec<usize>,
    first_dfa_rule_idx: usize,
    dfas: Vec<MultiDfaEntry>,
}

impl CompiledState {
    fn new() -> Self {
        Self {
            rules: Vec::new(),
            unindexed_rules: Vec::new(),
            first_dfa_rule_idx: 0,
            dfas: Vec::new(),
        }
    }

    fn finalize(&mut self) {
        self.unindexed_rules.clear();
        self.dfas.clear();
        self.first_dfa_rule_idx = self
            .rules
            .iter()
            .position(|r| r.dfa_pattern.is_some())
            .unwrap_or(self.rules.len());

        let mut fancy_indices = Vec::new();
        for (idx, r) in self.rules.iter().enumerate() {
            if r.dfa_pattern.is_none() {
                self.unindexed_rules.push(idx);
            } else if r.needs_fancy_verify {
                fancy_indices.push(idx);
            }
        }

        self.build_dfa_from(0);

        for &f_idx in fancy_indices.iter().take(MAX_SUFFIX_DFAS_PER_STATE) {
            let next_idx = f_idx + 1;
            if next_idx < self.rules.len() {
                self.build_dfa_from(next_idx);
            }
        }
    }

    fn build_dfa_from(&mut self, start_idx: usize) {
        let mut patterns = Vec::new();
        let mut map = Vec::new();
        for (idx, r) in self.rules.iter().enumerate().skip(start_idx) {
            if let Some(pat) = &r.dfa_pattern {
                patterns.push(pat.as_str());
                map.push(idx);
            }
        }
        if patterns.is_empty() {
            return;
        }

        if let Ok(meta) = RaRegex::builder().build_many(&patterns) {
            let mut hb = HybridDFA::builder();
            hb.configure(
                HybridDFA::config()
                    .unicode_word_boundary(true)
                    .cache_capacity(HYBRID_CACHE_BYTES),
            );
            let hybrid = hb.build_many(&patterns).ok();

            self.dfas.push(MultiDfaEntry {
                start_idx,
                hybrid,
                meta,
                pat_to_rule: map,
            });
        }
    }

    #[inline]
    fn dfa_index_starting_at(&self, min_idx: usize) -> Option<usize> {
        for (i, entry) in self.dfas.iter().enumerate() {
            if entry.start_idx == min_idx {
                return Some(i);
            }
        }
        None
    }
}

#[derive(Default, Clone, Copy)]
struct ProfileCounters {
    tokens_emitted: u64,
    dfa_searches: u64,
    dfa_hybrid_hits: u64,
    dfa_meta_fallbacks: u64,
    fancy_verify_calls: u64,
    superset_false_positives: u64,
    py_regex_calls: u64,
    py_regex_hits: u64,
    py_callback_calls: u64,
    fancy_ns: u64,
    py_fallback_ns: u64,
}

#[pyclass(frozen)]
pub struct LexProfile {
    #[pyo3(get)]
    pub tokens_emitted: u64,
    #[pyo3(get)]
    pub dfa_searches: u64,
    #[pyo3(get)]
    pub dfa_hybrid_hits: u64,
    #[pyo3(get)]
    pub dfa_meta_fallbacks: u64,
    #[pyo3(get)]
    pub fancy_verify_calls: u64,
    #[pyo3(get)]
    pub superset_false_positives: u64,
    #[pyo3(get)]
    pub py_regex_calls: u64,
    #[pyo3(get)]
    pub py_regex_hits: u64,
    #[pyo3(get)]
    pub py_callback_calls: u64,
    #[pyo3(get)]
    pub fancy_ns: u64,
    #[pyo3(get)]
    pub py_fallback_ns: u64,
}

impl From<ProfileCounters> for LexProfile {
    fn from(c: ProfileCounters) -> Self {
        Self {
            tokens_emitted: c.tokens_emitted,
            dfa_searches: c.dfa_searches,
            dfa_hybrid_hits: c.dfa_hybrid_hits,
            dfa_meta_fallbacks: c.dfa_meta_fallbacks,
            fancy_verify_calls: c.fancy_verify_calls,
            superset_false_positives: c.superset_false_positives,
            py_regex_calls: c.py_regex_calls,
            py_regex_hits: c.py_regex_hits,
            py_callback_calls: c.py_callback_calls,
            fancy_ns: c.fancy_ns,
            py_fallback_ns: c.py_fallback_ns,
        }
    }
}
struct LexerScratch {
    hybrid_caches: Vec<Vec<Option<HybridCache>>>,
    meta_caches: Vec<Vec<Option<MetaCache>>>,
    rule_meta_caches: Vec<Vec<Option<MetaCache>>>,
    rule_caps: Vec<Vec<Option<Captures>>>,
}

struct SpanRec {
    byte_start: usize,
    byte_end: usize,
    char_start: usize,
    char_len: usize,
    tid: u32,
}

struct FastCharCursor<'a> {
    text: &'a str,
    is_ascii: bool,
    last_byte: usize,
    last_char: usize,
    total_chars: Option<usize>,
    min_emit_byte: usize,
    max_byte_end: Option<usize>,
    need_char_offsets: bool,
}

impl<'a> FastCharCursor<'a> {
    fn new(
        text: &'a str,
        min_emit_byte: usize,
        max_byte_end: Option<usize>,
        need_char_offsets: bool,
    ) -> Self {
        let is_ascii = text.is_ascii();
        Self {
            text,
            is_ascii,
            last_byte: 0,
            last_char: 0,
            total_chars: if is_ascii { Some(text.len()) } else { None },
            min_emit_byte,
            max_byte_end,
            need_char_offsets,
        }
    }

    #[inline(always)]
    fn to_char(&mut self, byte_idx: usize) -> usize {
        if self.is_ascii || byte_idx == 0 {
            return byte_idx;
        }
        if byte_idx == self.last_byte {
            return self.last_char;
        }
        if byte_idx > self.last_byte {
            let slice = &self.text[self.last_byte..byte_idx];
            let delta = if slice.is_ascii() {
                slice.len()
            } else {
                slice.chars().count()
            };
            self.last_byte = byte_idx;
            self.last_char += delta;
            self.last_char
        } else {
            let slice = &self.text[byte_idx..self.last_byte];
            let delta = if slice.is_ascii() {
                slice.len()
            } else {
                slice.chars().count()
            };
            self.last_byte = byte_idx;
            self.last_char -= delta;
            self.last_char
        }
    }

    fn to_byte(&mut self, char_idx: usize) -> usize {
        if self.is_ascii {
            return char_idx;
        }
        if char_idx == self.last_char {
            return self.last_byte;
        }
        if char_idx > self.last_char {
            let diff = char_idx - self.last_char;
            let rest = &self.text[self.last_byte..];
            let adv = if rest.len() >= diff && rest.as_bytes()[..diff].is_ascii() {
                diff
            } else {
                rest.char_indices().nth(diff).map_or(rest.len(), |(b, _)| b)
            };
            self.last_byte += adv;
            self.last_char = char_idx;
            self.last_byte
        } else {
            let diff = self.last_char - char_idx;
            let prefix = &self.text[..self.last_byte];
            let pos = if prefix.is_ascii() {
                char_idx
            } else {
                prefix
                    .char_indices()
                    .rev()
                    .nth(diff - 1)
                    .map_or(0, |(b, _)| b)
            };
            self.last_byte = pos;
            self.last_char = char_idx;
            self.last_byte
        }
    }

    fn total_char_len(&mut self) -> usize {
        if let Some(n) = self.total_chars {
            return n;
        }
        let total = self.to_char(self.text.len());
        self.total_chars = Some(total);
        total
    }
}

fn wrap_inline_flags(pat: &str, i: bool, m: bool, s: bool, x: bool) -> String {
    let mut pos = String::new();
    let mut neg = String::new();
    if i {
        pos.push('i');
    } else {
        neg.push('i');
    }
    if m {
        pos.push('m');
    } else {
        neg.push('m');
    }
    if s {
        pos.push('s');
    } else {
        neg.push('s');
    }
    if x {
        format!("(?{pos}x-{neg}:\n{pat}\n)")
    } else {
        neg.push('x');
        format!("(?{pos}-{neg}:{pat})")
    }
}

fn strip_lookarounds_to_superset(pat: &str, verbose: bool) -> Option<String> {
    let bytes = pat.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0usize;
    let mut in_class = false;
    let mut class_start = false;

    while i < len {
        let b = bytes[i];
        if b == b'\\' {
            if i + 1 >= len {
                return None;
            }
            let next = bytes[i + 1];
            if !in_class && ((next >= b'1' && next <= b'9') || next == b'g') {
                return None;
            }
            let ch_len = pat[i + 1..].chars().next()?.len_utf8();
            out.push_str(&pat[i..i + 1 + ch_len]);
            i += 1 + ch_len;
            class_start = false;
            continue;
        }

        if in_class {
            if b == b'[' {
                out.push('[');
                i += 1;
                class_start = false;
                continue;
            }
            if b == b'^' && class_start {
                out.push('^');
                i += 1;
                continue;
            }
            if b == b']' && !class_start {
                in_class = false;
            }
            class_start = false;
            let ch = pat[i..].chars().next()?;
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }

        if verbose && b == b'#' {
            while i < len && bytes[i] != b'\n' {
                let ch = pat[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
            continue;
        }

        if b == b'[' {
            in_class = true;
            class_start = true;
            out.push('[');
            i += 1;
            continue;
        }

        if b == b'(' && i + 1 < len && bytes[i + 1] == b'?' {
            if i + 2 < len && (bytes[i + 2] == b'#' || bytes[i + 2] == b'(') {
                return None;
            }

            let look_len = if pat[i..].starts_with("(?=") || pat[i..].starts_with("(?!") {
                3
            } else if pat[i..].starts_with("(?<=") || pat[i..].starts_with("(?<!") {
                4
            } else {
                0
            };

            if look_len > 0 {
                i += look_len;
                let mut depth = 1usize;
                let mut skip_class = false;
                let mut skip_class_start = false;

                while i < len && depth > 0 {
                    let sb = bytes[i];
                    if sb == b'\\' {
                        if i + 1 >= len {
                            return None;
                        }
                        if !skip_class
                            && ((bytes[i + 1] >= b'1' && bytes[i + 1] <= b'9')
                                || bytes[i + 1] == b'g')
                        {
                            return None;
                        }
                        let ch_len = pat[i + 1..].chars().next()?.len_utf8();
                        i += 1 + ch_len;
                        skip_class_start = false;
                        continue;
                    }
                    if skip_class {
                        if sb == b'^' && skip_class_start {
                            i += 1;
                            continue;
                        }
                        if sb == b']' && !skip_class_start {
                            skip_class = false;
                        }
                        skip_class_start = false;
                        i += pat[i..].chars().next()?.len_utf8();
                        continue;
                    }
                    if verbose && sb == b'#' {
                        while i < len && bytes[i] != b'\n' {
                            i += pat[i..].chars().next()?.len_utf8();
                        }
                        continue;
                    }
                    if sb == b'[' {
                        skip_class = true;
                        skip_class_start = true;
                        i += 1;
                        continue;
                    }
                    if sb == b'(' {
                        depth += 1;
                    } else if sb == b')' {
                        depth -= 1;
                    }
                    i += pat[i..].chars().next()?.len_utf8();
                }
                if depth != 0 {
                    return None;
                }
                out.push_str("(?:)");
                continue;
            }

            let mut j = i + 2;
            while j < len && (bytes[j].is_ascii_alphabetic() || bytes[j] == b'-') {
                if bytes[j] == b'x' {
                    return None;
                }
                j += 1;
            }
        }

        let ch = pat[i..].chars().next()?;
        out.push(ch);
        i += ch.len_utf8();
    }

    Some(out)
}

/// Translate Python `re`-specific syntax conventions into `regex-syntax` / `fancy-regex` syntax:
/// - Outside `[...]`: `\<` -> `<`, `\>` -> `>`, `\Z` -> `\z`
/// - Inside `[...]`: `\b` -> `\x08`, `\<` -> `<`, `\>` -> `>`
/// - Inside `[...]` when `(?x)` (`verbose`) is enabled: escape `#` as `\#` and escape whitespace
///   (because Rust's `regex-syntax` handles comments and whitespace inside `[...]` as well, unlike Python's `re`).
/// Returns `(translated_pattern, ends_with_verbose)`.
fn transpile_python_regex(pat: &str, initial_verbose: bool) -> (String, bool) {
    let bytes = pat.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + 16);
    let mut i = 0usize;
    let mut in_class = false;
    let mut class_start = false;
    let mut x_stack: Vec<bool> = vec![initial_verbose];

    while i < len {
        let x_active = *x_stack.last().unwrap_or(&initial_verbose);
        let b = bytes[i];

        if b == b'\\' {
            if i + 1 >= len {
                out.push('\\');
                break;
            }
            let next = bytes[i + 1];
            if in_class {
                class_start = false;
                match next {
                    b'b' => {
                        out.push_str("\\x08");
                        i += 2;
                        continue;
                    }
                    b'<' => {
                        out.push('<');
                        i += 2;
                        continue;
                    }
                    b'>' => {
                        out.push('>');
                        i += 2;
                        continue;
                    }
                    _ => {}
                }
            } else {
                match next {
                    b'<' => {
                        out.push('<');
                        i += 2;
                        continue;
                    }
                    b'>' => {
                        out.push('>');
                        i += 2;
                        continue;
                    }
                    b'Z' => {
                        out.push_str("\\z");
                        i += 2;
                        continue;
                    }
                    _ => {}
                }
            }

            let Some(ch) = pat[i + 1..].chars().next() else {
                out.push('\\');
                break;
            };
            out.push('\\');
            out.push(ch);
            i += 1 + ch.len_utf8();
            continue;
        }

        if in_class {
            if b == b'[' {
                out.push('[');
                i += 1;
                class_start = false;
                continue;
            }
            if b == b'^' && class_start {
                out.push('^');
                i += 1;
                continue;
            }
            if b == b']' && !class_start {
                in_class = false;
                out.push(']');
                i += 1;
                continue;
            }
            class_start = false;

            if x_active {
                match b {
                    b'#' => {
                        out.push_str("\\#");
                        i += 1;
                        continue;
                    }
                    b' ' => {
                        out.push_str("\\x20");
                        i += 1;
                        continue;
                    }
                    b'\t' => {
                        out.push_str("\\t");
                        i += 1;
                        continue;
                    }
                    b'\n' => {
                        out.push_str("\\n");
                        i += 1;
                        continue;
                    }
                    b'\r' => {
                        out.push_str("\\r");
                        i += 1;
                        continue;
                    }
                    0x0B => {
                        out.push_str("\\x0b");
                        i += 1;
                        continue;
                    }
                    0x0C => {
                        out.push_str("\\f");
                        i += 1;
                        continue;
                    }
                    _ => {}
                }
            }

            if let Some(ch) = pat[i..].chars().next() {
                out.push(ch);
                i += ch.len_utf8();
            } else {
                break;
            }
            continue;
        }

        // Outside character classes (`[...]`)
        if x_active && b == b'#' {
            while i < len && bytes[i] != b'\n' {
                if let Some(ch) = pat[i..].chars().next() {
                    out.push(ch);
                    i += ch.len_utf8();
                } else {
                    break;
                }
            }
            continue;
        }

        if b == b'[' {
            in_class = true;
            class_start = true;
            out.push('[');
            i += 1;
            continue;
        }

        if b == b'(' {
            if i + 1 < len && bytes[i + 1] == b'?' {
                // Check (?#comment)
                if i + 2 < len && bytes[i + 2] == b'#' {
                    out.push_str("(?#");
                    i += 3;
                    while i < len {
                        if bytes[i] == b'\\' && i + 1 < len {
                            if let Some(ch) = pat[i + 1..].chars().next() {
                                out.push('\\');
                                out.push(ch);
                                i += 1 + ch.len_utf8();
                                continue;
                            }
                        }
                        if bytes[i] == b')' {
                            out.push(')');
                            i += 1;
                            break;
                        }
                        if let Some(ch) = pat[i..].chars().next() {
                            out.push(ch);
                            i += ch.len_utf8();
                        } else {
                            break;
                        }
                    }
                    continue;
                }

                // Check (?flags) or (?flags:...)
                let mut j = i + 2;
                let mut next_x = x_active;
                let mut neg = false;
                let mut valid_flags = false;
                while j < len {
                    let fb = bytes[j];
                    if fb == b'-' && !neg {
                        neg = true;
                        j += 1;
                        continue;
                    }
                    if matches!(fb, b'a' | b'i' | b'L' | b'm' | b's' | b'u' | b'x') {
                        valid_flags = true;
                        if fb == b'x' {
                            next_x = !neg;
                        }
                        j += 1;
                        continue;
                    }
                    break;
                }
                if valid_flags && j < len && (bytes[j] == b')' || bytes[j] == b':') {
                    if bytes[j] == b')' {
                        if let Some(top) = x_stack.last_mut() {
                            *top = next_x;
                        }
                        out.push_str(&pat[i..=j]);
                        i = j + 1;
                        continue;
                    } else {
                        x_stack.push(next_x);
                        out.push_str(&pat[i..=j]);
                        i = j + 1;
                        continue;
                    }
                }
            }
            x_stack.push(x_active);
            out.push('(');
            i += 1;
            continue;
        }

        if b == b')' {
            if x_stack.len() > 1 {
                x_stack.pop();
            }
            out.push(')');
            i += 1;
            continue;
        }

        if let Some(ch) = pat[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }

    let ends_verbose = *x_stack.last().unwrap_or(&initial_verbose);
    (out, ends_verbose)
}

fn compile_engine(
    pattern: &str,
    ignore_case: bool,
    multi_line: bool,
    dot_all: bool,
    verbose: bool,
    ascii: bool,
    locale: bool,
) -> (Engine, Option<String>, bool) {
    if locale {
        return (Engine::PyOnly, None, false);
    }

    if ascii
        && (pattern.contains("\\w")
            || pattern.contains("\\W")
            || pattern.contains("\\d")
            || pattern.contains("\\D")
            || pattern.contains("\\s")
            || pattern.contains("\\S")
            || pattern.contains("\\b")
            || pattern.contains("\\B"))
    {
        return (Engine::PyOnly, None, false);
    }

    let (transpiled, ends_verbose) = transpile_python_regex(pattern, verbose);
    let pat_str = transpiled.as_str();

    let syn_cfg = syntax::Config::new()
        .case_insensitive(ignore_case)
        .multi_line(multi_line)
        .dot_matches_new_line(dot_all)
        .ignore_whitespace(verbose)
        .unicode(true);

    if let Ok(re) = RaRegex::builder().syntax(syn_cfg).build(pat_str) {
        let wrapped = wrap_inline_flags(pat_str, ignore_case, multi_line, dot_all, verbose);
        let dfa_pat = if RaRegex::new(&wrapped).is_ok() {
            Some(wrapped)
        } else {
            None
        };
        return (Engine::Automata(re), dfa_pat, false);
    }

    // Note: Do not use a raw string `r"..."` so that `\n` becomes an actual newline for the trailing comment in `x` mode!
    let anchored_pat = if verbose || ends_verbose {
        format!("\\G(?:\n{pat_str}\n)")
    } else {
        format!("\\G(?:{pat_str})")
    };

    let mut fb = FancyBuilder::new(&anchored_pat);
    fb.case_insensitive(ignore_case)
        .multi_line(multi_line)
        .dot_matches_new_line(dot_all)
        .ignore_whitespace(verbose)
        .unicode_mode(true)
        .backtrack_limit(1_000_000);

    match fb.build() {
        Ok(re) => {
            let mut superset_pat = None;
            if let Some(stripped) = strip_lookarounds_to_superset(pat_str, verbose) {
                let wrapped =
                    wrap_inline_flags(&stripped, ignore_case, multi_line, dot_all, verbose);
                if let Ok(sup_re) = RaRegex::new(&wrapped) {
                    if sup_re
                        .find(Input::new("").anchored(Anchored::Yes))
                        .is_none()
                    {
                        superset_pat = Some(wrapped);
                    }
                }
            }
            let needs_verify = superset_pat.is_some();
            (Engine::Fancy(re), superset_pat, needs_verify)
        }
        Err(_) => (Engine::PyOnly, None, false),
    }
}

#[pyclass]
pub struct CompiledLexerBuilder {
    states: Vec<CompiledState>,
    state_names: HashMap<String, usize>,
    state_id_to_name: Vec<String>,
    root_state_id: usize,
    eol_token_id: u32,
    error_token_id: u32,
    has_py_fallback: bool,
    id_to_token: Py<PyList>,
    token_to_id: Py<PyDict>,
    reg_lock: Py<PyAny>,
    built: bool,
}

#[pymethods]
impl CompiledLexerBuilder {
    #[new]
    fn new(
        state_names_list: Vec<String>,
        root_state: String,
        eol_token_id: u32,
        error_token_id: u32,
        id_to_token: Py<PyList>,
        token_to_id: Py<PyDict>,
        reg_lock: Py<PyAny>,
    ) -> PyResult<Self> {
        let mut state_names = HashMap::with_capacity(state_names_list.len());
        let mut states = Vec::with_capacity(state_names_list.len());
        for (idx, name) in state_names_list.iter().enumerate() {
            state_names.insert(name.clone(), idx);
            states.push(CompiledState::new());
        }
        let root_state_id = *state_names.get(&root_state).ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err("root state not found in state_names_list")
        })?;
        Ok(Self {
            states,
            state_names,
            state_id_to_name: state_names_list,
            root_state_id,
            eol_token_id,
            error_token_id,
            has_py_fallback: false,
            id_to_token,
            token_to_id,
            reg_lock,
            built: false,
        })
    }

    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (state_name, pattern, ignore_case, multi_line, dot_all, verbose, ascii, locale, action_kind, token_id, group_tokens, ops, py_rexmatch=None, py_action=None))]
    fn add_rule(
        &mut self,
        state_name: &str,
        pattern: Option<&str>,
        ignore_case: bool,
        multi_line: bool,
        dot_all: bool,
        verbose: bool,
        ascii: bool,
        locale: bool,
        action_kind: &str,
        token_id: u32,
        group_tokens: Vec<u32>,
        ops: Vec<(String, i32)>,
        py_rexmatch: Option<Py<PyAny>>,
        py_action: Option<Py<PyAny>>,
    ) -> PyResult<()> {
        if self.built {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "CompiledLexerBuilder already built",
            ));
        }

        let state_id = *self.state_names.get(state_name).ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err(format!("unknown state: {state_name}"))
        })?;

        let (mut engine, mut dfa_pattern, mut needs_fancy_verify) = match pattern {
            Some(pat) => compile_engine(
                pat,
                ignore_case,
                multi_line,
                dot_all,
                verbose,
                ascii,
                locale,
            ),
            None => (Engine::PyOnly, None, false),
        };

        let action = match action_kind {
            "none" => ActionKind::None,
            "token" => ActionKind::Token(token_id),
            "bygroups" => {
                if matches!(engine, Engine::PyOnly) {
                    if let Some(cb) = py_action {
                        self.has_py_fallback = true;
                        ActionKind::PyCallback(cb)
                    } else {
                        return Err(pyo3::exceptions::PyValueError::new_err(
                            "py_action required when PyOnly engine is used with bygroups",
                        ));
                    }
                } else {
                    ActionKind::ByGroups(group_tokens)
                }
            }
            "py" => {
                self.has_py_fallback = true;
                let cb = py_action.ok_or_else(|| {
                    pyo3::exceptions::PyValueError::new_err("py_action required for 'py'")
                })?;
                ActionKind::PyCallback(cb)
            }
            other => {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "unknown action_kind: {other}"
                )))
            }
        };

        if matches!(engine, Engine::PyOnly) {
            if py_rexmatch.is_none() {
                return Err(pyo3::exceptions::PyValueError::new_err(
                    "py_rexmatch required for PyOnly fallback",
                ));
            }
            self.has_py_fallback = true;
        } else if matches!(action, ActionKind::PyCallback(_)) && py_rexmatch.is_none() {
            engine = Engine::PyOnly;
            dfa_pattern = None;
            needs_fancy_verify = false;
        }

        let mut compiled_ops = Vec::with_capacity(ops.len());
        for (op_type, val) in ops {
            match op_type.as_str() {
                "push" => compiled_ops.push(StateOp::Push(val as usize)),
                "push_current" => compiled_ops.push(StateOp::PushCurrent),
                "pop" => compiled_ops.push(StateOp::Pop(val as usize)),
                other => {
                    return Err(pyo3::exceptions::PyValueError::new_err(format!(
                        "unknown state op: {other}"
                    )))
                }
            }
        }

        self.states[state_id].rules.push(CompiledRule {
            engine,
            dfa_pattern,
            needs_fancy_verify,
            action,
            ops: compiled_ops,
            py_rexmatch,
        });
        Ok(())
    }

    fn build(&mut self, py: Python<'_>) -> PyResult<CompiledLexer> {
        if self.built {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "CompiledLexerBuilder already built",
            ));
        }
        self.built = true;

        let mut states = std::mem::take(&mut self.states);
        for st in &mut states {
            st.finalize();
        }

        Ok(CompiledLexer {
            states,
            state_names: std::mem::take(&mut self.state_names),
            state_id_to_name: std::mem::take(&mut self.state_id_to_name),
            root_state_id: self.root_state_id,
            eol_token_id: self.eol_token_id,
            error_token_id: self.error_token_id,
            has_py_fallback: self.has_py_fallback,
            id_to_token: self.id_to_token.clone_ref(py),
            token_to_id: self.token_to_id.clone_ref(py),
            reg_lock: self.reg_lock.clone_ref(py),
            scratch: Mutex::new(None),
        })
    }
}

#[pyclass(frozen)]
pub struct CompiledLexer {
    states: Vec<CompiledState>,
    state_names: HashMap<String, usize>,
    state_id_to_name: Vec<String>,
    root_state_id: usize,
    eol_token_id: u32,
    error_token_id: u32,
    has_py_fallback: bool,
    id_to_token: Py<PyList>,
    token_to_id: Py<PyDict>,
    reg_lock: Py<PyAny>,
    scratch: Mutex<Option<LexerScratch>>,
}

impl CompiledLexer {
    fn resolve_stack(&self, stack: Option<Vec<String>>) -> PyResult<Vec<usize>> {
        let mut initial_stack = Vec::new();
        if let Some(names) = stack {
            for name in names {
                let sid = *self.state_names.get(&name).ok_or_else(|| {
                    pyo3::exceptions::PyKeyError::new_err(format!("unknown stack state: {name}"))
                })?;
                initial_stack.push(sid);
            }
        }
        if initial_stack.is_empty() {
            initial_stack.push(self.root_state_id);
        }
        Ok(initial_stack)
    }

    fn new_scratch(&self) -> LexerScratch {
        LexerScratch {
            hybrid_caches: self
                .states
                .iter()
                .map(|st| (0..st.dfas.len()).map(|_| None).collect())
                .collect(),
            meta_caches: self
                .states
                .iter()
                .map(|st| (0..st.dfas.len()).map(|_| None).collect())
                .collect(),
            rule_meta_caches: self
                .states
                .iter()
                .map(|st| (0..st.rules.len()).map(|_| None).collect())
                .collect(),
            rule_caps: self
                .states
                .iter()
                .map(|st| (0..st.rules.len()).map(|_| None).collect())
                .collect(),
        }
    }

    #[inline]
    fn register_dynamic_token(&self, py: Python<'_>, tok_obj: &Bound<'_, PyAny>) -> PyResult<u32> {
        let _guard = PyLockGuard::acquire(self.reg_lock.bind(py))?;
        let token_to_id = self.token_to_id.bind(py);
        if let Some(existing) = token_to_id.get_item(tok_obj)? {
            return existing.extract::<u32>();
        }
        let id_to_token = self.id_to_token.bind(py);
        let new_id = id_to_token.len() as u32;
        id_to_token.append(tok_obj)?;
        token_to_id.set_item(tok_obj, new_id)?;
        Ok(new_id)
    }

    fn snapshot_token_vec<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyAny>>> {
        if !self.has_py_fallback {
            return Ok(self.id_to_token.bind(py).iter().collect());
        }
        let _guard = PyLockGuard::acquire(self.reg_lock.bind(py))?;
        Ok(self.id_to_token.bind(py).iter().collect())
    }

    #[inline]
    fn apply_ops(&self, ops: &[StateOp], statestack: &mut Vec<usize>) {
        for op in ops {
            match *op {
                StateOp::Push(sid) => statestack.push(sid),
                StateOp::PushCurrent => {
                    let cur = *statestack.last().unwrap_or(&self.root_state_id);
                    statestack.push(cur);
                }
                StateOp::Pop(n) => {
                    if n >= statestack.len() {
                        statestack.truncate(1);
                        statestack[0] = self.root_state_id;
                    } else {
                        let new_len = statestack.len() - n;
                        statestack.truncate(new_len.max(1));
                    }
                }
            }
        }
    }

    #[inline]
    fn push_span(
        cursor: &mut FastCharCursor<'_>,
        byte_start: usize,
        byte_end: usize,
        tid: u32,
        spans: &mut Vec<SpanRec>,
    ) {
        if cursor.min_emit_byte > 0 && byte_end <= cursor.min_emit_byte {
            return;
        }
        let (char_start, char_len) = if cursor.need_char_offsets {
            let char_start = cursor.to_char(byte_start);
            let char_end = cursor.to_char(byte_end);
            (char_start, char_end - char_start)
        } else {
            (0, 0)
        };
        spans.push(SpanRec {
            byte_start,
            byte_end,
            char_start,
            char_len,
            tid,
        });
    }

    #[inline]
    fn dfa_search<const PROFILE: bool>(
        entry: &MultiDfaEntry,
        h_cache: &mut Option<HybridCache>,
        m_cache: &mut Option<MetaCache>,
        input: &Input<'_>,
        prof: &mut ProfileCounters,
    ) -> Option<(usize, usize)> {
        if PROFILE {
            prof.dfa_searches += 1;
        }
        if let Some(h_dfa) = &entry.hybrid {
            let hc = h_cache.get_or_insert_with(|| h_dfa.create_cache());
            if let Ok(opt_hm) = h_dfa.try_search_fwd(hc, input) {
                if PROFILE {
                    prof.dfa_hybrid_hits += 1;
                }
                return opt_hm.map(|hm| (entry.pat_to_rule[hm.pattern().as_usize()], hm.offset()));
            }
        }
        if PROFILE {
            prof.dfa_meta_fallbacks += 1;
        }
        let mc = m_cache.get_or_insert_with(|| entry.meta.create_cache());
        entry
            .meta
            .search_half_with(mc, input)
            .map(|hm| (entry.pat_to_rule[hm.pattern().as_usize()], hm.offset()))
    }

    #[inline]
    fn try_single_rule_pure<const PROFILE: bool>(
        rule: &CompiledRule,
        meta_cache_slot: &mut Option<MetaCache>,
        caps_slot: &mut Option<Captures>,
        text: &str,
        byte_pos: usize,
        cursor: &mut FastCharCursor<'_>,
        spans: &mut Vec<SpanRec>,
        prof: &mut ProfileCounters,
    ) -> Option<usize> {
        match (&rule.engine, &rule.action) {
            (Engine::Automata(re), ActionKind::ByGroups(group_tokens)) => {
                let mc = meta_cache_slot.get_or_insert_with(|| re.create_cache());
                let caps = caps_slot.get_or_insert_with(|| re.create_captures());
                let input = Input::new(text).range(byte_pos..).anchored(Anchored::Yes);
                re.search_captures_with(mc, &input, caps);
                if caps.is_match() {
                    let end_byte = caps.get_group(0).unwrap().end;
                    for (g_i, &tid) in group_tokens.iter().enumerate() {
                        if tid != SKIP_TOKEN {
                            if let Some(span) = caps.get_group(g_i + 1) {
                                if span.start < span.end {
                                    Self::push_span(cursor, span.start, span.end, tid, spans);
                                }
                            }
                        }
                    }
                    return Some(end_byte);
                }
                None
            }
            (Engine::Automata(re), action) => {
                let mc = meta_cache_slot.get_or_insert_with(|| re.create_cache());
                let input = Input::new(text).range(byte_pos..).anchored(Anchored::Yes);
                if let Some(hm) = re.search_half_with(mc, &input) {
                    if let ActionKind::Token(tid) = action {
                        Self::push_span(cursor, byte_pos, hm.offset(), *tid, spans);
                    }
                    return Some(hm.offset());
                }
                None
            }
            (Engine::Fancy(re), ActionKind::ByGroups(group_tokens)) => {
                let t0 = if PROFILE {
                    prof.fancy_verify_calls += 1;
                    Some(Instant::now())
                } else {
                    None
                };
                let res = re.captures_from_pos(text, byte_pos);
                if let Some(start_t) = t0 {
                    prof.fancy_ns += start_t.elapsed().as_nanos() as u64;
                }
                if let Ok(Some(caps)) = res {
                    if let Some(m0) = caps.get(0) {
                        if m0.start() == byte_pos {
                            let end_byte = m0.end();
                            for (g_i, &tid) in group_tokens.iter().enumerate() {
                                if tid != SKIP_TOKEN {
                                    if let Some(gm) = caps.get(g_i + 1) {
                                        if gm.start() < gm.end() {
                                            Self::push_span(
                                                cursor,
                                                gm.start(),
                                                gm.end(),
                                                tid,
                                                spans,
                                            );
                                        }
                                    }
                                }
                            }
                            return Some(end_byte);
                        }
                    }
                }
                None
            }
            (Engine::Fancy(re), action) => {
                let t0 = if PROFILE {
                    prof.fancy_verify_calls += 1;
                    Some(Instant::now())
                } else {
                    None
                };
                let res = re.find_from_pos(text, byte_pos);
                if let Some(start_t) = t0 {
                    prof.fancy_ns += start_t.elapsed().as_nanos() as u64;
                }
                if let Ok(Some(m)) = res {
                    if m.start() == byte_pos {
                        if let ActionKind::Token(tid) = action {
                            Self::push_span(cursor, byte_pos, m.end(), *tid, spans);
                        }
                        return Some(m.end());
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn run_pure_rust<const PROFILE: bool>(
        &self,
        text: &str,
        initial_stack: Vec<usize>,
        scratch: &mut LexerScratch,
        min_emit_byte: usize,
        max_byte_end: Option<usize>,
        need_char_offsets: bool,
    ) -> (Vec<SpanRec>, Vec<usize>, ProfileCounters) {
        let mut cursor = FastCharCursor::new(text, min_emit_byte, max_byte_end, need_char_offsets);
        let mut statestack = initial_stack;
        if statestack.is_empty() {
            statestack.push(self.root_state_id);
        }

        let estimated_bytes = cursor
            .max_byte_end
            .unwrap_or(text.len())
            .min(text.len())
            .saturating_sub(cursor.min_emit_byte);
        let mut spans: Vec<SpanRec> = Vec::with_capacity(estimated_bytes / 6 + 16);
        let mut prof = ProfileCounters::default();
        let text_len = text.len();
        let mut byte_pos = 0usize;
        let mut zero_width_streak = 0u32;

        while byte_pos <= text_len {
            if cursor.max_byte_end.is_some_and(|limit| byte_pos >= limit) {
                break;
            }
            let state_id = *statestack.last().unwrap_or(&self.root_state_id);
            let state = &self.states[state_id];
            let h_caches = &mut scratch.hybrid_caches[state_id];
            let m_caches = &mut scratch.meta_caches[state_id];
            let r_meta_caches = &mut scratch.rule_meta_caches[state_id];
            let r_caps = &mut scratch.rule_caps[state_id];

            let mut matched = false;
            let mut min_idx = 0usize;
            let mut unindexed_floor = 0usize;

            // Check unindexed rules that appear before any DFA rule first (avoid unnecessary DFA execution).
            for &u_idx in &state.unindexed_rules {
                if u_idx >= state.first_dfa_rule_idx {
                    break;
                }
                if let Some(end_b) = Self::try_single_rule_pure::<PROFILE>(
                    &state.rules[u_idx],
                    &mut r_meta_caches[u_idx],
                    &mut r_caps[u_idx],
                    text,
                    byte_pos,
                    &mut cursor,
                    &mut spans,
                    &mut prof,
                ) {
                    if end_b == byte_pos {
                        zero_width_streak += 1;
                    } else {
                        zero_width_streak = 0;
                    }
                    byte_pos = end_b;
                    if !state.rules[u_idx].ops.is_empty() {
                        self.apply_ops(&state.rules[u_idx].ops, &mut statestack);
                    }
                    matched = true;
                    break;
                }
            }
            if !matched {
                unindexed_floor = state.first_dfa_rule_idx;
            }

            if !matched {
                loop {
                    if let Some(dfa_idx) = state.dfa_index_starting_at(min_idx) {
                        let entry = &state.dfas[dfa_idx];
                        let input = Input::new(text).range(byte_pos..).anchored(Anchored::Yes);
                        let candidate = Self::dfa_search::<PROFILE>(
                            entry,
                            &mut h_caches[dfa_idx],
                            &mut m_caches[dfa_idx],
                            &input,
                            &mut prof,
                        );

                        let limit_idx = candidate.map_or(state.rules.len(), |(idx, _)| idx);
                        let check_from = min_idx.max(unindexed_floor);

                        let mut unindexed_win: Option<(usize, usize)> = None;
                        for &u_idx in &state.unindexed_rules {
                            if u_idx < check_from {
                                continue;
                            }
                            if u_idx >= limit_idx {
                                break;
                            }
                            if let Some(end_b) = Self::try_single_rule_pure::<PROFILE>(
                                &state.rules[u_idx],
                                &mut r_meta_caches[u_idx],
                                &mut r_caps[u_idx],
                                text,
                                byte_pos,
                                &mut cursor,
                                &mut spans,
                                &mut prof,
                            ) {
                                unindexed_win = Some((u_idx, end_b));
                                break;
                            }
                        }

                        if let Some((win_u, end_b)) = unindexed_win {
                            if end_b == byte_pos {
                                zero_width_streak += 1;
                            } else {
                                zero_width_streak = 0;
                            }
                            byte_pos = end_b;
                            if !state.rules[win_u].ops.is_empty() {
                                self.apply_ops(&state.rules[win_u].ops, &mut statestack);
                            }
                            matched = true;
                            break;
                        }

                        let Some((win_idx, dfa_end)) = candidate else {
                            break;
                        };

                        let rule = &state.rules[win_idx];
                        if !rule.needs_fancy_verify {
                            let end_b = match &rule.action {
                                ActionKind::Token(tid) => {
                                    Self::push_span(
                                        &mut cursor,
                                        byte_pos,
                                        dfa_end,
                                        *tid,
                                        &mut spans,
                                    );
                                    dfa_end
                                }
                                ActionKind::None => dfa_end,
                                ActionKind::ByGroups(_) => Self::try_single_rule_pure::<PROFILE>(
                                    rule,
                                    &mut r_meta_caches[win_idx],
                                    &mut r_caps[win_idx],
                                    text,
                                    byte_pos,
                                    &mut cursor,
                                    &mut spans,
                                    &mut prof,
                                )
                                .unwrap_or(dfa_end),
                                ActionKind::PyCallback(_) => unreachable!(),
                            };

                            if end_b == byte_pos {
                                zero_width_streak += 1;
                            } else {
                                zero_width_streak = 0;
                            }
                            byte_pos = end_b;
                            if !rule.ops.is_empty() {
                                self.apply_ops(&rule.ops, &mut statestack);
                            }
                            matched = true;
                            break;
                        } else if let Some(end_b) = Self::try_single_rule_pure::<PROFILE>(
                            rule,
                            &mut r_meta_caches[win_idx],
                            &mut r_caps[win_idx],
                            text,
                            byte_pos,
                            &mut cursor,
                            &mut spans,
                            &mut prof,
                        ) {
                            if end_b == byte_pos {
                                zero_width_streak += 1;
                            } else {
                                zero_width_streak = 0;
                            }
                            byte_pos = end_b;
                            if !rule.ops.is_empty() {
                                self.apply_ops(&rule.ops, &mut statestack);
                            }
                            matched = true;
                            break;
                        } else {
                            if PROFILE {
                                prof.superset_false_positives += 1;
                            }
                            min_idx = win_idx + 1;
                        }
                    } else {
                        let start_scan = min_idx.max(unindexed_floor);
                        for idx in start_scan..state.rules.len() {
                            if let Some(end_b) = Self::try_single_rule_pure::<PROFILE>(
                                &state.rules[idx],
                                &mut r_meta_caches[idx],
                                &mut r_caps[idx],
                                text,
                                byte_pos,
                                &mut cursor,
                                &mut spans,
                                &mut prof,
                            ) {
                                if end_b == byte_pos {
                                    zero_width_streak += 1;
                                } else {
                                    zero_width_streak = 0;
                                }
                                byte_pos = end_b;
                                if !state.rules[idx].ops.is_empty() {
                                    self.apply_ops(&state.rules[idx].ops, &mut statestack);
                                }
                                matched = true;
                                break;
                            }
                        }
                        break;
                    }
                }
            }

            if zero_width_streak > 64 {
                break;
            }

            if !matched {
                if byte_pos >= text_len {
                    break;
                }
                let bytes = text.as_bytes();
                if bytes[byte_pos] == b'\n' {
                    statestack.clear();
                    statestack.push(self.root_state_id);
                    Self::push_span(
                        &mut cursor,
                        byte_pos,
                        byte_pos + 1,
                        self.eol_token_id,
                        &mut spans,
                    );
                    byte_pos += 1;
                    zero_width_streak = 0;
                } else {
                    let ch = text[byte_pos..].chars().next().unwrap();
                    Self::push_span(
                        &mut cursor,
                        byte_pos,
                        byte_pos + ch.len_utf8(),
                        self.error_token_id,
                        &mut spans,
                    );
                    byte_pos += ch.len_utf8();
                    zero_width_streak = 0;
                }
            }
        }

        if PROFILE {
            prof.tokens_emitted = spans.len() as u64;
        }
        (spans, statestack, prof)
    }

    #[allow(clippy::too_many_arguments)]
    fn try_single_rule_py<const PROFILE: bool>(
        &self,
        py: Python<'_>,
        rule: &CompiledRule,
        meta_cache_slot: &mut Option<MetaCache>,
        caps_slot: &mut Option<Captures>,
        text: &str,
        py_text: &Bound<'_, PyString>,
        py_lexer: &Bound<'_, PyAny>,
        byte_pos: usize,
        cursor: &mut FastCharCursor<'_>,
        spans: &mut Vec<SpanRec>,
        overrides: &mut Vec<(usize, Py<PyAny>, Py<PyAny>)>,
        prof: &mut ProfileCounters,
    ) -> PyResult<Option<usize>> {
        match (&rule.engine, &rule.action) {
            (Engine::Automata(_), ActionKind::PyCallback(cb)) => self.invoke_py_rule::<PROFILE>(
                py,
                cursor,
                text,
                py_text,
                py_lexer,
                rule.py_rexmatch.as_ref().unwrap(),
                Some(cb),
                byte_pos,
                spans,
                overrides,
                prof,
            ),
            (Engine::Fancy(re), ActionKind::PyCallback(cb)) => {
                let t0 = if PROFILE {
                    prof.fancy_verify_calls += 1;
                    Some(Instant::now())
                } else {
                    None
                };
                let res = re.find_from_pos(text, byte_pos);
                if let Some(start_t) = t0 {
                    prof.fancy_ns += start_t.elapsed().as_nanos() as u64;
                }
                let matched_here = match res {
                    Ok(Some(m)) => m.start() == byte_pos,
                    _ => false,
                };
                if matched_here {
                    self.invoke_py_rule::<PROFILE>(
                        py,
                        cursor,
                        text,
                        py_text,
                        py_lexer,
                        rule.py_rexmatch.as_ref().unwrap(),
                        Some(cb),
                        byte_pos,
                        spans,
                        overrides,
                        prof,
                    )
                } else {
                    Ok(None)
                }
            }
            (Engine::PyOnly, ActionKind::PyCallback(cb)) => self.invoke_py_rule::<PROFILE>(
                py,
                cursor,
                text,
                py_text,
                py_lexer,
                rule.py_rexmatch.as_ref().unwrap(),
                Some(cb),
                byte_pos,
                spans,
                overrides,
                prof,
            ),
            (Engine::PyOnly, ActionKind::Token(tid)) => {
                let t0 = if PROFILE {
                    prof.py_regex_calls += 1;
                    Some(Instant::now())
                } else {
                    None
                };
                let char_pos = cursor.to_char(byte_pos);
                let m = rule
                    .py_rexmatch
                    .as_ref()
                    .unwrap()
                    .bind(py)
                    .call1((py_text, char_pos))?;
                let out = if !m.is_none() {
                    if PROFILE {
                        prof.py_regex_hits += 1;
                    }
                    let end_char: usize = m.call_method0(pyo3::intern!(py, "end"))?.extract()?;
                    let end_byte = cursor.to_byte(end_char);
                    Self::push_span(cursor, byte_pos, end_byte, *tid, spans);
                    Some(end_byte)
                } else {
                    None
                };
                if let Some(start_t) = t0 {
                    prof.py_fallback_ns += start_t.elapsed().as_nanos() as u64;
                }
                Ok(out)
            }
            (Engine::PyOnly, ActionKind::None) => {
                let t0 = if PROFILE {
                    prof.py_regex_calls += 1;
                    Some(Instant::now())
                } else {
                    None
                };
                let char_pos = cursor.to_char(byte_pos);
                let m = rule
                    .py_rexmatch
                    .as_ref()
                    .unwrap()
                    .bind(py)
                    .call1((py_text, char_pos))?;
                let out = if !m.is_none() {
                    if PROFILE {
                        prof.py_regex_hits += 1;
                    }
                    let end_char: usize = m.call_method0(pyo3::intern!(py, "end"))?.extract()?;
                    Some(cursor.to_byte(end_char))
                } else {
                    None
                };
                if let Some(start_t) = t0 {
                    prof.py_fallback_ns += start_t.elapsed().as_nanos() as u64;
                }
                Ok(out)
            }
            _ => Ok(Self::try_single_rule_pure::<PROFILE>(
                rule,
                meta_cache_slot,
                caps_slot,
                text,
                byte_pos,
                cursor,
                spans,
                prof,
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_with_python<const PROFILE: bool>(
        &self,
        py: Python<'_>,
        text: &str,
        py_text: &Bound<'_, PyString>,
        py_lexer: &Bound<'_, PyAny>,
        initial_stack: Vec<usize>,
        scratch: &mut LexerScratch,
        min_emit_byte: usize,
        max_byte_end: Option<usize>,
        need_char_offsets: bool,
    ) -> PyResult<(
        Vec<SpanRec>,
        Vec<(usize, Py<PyAny>, Py<PyAny>)>,
        Vec<usize>,
        ProfileCounters,
    )> {
        let mut cursor = FastCharCursor::new(text, min_emit_byte, max_byte_end, need_char_offsets);
        let mut statestack = initial_stack;
        if statestack.is_empty() {
            statestack.push(self.root_state_id);
        }

        let estimated_bytes = cursor
            .max_byte_end
            .unwrap_or(text.len())
            .min(text.len())
            .saturating_sub(cursor.min_emit_byte);
        let mut spans: Vec<SpanRec> = Vec::with_capacity(estimated_bytes / 6 + 16);
        let mut overrides: Vec<(usize, Py<PyAny>, Py<PyAny>)> = Vec::new();
        let mut prof = ProfileCounters::default();
        let text_len = text.len();
        let mut byte_pos = 0usize;
        let mut zero_width_streak = 0u32;

        while byte_pos <= text_len {
            if cursor.max_byte_end.is_some_and(|limit| byte_pos >= limit) {
                break;
            }
            let state_id = *statestack.last().unwrap_or(&self.root_state_id);
            let state = &self.states[state_id];
            let h_caches = &mut scratch.hybrid_caches[state_id];
            let m_caches = &mut scratch.meta_caches[state_id];
            let r_meta_caches = &mut scratch.rule_meta_caches[state_id];
            let r_caps = &mut scratch.rule_caps[state_id];

            let mut matched = false;
            let mut min_idx = 0usize;
            let mut unindexed_floor = 0usize;

            // Check unindexed rules that appear before any DFA rule first (avoid unnecessary DFA execution).
            for &u_idx in &state.unindexed_rules {
                if u_idx >= state.first_dfa_rule_idx {
                    break;
                }
                if let Some(end_b) = self.try_single_rule_py::<PROFILE>(
                    py,
                    &state.rules[u_idx],
                    &mut r_meta_caches[u_idx],
                    &mut r_caps[u_idx],
                    text,
                    py_text,
                    py_lexer,
                    byte_pos,
                    &mut cursor,
                    &mut spans,
                    &mut overrides,
                    &mut prof,
                )? {
                    if end_b == byte_pos {
                        zero_width_streak += 1;
                    } else {
                        zero_width_streak = 0;
                    }
                    byte_pos = end_b;
                    if !state.rules[u_idx].ops.is_empty() {
                        self.apply_ops(&state.rules[u_idx].ops, &mut statestack);
                    }
                    matched = true;
                    break;
                }
            }
            if !matched {
                unindexed_floor = state.first_dfa_rule_idx;
            }

            if !matched {
                loop {
                    if let Some(dfa_idx) = state.dfa_index_starting_at(min_idx) {
                        let entry = &state.dfas[dfa_idx];
                        let input = Input::new(text).range(byte_pos..).anchored(Anchored::Yes);
                        let candidate = Self::dfa_search::<PROFILE>(
                            entry,
                            &mut h_caches[dfa_idx],
                            &mut m_caches[dfa_idx],
                            &input,
                            &mut prof,
                        );

                        let limit_idx = candidate.map_or(state.rules.len(), |(idx, _)| idx);
                        let check_from = min_idx.max(unindexed_floor);

                        let mut unindexed_win: Option<(usize, usize)> = None;
                        for &u_idx in &state.unindexed_rules {
                            if u_idx < check_from {
                                continue;
                            }
                            if u_idx >= limit_idx {
                                break;
                            }
                            if let Some(end_b) = self.try_single_rule_py::<PROFILE>(
                                py,
                                &state.rules[u_idx],
                                &mut r_meta_caches[u_idx],
                                &mut r_caps[u_idx],
                                text,
                                py_text,
                                py_lexer,
                                byte_pos,
                                &mut cursor,
                                &mut spans,
                                &mut overrides,
                                &mut prof,
                            )? {
                                unindexed_win = Some((u_idx, end_b));
                                break;
                            }
                        }

                        if let Some((win_u, end_b)) = unindexed_win {
                            if end_b == byte_pos {
                                zero_width_streak += 1;
                            } else {
                                zero_width_streak = 0;
                            }
                            byte_pos = end_b;
                            if !state.rules[win_u].ops.is_empty() {
                                self.apply_ops(&state.rules[win_u].ops, &mut statestack);
                            }
                            matched = true;
                            break;
                        }

                        let Some((win_idx, dfa_end)) = candidate else {
                            break;
                        };

                        let rule = &state.rules[win_idx];
                        if !rule.needs_fancy_verify {
                            let end_b = match &rule.action {
                                ActionKind::Token(tid) => {
                                    Self::push_span(
                                        &mut cursor,
                                        byte_pos,
                                        dfa_end,
                                        *tid,
                                        &mut spans,
                                    );
                                    dfa_end
                                }
                                ActionKind::None => dfa_end,
                                ActionKind::ByGroups(_) => Self::try_single_rule_pure::<PROFILE>(
                                    rule,
                                    &mut r_meta_caches[win_idx],
                                    &mut r_caps[win_idx],
                                    text,
                                    byte_pos,
                                    &mut cursor,
                                    &mut spans,
                                    &mut prof,
                                )
                                .unwrap_or(dfa_end),
                                ActionKind::PyCallback(cb) => self
                                    .invoke_py_rule::<PROFILE>(
                                        py,
                                        &mut cursor,
                                        text,
                                        py_text,
                                        py_lexer,
                                        rule.py_rexmatch.as_ref().unwrap(),
                                        Some(cb),
                                        byte_pos,
                                        &mut spans,
                                        &mut overrides,
                                        &mut prof,
                                    )?
                                    .unwrap_or(dfa_end),
                            };

                            if end_b == byte_pos {
                                zero_width_streak += 1;
                            } else {
                                zero_width_streak = 0;
                            }
                            byte_pos = end_b;
                            if !rule.ops.is_empty() {
                                self.apply_ops(&rule.ops, &mut statestack);
                            }
                            matched = true;
                            break;
                        } else if let Some(end_b) = self.try_single_rule_py::<PROFILE>(
                            py,
                            rule,
                            &mut r_meta_caches[win_idx],
                            &mut r_caps[win_idx],
                            text,
                            py_text,
                            py_lexer,
                            byte_pos,
                            &mut cursor,
                            &mut spans,
                            &mut overrides,
                            &mut prof,
                        )? {
                            if end_b == byte_pos {
                                zero_width_streak += 1;
                            } else {
                                zero_width_streak = 0;
                            }
                            byte_pos = end_b;
                            if !rule.ops.is_empty() {
                                self.apply_ops(&rule.ops, &mut statestack);
                            }
                            matched = true;
                            break;
                        } else {
                            if PROFILE {
                                prof.superset_false_positives += 1;
                            }
                            min_idx = win_idx + 1;
                        }
                    } else {
                        let start_scan = min_idx.max(unindexed_floor);
                        for idx in start_scan..state.rules.len() {
                            let rule = &state.rules[idx];
                            let matched_opt = if matches!(&rule.engine, Engine::Automata(_))
                                && matches!(&rule.action, ActionKind::PyCallback(_))
                            {
                                let Engine::Automata(re) = &rule.engine else {
                                    unreachable!()
                                };
                                let mc =
                                    r_meta_caches[idx].get_or_insert_with(|| re.create_cache());
                                let input =
                                    Input::new(text).range(byte_pos..).anchored(Anchored::Yes);
                                if re.search_half_with(mc, &input).is_some() {
                                    self.try_single_rule_py::<PROFILE>(
                                        py,
                                        rule,
                                        &mut r_meta_caches[idx],
                                        &mut r_caps[idx],
                                        text,
                                        py_text,
                                        py_lexer,
                                        byte_pos,
                                        &mut cursor,
                                        &mut spans,
                                        &mut overrides,
                                        &mut prof,
                                    )?
                                } else {
                                    None
                                }
                            } else {
                                self.try_single_rule_py::<PROFILE>(
                                    py,
                                    rule,
                                    &mut r_meta_caches[idx],
                                    &mut r_caps[idx],
                                    text,
                                    py_text,
                                    py_lexer,
                                    byte_pos,
                                    &mut cursor,
                                    &mut spans,
                                    &mut overrides,
                                    &mut prof,
                                )?
                            };

                            if let Some(end_b) = matched_opt {
                                if end_b == byte_pos {
                                    zero_width_streak += 1;
                                } else {
                                    zero_width_streak = 0;
                                }
                                byte_pos = end_b;
                                if !rule.ops.is_empty() {
                                    self.apply_ops(&rule.ops, &mut statestack);
                                }
                                matched = true;
                                break;
                            }
                        }
                        break;
                    }
                }
            }

            if zero_width_streak > 64 {
                break;
            }

            if !matched {
                if byte_pos >= text_len {
                    break;
                }
                let bytes = text.as_bytes();
                if bytes[byte_pos] == b'\n' {
                    statestack.clear();
                    statestack.push(self.root_state_id);
                    Self::push_span(
                        &mut cursor,
                        byte_pos,
                        byte_pos + 1,
                        self.eol_token_id,
                        &mut spans,
                    );
                    byte_pos += 1;
                    zero_width_streak = 0;
                } else {
                    let ch = text[byte_pos..].chars().next().unwrap();
                    Self::push_span(
                        &mut cursor,
                        byte_pos,
                        byte_pos + ch.len_utf8(),
                        self.error_token_id,
                        &mut spans,
                    );
                    byte_pos += ch.len_utf8();
                    zero_width_streak = 0;
                }
            }
        }

        if PROFILE {
            prof.tokens_emitted = spans.len() as u64;
        }
        Ok((spans, overrides, statestack, prof))
    }

    #[allow(clippy::too_many_arguments)]
    fn invoke_py_rule<const PROFILE: bool>(
        &self,
        py: Python<'_>,
        cursor: &mut FastCharCursor<'_>,
        text: &str,
        py_text: &Bound<'_, PyString>,
        py_lexer: &Bound<'_, PyAny>,
        py_rexmatch: &Py<PyAny>,
        py_action: Option<&Py<PyAny>>,
        byte_pos: usize,
        spans: &mut Vec<SpanRec>,
        overrides: &mut Vec<(usize, Py<PyAny>, Py<PyAny>)>,
        prof: &mut ProfileCounters,
    ) -> PyResult<Option<usize>> {
        let t0 = if PROFILE {
            prof.py_regex_calls += 1;
            Some(Instant::now())
        } else {
            None
        };

        let char_pos = cursor.to_char(byte_pos);
        let m = py_rexmatch.bind(py).call1((py_text, char_pos))?;
        if m.is_none() {
            if let Some(start_t) = t0 {
                prof.py_fallback_ns += start_t.elapsed().as_nanos() as u64;
            }
            return Ok(None);
        }
        if PROFILE {
            prof.py_regex_hits += 1;
        }
        let end_char: usize = m.call_method0(pyo3::intern!(py, "end"))?.extract()?;

        if let Some(cb) = py_action {
            if PROFILE {
                prof.py_callback_calls += 1;
            }
            let iter = cb.bind(py).call1((py_lexer, &m))?;
            for item_res in iter.try_iter()? {
                let item = item_res?;
                let (pos_c, tok_obj, val_obj): (usize, Bound<'_, PyAny>, Bound<'_, PyAny>) =
                    item.extract()?;
                let val_str = val_obj.cast::<PyString>()?.to_str()?;
                let val_char_len = if val_str.is_ascii() {
                    val_str.len()
                } else {
                    val_str.chars().count()
                };

                let tid = self.register_dynamic_token(py, &tok_obj)?;

                let total_chars = cursor.total_char_len();
                let (sb, eb, matches_slice) = match pos_c.checked_add(val_char_len) {
                    Some(span_end_char) if span_end_char <= total_chars => {
                        let sb = cursor.to_byte(pos_c);
                        let eb = cursor.to_byte(span_end_char);
                        (sb, eb, &text[sb..eb] == val_str)
                    }
                    _ => {
                        let end = text.len();
                        (end, end, false)
                    }
                };

                if cursor.min_emit_byte == 0 || eb > cursor.min_emit_byte {
                    let span_idx = spans.len();
                    let (char_start, char_len) = if cursor.need_char_offsets {
                        (pos_c, val_char_len)
                    } else {
                        (0, 0)
                    };
                    spans.push(SpanRec {
                        byte_start: sb,
                        byte_end: eb,
                        char_start,
                        char_len,
                        tid,
                    });
                    if !matches_slice {
                        overrides.push((span_idx, tok_obj.unbind(), val_obj.unbind()));
                    }
                }
            }
        }

        let end_byte = cursor.to_byte(end_char);
        if let Some(start_t) = t0 {
            prof.py_fallback_ns += start_t.elapsed().as_nanos() as u64;
        }
        Ok(Some(end_byte))
    }

    fn execute_core<'py, const PROFILE: bool>(
        &self,
        py: Python<'py>,
        text: &Bound<'py, PyString>,
        py_lexer: &Bound<'py, PyAny>,
        stack: Option<Vec<String>>,
        min_emit_byte: usize,
        max_byte_end: Option<usize>,
        need_char_offsets: bool,
    ) -> PyResult<(
        Vec<SpanRec>,
        Vec<(usize, Py<PyAny>, Py<PyAny>)>,
        Vec<String>,
        ProfileCounters,
    )> {
        let initial_stack = self.resolve_stack(stack)?;

        let mut scratch = self
            .scratch
            .try_lock()
            .ok()
            .and_then(|mut guard| guard.take())
            .unwrap_or_else(|| self.new_scratch());

        let text_str = text.to_str()?;
        let result = if !self.has_py_fallback {
            let scan_limit = max_byte_end.unwrap_or(text_str.len()).min(text_str.len());
            let (spans, final_stack, prof, returned_scratch) = if scan_limit >= 16_384 {
                let owned = text_str.to_owned();
                py.detach(move || {
                    let (s, st, p) = self.run_pure_rust::<PROFILE>(
                        &owned,
                        initial_stack,
                        &mut scratch,
                        min_emit_byte,
                        max_byte_end,
                        need_char_offsets,
                    );
                    (s, st, p, scratch)
                })
            } else {
                let (s, st, p) = self.run_pure_rust::<PROFILE>(
                    text_str,
                    initial_stack,
                    &mut scratch,
                    min_emit_byte,
                    max_byte_end,
                    need_char_offsets,
                );
                (s, st, p, scratch)
            };
            scratch = returned_scratch;
            let final_names = final_stack
                .into_iter()
                .map(|id| self.state_id_to_name[id].clone())
                .collect();
            Ok((spans, Vec::new(), final_names, prof))
        } else {
            let (spans, overrides, final_stack, prof) = self.run_with_python::<PROFILE>(
                py,
                text_str,
                text,
                py_lexer,
                initial_stack,
                &mut scratch,
                min_emit_byte,
                max_byte_end,
                need_char_offsets,
            )?;
            let final_names = final_stack
                .into_iter()
                .map(|id| self.state_id_to_name[id].clone())
                .collect();
            Ok((spans, overrides, final_names, prof))
        };

        if let Ok(mut guard) = self.scratch.try_lock() {
            if guard.is_none() {
                *guard = Some(scratch);
            }
        }

        result
    }
}

#[pymethods]
impl CompiledLexer {
    fn stats(&self) -> (usize, usize, usize, usize, usize, bool) {
        let mut total = 0usize;
        let mut automata = 0usize;
        let mut fancy_indexed = 0usize;
        let mut unindexed = 0usize;
        for st in &self.states {
            total += st.rules.len();
            unindexed += st.unindexed_rules.len();
            for r in &st.rules {
                match r.engine {
                    Engine::Automata(_) if r.dfa_pattern.is_some() => automata += 1,
                    Engine::Fancy(_) if r.needs_fancy_verify => fancy_indexed += 1,
                    _ => {}
                }
            }
        }
        (
            self.states.len(),
            total,
            automata,
            fancy_indexed,
            unindexed,
            self.has_py_fallback,
        )
    }

    fn token_table<'py>(&self, py: Python<'py>) -> PyResult<Py<PyTuple>> {
        let items = self.snapshot_token_vec(py)?;
        Ok(PyTuple::new(py, items)?.unbind())
    }

    #[pyo3(signature = (text, py_lexer, stack=None))]
    fn lex_py_tokens<'py>(
        &self,
        py: Python<'py>,
        text: &Bound<'py, PyString>,
        py_lexer: &Bound<'py, PyAny>,
        stack: Option<Vec<String>>,
    ) -> PyResult<Py<PyList>> {
        let text_str = text.to_str()?;
        let (spans, overrides, _final_stack, _prof) =
            self.execute_core::<false>(py, text, py_lexer, stack, 0, None, true)?;

        let tok_cache = self.snapshot_token_vec(py)?;
        let mut py_items: Vec<Bound<'py, PyTuple>> = Vec::with_capacity(spans.len());

        if overrides.is_empty() {
            for r in &spans {
                let sub = PyString::new(py, &text_str[r.byte_start..r.byte_end]);
                py_items.push((r.char_start, &tok_cache[r.tid as usize], sub).into_pyobject(py)?);
            }
        } else {
            let ov_map: HashMap<usize, (Py<PyAny>, Py<PyAny>)> =
                overrides.into_iter().map(|(i, t, v)| (i, (t, v))).collect();
            for (idx, r) in spans.iter().enumerate() {
                if let Some((tok_ov, val_ov)) = ov_map.get(&idx) {
                    py_items
                        .push((r.char_start, tok_ov.bind(py), val_ov.bind(py)).into_pyobject(py)?);
                } else {
                    let sub = PyString::new(py, &text_str[r.byte_start..r.byte_end]);
                    py_items
                        .push((r.char_start, &tok_cache[r.tid as usize], sub).into_pyobject(py)?);
                }
            }
        }

        Ok(PyList::new(py, py_items)?.unbind())
    }

    #[pyo3(signature = (text, py_lexer, stack=None))]
    fn lex_spans<'py>(
        &self,
        py: Python<'py>,
        text: &Bound<'py, PyString>,
        py_lexer: &Bound<'py, PyAny>,
        stack: Option<Vec<String>>,
    ) -> PyResult<(Vec<(usize, usize, u32)>, Py<PyTuple>, Vec<String>)> {
        let (spans, _overrides, final_stack, _prof) =
            self.execute_core::<false>(py, text, py_lexer, stack, 0, None, true)?;
        let table = self.token_table(py)?;
        let out = spans
            .into_iter()
            .map(|r| (r.char_start, r.char_len, r.tid))
            .collect();
        Ok((out, table, final_stack))
    }

    #[pyo3(signature = (text, py_lexer, stack=None))]
    fn lex_flat_spans<'py>(
        &self,
        py: Python<'py>,
        text: &Bound<'py, PyString>,
        py_lexer: &Bound<'py, PyAny>,
        stack: Option<Vec<String>>,
    ) -> PyResult<(Bound<'py, PyBytes>, Py<PyTuple>, Vec<String>)> {
        let (spans, _overrides, final_stack, _prof) =
            self.execute_core::<false>(py, text, py_lexer, stack, 0, None, true)?;
        let table = self.token_table(py)?;
        let mut buf = Vec::with_capacity(spans.len() * 12);
        for r in &spans {
            buf.extend_from_slice(&(r.char_start as u32).to_le_bytes());
            buf.extend_from_slice(&(r.char_len as u32).to_le_bytes());
            buf.extend_from_slice(&r.tid.to_le_bytes());
        }
        Ok((PyBytes::new(py, &buf), table, final_stack))
    }

    #[pyo3(signature = (text, py_lexer, stack=None))]
    fn lex_flat_spans_profiled<'py>(
        &self,
        py: Python<'py>,
        text: &Bound<'py, PyString>,
        py_lexer: &Bound<'py, PyAny>,
        stack: Option<Vec<String>>,
    ) -> PyResult<(Bound<'py, PyBytes>, Py<PyTuple>, Vec<String>, LexProfile)> {
        let (spans, _overrides, final_stack, prof) =
            self.execute_core::<true>(py, text, py_lexer, stack, 0, None, true)?;
        let table = self.token_table(py)?;
        let mut buf = Vec::with_capacity(spans.len() * 12);
        for r in &spans {
            buf.extend_from_slice(&(r.char_start as u32).to_le_bytes());
            buf.extend_from_slice(&(r.char_len as u32).to_le_bytes());
            buf.extend_from_slice(&r.tid.to_le_bytes());
        }
        Ok((PyBytes::new(py, &buf), table, final_stack, prof.into()))
    }

    /// Return flat UTF-8 byte spans intersecting the requested range.
    #[pyo3(signature = (text, py_lexer, stack=None, start_byte=0, end_byte=None))]
    fn lex_flat_byte_spans<'py>(
        &self,
        py: Python<'py>,
        text: &Bound<'py, PyString>,
        py_lexer: &Bound<'py, PyAny>,
        stack: Option<Vec<String>>,
        start_byte: usize,
        end_byte: Option<usize>,
    ) -> PyResult<(Bound<'py, PyBytes>, Py<PyTuple>, Vec<String>)> {
        let text_len = text.to_str()?.len();
        let limit = end_byte.unwrap_or(text_len).min(text_len);
        if start_byte >= limit {
            let initial_stack = self.resolve_stack(stack)?;
            let final_stack = initial_stack
                .into_iter()
                .map(|id| self.state_id_to_name[id].clone())
                .collect();
            return Ok((PyBytes::new(py, &[]), self.token_table(py)?, final_stack));
        }

        let (spans, _overrides, final_stack, _prof) =
            self.execute_core::<false>(py, text, py_lexer, stack, start_byte, Some(limit), false)?;
        let table = self.token_table(py)?;
        let mut buf = Vec::with_capacity(spans.len() * 12);
        for span in &spans {
            if span.byte_end <= start_byte || span.byte_start >= limit {
                continue;
            }
            let clipped_start = span.byte_start.max(start_byte);
            let clipped_end = span.byte_end.min(limit);
            if clipped_start < clipped_end {
                buf.extend_from_slice(&(clipped_start as u32).to_le_bytes());
                buf.extend_from_slice(&((clipped_end - clipped_start) as u32).to_le_bytes());
                buf.extend_from_slice(&span.tid.to_le_bytes());
            }
        }
        Ok((PyBytes::new(py, &buf), table, final_stack))
    }
}

#[pymodule]
fn _rsgments(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<CompiledLexerBuilder>()?;
    m.add_class::<CompiledLexer>()?;
    m.add_class::<LexProfile>()?;
    Ok(())
}
