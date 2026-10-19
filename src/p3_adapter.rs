//! fork (setouthq/rust#79): a wasm32-wasip3 link that embeds the wasip1
//! adapter says so.
//!
//! On every link the linker offers wit-component the wasip1 adapter for the
//! `wasi_snapshot_preview1` import module, and wit-component embeds it only
//! when the core module imports from that module. On wasm32-wasip3 std and
//! wasi-libc import nothing from preview 1, so the adapter normally goes
//! unused. A crate on the preview 1 bindings (the `wasi` crates 0.9 to 0.11,
//! getrandom 0.1 and 0.2 through them) brings it back. The adapter is written
//! against WASI 0.2, so the component then imports a few `wasi:*@0.2`
//! interfaces next to its `wasi:*@0.3.0` ones: it still runs on hosts that
//! serve both, but it no longer fits a WASI 0.3-only world, and nothing said
//! so at build time.
//!
//! This module finds that case and words the warning. A link is a wasm32-wasip3
//! link when `--cooperative-threading` reaches `wasm-ld` (rustc passes it only
//! for wasm32-wasip3) or when the core module imports a `wasi:*@0.3` interface.
//! The warning names each preview 1 import and, when the module has a name
//! section, up to three functions that call it.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use wasmparser::{KnownCustom, Name, Operator, Parser, Payload, TypeRef};

/// The import module the wasip1 adapter serves.
pub(crate) const PREVIEW1: &str = "wasi_snapshot_preview1";

/// How many callers the warning names per import.
const MAX_CALLERS: usize = 3;

/// The warning for this link, or `None` when it is not a wasm32-wasip3 link
/// or the core module imports nothing from preview 1.
pub(crate) fn warning(core_module: &[u8], lld_flags: &[&OsString]) -> Option<String> {
    let flagged = lld_flags.iter().any(|f| *f == "--cooperative-threading");
    let scan = scan(core_module);
    if !(flagged || scan.imports_wasi_0_3) || scan.preview1.is_empty() {
        return None;
    }
    let names = scan
        .preview1
        .iter()
        .map(|(name, callers)| {
            if callers.is_empty() {
                name.clone()
            } else {
                let list = callers.iter().cloned().collect::<Vec<_>>().join(", ");
                format!("{name} (called from {list})")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let plural = if scan.preview1.len() == 1 { "" } else { "s" };
    Some(format!(
        "wasm32-wasip3 link embeds the wasip1 adapter for the core module's \
         {PREVIEW1} import{plural}: {names}\n\
         note: the component also imports wasi:*@0.2 interfaces beside wasi:*@0.3.0; \
         it runs on hosts that serve both, but does not fit a WASI 0.3-only world. \
         Find the crate with `cargo tree -i wasi --target wasm32-wasip3` \
         (the `wasi` 0.9 to 0.11 crates are preview 1 bindings)"
    ))
}

#[derive(Default)]
struct Scan {
    /// preview 1 import name -> names of functions that call it.
    preview1: BTreeMap<String, BTreeSet<String>>,
    /// The module imports some `wasi:<package>/<interface>@0.3.x`.
    imports_wasi_0_3: bool,
}

/// Reads the imports, the calls into preview 1 imports and the function names.
/// A module the parser rejects yields what was read before the error: the
/// warning is advice, never a reason to fail a link.
fn scan(core_module: &[u8]) -> Scan {
    let mut out = Scan::default();
    // function index -> preview 1 import name
    let mut targets = BTreeMap::<u32, String>::new();
    // preview 1 import name -> caller function indices
    let mut callers = BTreeMap::<String, BTreeSet<u32>>::new();
    let mut imported_funcs = 0u32;
    let mut defined = 0u32;
    let mut names = BTreeMap::<u32, String>::new();

    for payload in Parser::new(0).parse_all(core_module) {
        let Ok(payload) = payload else { break };
        match payload {
            Payload::ImportSection(reader) => {
                for import in reader.into_imports() {
                    let Ok(import) = import else { break };
                    if import.module.starts_with("wasi:") && is_wasi_0_3(import.module) {
                        out.imports_wasi_0_3 = true;
                    }
                    if let TypeRef::Func(_) | TypeRef::FuncExact(_) = import.ty {
                        if import.module == PREVIEW1 {
                            targets.insert(imported_funcs, import.name.to_string());
                            out.preview1.entry(import.name.to_string()).or_default();
                        }
                        imported_funcs += 1;
                    }
                }
            }
            Payload::CodeSectionEntry(body) => {
                let index = imported_funcs + defined;
                defined += 1;
                if targets.is_empty() {
                    continue;
                }
                let Ok(mut ops) = body.get_operators_reader() else {
                    continue;
                };
                while !ops.eof() {
                    let Ok(op) = ops.read() else { break };
                    let callee = match op {
                        Operator::Call { function_index }
                        | Operator::ReturnCall { function_index } => function_index,
                        _ => continue,
                    };
                    if let Some(name) = targets.get(&callee) {
                        callers.entry(name.clone()).or_default().insert(index);
                    }
                }
            }
            Payload::CustomSection(reader) => {
                if targets.is_empty() {
                    continue;
                }
                if let KnownCustom::Name(reader) = reader.as_known() {
                    for subsection in reader {
                        let Ok(Name::Function(map)) = subsection else {
                            continue;
                        };
                        for naming in map {
                            let Ok(naming) = naming else { break };
                            names.insert(naming.index, naming.name.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    for (import, indices) in callers {
        let set = out.preview1.entry(import).or_default();
        for index in indices {
            if set.len() == MAX_CALLERS {
                break;
            }
            if let Some(name) = names.get(&index) {
                set.insert(demangle(name));
            }
        }
    }
    out
}

/// `wasi:cli/environment@0.3.0`, `wasi:io/...@0.3.0-rc-2026-03-15` and the like.
fn is_wasi_0_3(module: &str) -> bool {
    module
        .rsplit_once('@')
        .is_some_and(|(_, version)| version == "0.3" || version.starts_with("0.3."))
}

/// A readable name for a symbol from the name section. rustc links with
/// `--no-demangle`, so Rust names arrive mangled. Legacy (`_ZN...E`) and the
/// plain paths of v0 (`_R` with nested and crate-root paths) are rendered as
/// `a::b::c`; anything else comes back unchanged.
pub(crate) fn demangle(sym: &str) -> String {
    legacy(sym)
        .or_else(|| v0(sym))
        .unwrap_or_else(|| sym.to_string())
}

fn legacy(sym: &str) -> Option<String> {
    let rest = sym.strip_prefix("_ZN")?;
    let mut rest = rest.as_bytes();
    let mut parts = Vec::new();
    loop {
        if rest.first() == Some(&b'E') {
            rest = &rest[1..];
            break;
        }
        let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        let len: usize = std::str::from_utf8(&rest[..digits]).ok()?.parse().ok()?;
        rest = &rest[digits..];
        if rest.len() < len {
            return None;
        }
        parts.push(std::str::from_utf8(&rest[..len]).ok()?);
        rest = &rest[len..];
    }
    // A suffix such as `.llvm.123` may follow the `E`.
    if !rest.is_empty() && rest[0] != b'.' {
        return None;
    }
    if let Some(last) = parts.last() {
        let hash = last.len() == 17
            && last.starts_with('h')
            && last[1..].bytes().all(|b| b.is_ascii_hexdigit());
        if hash {
            parts.pop();
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(
        parts
            .iter()
            .map(|p| unescape_legacy(p))
            .collect::<Vec<_>>()
            .join("::"),
    )
}

fn unescape_legacy(part: &str) -> String {
    // A leading `$` is written `_$`.
    let part = if part.starts_with("_$") {
        &part[1..]
    } else {
        part
    };
    let mut out = String::new();
    let mut rest = part;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("..") {
            out.push_str("::");
            rest = r;
            continue;
        }
        if rest.starts_with('$') {
            if let Some(end) = rest[1..].find('$') {
                let code = &rest[1..1 + end];
                let ch = match code {
                    "SP" => Some('@'),
                    "BP" => Some('*'),
                    "RF" => Some('&'),
                    "LT" => Some('<'),
                    "GT" => Some('>'),
                    "LP" => Some('('),
                    "RP" => Some(')'),
                    "C" => Some(','),
                    _ => code
                        .strip_prefix('u')
                        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                        .and_then(char::from_u32),
                };
                if let Some(ch) = ch {
                    out.push(ch);
                    rest = &rest[2 + end..];
                    continue;
                }
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

/// The subset of v0 mangling that plain functions use: `N<ns><path><ident>`
/// and `C<ident>`, with disambiguators. Generic arguments, impl paths and
/// back-references are left to the raw symbol.
fn v0(sym: &str) -> Option<String> {
    let rest = sym.strip_prefix("_R")?;
    // An optional encoding version (decimal) may come first.
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    let mut p = V0 { s: rest.as_bytes() };
    let path = p.path()?;
    Some(path.join("::"))
}

struct V0<'a> {
    s: &'a [u8],
}

impl V0<'_> {
    fn path(&mut self) -> Option<Vec<String>> {
        match *self.s.first()? {
            b'C' => {
                self.s = &self.s[1..];
                self.disambiguator();
                Some(vec![self.ident()?])
            }
            b'N' => {
                self.s = &self.s[2.min(self.s.len())..];
                let mut path = self.path()?;
                self.disambiguator();
                let ident = self.ident()?;
                if !ident.is_empty() {
                    path.push(ident);
                }
                Some(path)
            }
            _ => None,
        }
    }

    fn disambiguator(&mut self) {
        if self.s.first() == Some(&b's') {
            let end = self
                .s
                .iter()
                .position(|b| *b == b'_')
                .unwrap_or(self.s.len());
            self.s = &self.s[(end + 1).min(self.s.len())..];
        }
    }

    fn ident(&mut self) -> Option<String> {
        if self.s.first() == Some(&b'u') {
            // Punycode identifiers are rare in function paths; give up.
            return None;
        }
        let digits = self.s.iter().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        let len: usize = std::str::from_utf8(&self.s[..digits]).ok()?.parse().ok()?;
        self.s = &self.s[digits..];
        if self.s.first() == Some(&b'_') {
            self.s = &self.s[1..];
        }
        if self.s.len() < len {
            return None;
        }
        let ident = std::str::from_utf8(&self.s[..len]).ok()?.to_string();
        self.s = &self.s[len..];
        Some(ident)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn refs(v: &[OsString]) -> Vec<&OsString> {
        v.iter().collect()
    }

    const P1_MODULE: &str = r#"
        (module
          (import "wasi_snapshot_preview1" "random_get" (func $random_get (param i32 i32) (result i32)))
          (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
          (memory (export "memory") 1)
          (func $_ZN4wasi13lib_generated10random_get17h0123456789abcdefE (param i32 i32) (result i32)
            local.get 0
            local.get 1
            call $random_get)
          (func $_ZN9getrandom3imp15getrandom_inner17hfedcba9876543210E (result i32)
            i32.const 0
            i32.const 16
            call $_ZN4wasi13lib_generated10random_get17h0123456789abcdefE)
          (func (export "_start")
            call $_ZN9getrandom3imp15getrandom_inner17hfedcba9876543210E
            drop)
        )
    "#;

    #[test]
    fn p3_flag_and_preview1_import_warn_and_name_the_import_and_caller() {
        let wasm = wat::parse_str(P1_MODULE).unwrap();
        let f = flags(&["--cooperative-threading", "--gc-sections"]);
        let w = warning(&wasm, &refs(&f)).expect("a warning");
        assert!(
            w.starts_with("wasm32-wasip3 link embeds the wasip1 adapter"),
            "{w}"
        );
        assert!(
            w.contains("random_get (called from wasi::lib_generated::random_get)"),
            "{w}"
        );
        // An import nothing calls is still named, with no caller list.
        assert!(
            w.contains(
                "imports: fd_write, random_get (called from wasi::lib_generated::random_get)\n"
            ),
            "{w}"
        );
        assert!(w.contains("wasi_snapshot_preview1 imports:"), "{w}");
        assert!(
            w.contains("cargo tree -i wasi --target wasm32-wasip3"),
            "{w}"
        );
        assert_eq!(w.lines().count(), 2, "{w}");
    }

    #[test]
    fn a_wasip2_link_does_not_warn() {
        let wasm = wat::parse_str(P1_MODULE).unwrap();
        let f = flags(&["--gc-sections"]);
        assert_eq!(warning(&wasm, &refs(&f)), None);
    }

    #[test]
    fn a_wasi_0_3_import_marks_a_p3_link_without_the_flag() {
        let wasm = wat::parse_str(
            r#"(module
                 (import "wasi:cli/environment@0.3.0" "get-arguments" (func (param i32)))
                 (import "wasi_snapshot_preview1" "random_get" (func (param i32 i32) (result i32))))"#,
        )
        .unwrap();
        let w = warning(&wasm, &[]).expect("a warning");
        assert!(w.contains("import: random_get\n"), "{w}");
    }

    #[test]
    fn a_wasi_0_2_import_alone_is_not_p3() {
        let wasm = wat::parse_str(
            r#"(module
                 (import "wasi:cli/environment@0.2.12" "get-arguments" (func (param i32)))
                 (import "wasi_snapshot_preview1" "random_get" (func (param i32 i32) (result i32))))"#,
        )
        .unwrap();
        assert_eq!(warning(&wasm, &[]), None);
    }

    #[test]
    fn a_p3_link_without_preview1_imports_is_quiet() {
        let wasm = wat::parse_str(
            r#"(module
                 (import "wasi:random/random@0.3.0" "get-random-u64" (func (result i64)))
                 (func (export "_start")))"#,
        )
        .unwrap();
        let f = flags(&["--cooperative-threading"]);
        assert_eq!(warning(&wasm, &refs(&f)), None);
    }

    #[test]
    fn a_stripped_module_names_the_import_only() {
        let wasm = wat::parse_str(
            r#"(module
                 (import "wasi_snapshot_preview1" "random_get" (func $r (param i32 i32) (result i32)))
                 (func (export "f") (result i32) i32.const 0 i32.const 1 call $r))"#,
        )
        .unwrap();
        // `wat` writes a name section only for named functions; the caller
        // here has none, so the warning carries the import alone.
        let f = flags(&["--cooperative-threading"]);
        let w = warning(&wasm, &refs(&f)).unwrap();
        assert!(w.contains("import: random_get\n"), "{w}");
    }

    #[test]
    fn callers_are_capped_at_three() {
        let mut funcs = String::new();
        for i in 0..5 {
            funcs.push_str(&format!(
                "(func $caller{i} (result i32) i32.const 0 i32.const 1 call $r)\n"
            ));
        }
        let wasm = wat::parse_str(format!(
            r#"(module
                 (import "wasi_snapshot_preview1" "random_get" (func $r (param i32 i32) (result i32)))
                 {funcs})"#
        ))
        .unwrap();
        let f = flags(&["--cooperative-threading"]);
        let w = warning(&wasm, &refs(&f)).unwrap();
        assert!(
            w.contains("random_get (called from caller0, caller1, caller2)\n"),
            "{w}"
        );
    }

    #[test]
    fn demangles_legacy_and_v0_paths() {
        assert_eq!(
            demangle("_ZN4wasi13lib_generated10random_get17h0123456789abcdefE"),
            "wasi::lib_generated::random_get"
        );
        assert_eq!(
            demangle("_ZN9getrandom3imp15getrandom_inner17h0123456789abcdefE.llvm.42"),
            "getrandom::imp::getrandom_inner"
        );
        assert_eq!(
            demangle(
                "_ZN4core3ptr42drop_in_place$LT$alloc..string..String$GT$17h0123456789abcdefE"
            ),
            "core::ptr::drop_in_place<alloc::string::String>"
        );
        assert_eq!(
            demangle("_RNvNtCs1234_4wasi13lib_generated10random_get"),
            "wasi::lib_generated::random_get"
        );
        assert_eq!(demangle("random_get"), "random_get");
        assert_eq!(demangle("_RINvCs1_3foo3barpE"), "_RINvCs1_3foo3barpE");
    }
}
