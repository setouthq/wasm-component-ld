//! fork: every component this linker writes is validated first, by default.
//!
//! wit-component can encode a component that no runtime will load (an
//! async-lowered import of a sync WIT function is one: the `async` canonical
//! option needs an async function type), and upstream wasm-component-ld
//! writes it without a word, because its `--validate-component` defaults to
//! `false`. The broken file then fails at load time on most hosts and runs
//! on others. Here the output is validated before it is written, so such a
//! link fails, with the validator's message and, where it can be found, the
//! import or core module the failure belongs to.
//!
//! An explicit `--validate-component[=true|false]` decides. Without one,
//! `SETOUT_WASM_COMPONENT_LD_VALIDATE=0` turns validation off; anything else,
//! or no variable, keeps it on.

use anyhow::{Result, bail};
use std::ffi::OsStr;
use std::path::Path;
use wasmparser::{
    CanonicalFunction, ComponentAlias, ComponentExternalKind, ComponentTypeRef, Parser, Payload,
    Validator, WasmFeatures,
};

/// The environment escape: `0` writes components without validating them.
pub(crate) const ENV: &str = "SETOUT_WASM_COMPONENT_LD_VALIDATE";

/// Whether this link validates its output: the `--validate-component` flag
/// when given (a bare flag means `true`), else on unless `ENV` is `0`.
pub(crate) fn enabled(flag: Option<Option<bool>>, env: Option<&OsStr>) -> bool {
    match flag {
        Some(value) => value.unwrap_or(true),
        None => env != Some(OsStr::new("0")),
    }
}

/// Validates `component` (the bytes about to be written to `output`) with
/// the validator wit-component's own `validate` option uses.
pub(crate) fn check(component: &[u8], output: &Path) -> Result<()> {
    let Err(err) = Validator::new_with_features(WasmFeatures::all()).validate_all(component) else {
        return Ok(());
    };
    let place = match locate(component, err.offset()) {
        Some(place) => format!(", in {place}"),
        None => String::new(),
    };
    bail!(
        "the linked component is invalid: {err}{place}\n\
         note: wasm-component-ld validates each component before writing it, and \
         this one would fail to load, so {output:?} was not written; set {ENV}=0 \
         or pass --validate-component=false to write it anyway"
    )
}

/// Best effort: what the component holds at `offset`, for the error. Finds
/// a canonical function at the top level (naming the function a `canon
/// lower` lowers when the import or alias that brought it in has a name) or
/// a top-level core module. `None` when nothing more than the offset is
/// known.
pub(crate) fn locate(component: &[u8], offset: u64) -> Option<String> {
    // Names of the top-level component instance and function index spaces,
    // as far as they are known.
    let mut instances: Vec<Option<String>> = Vec::new();
    let mut funcs: Vec<Option<String>> = Vec::new();
    let mut modules = 0usize;
    // Nesting depth: payloads of nested modules and components come inline.
    let mut depth = 0usize;

    for payload in Parser::new(0).parse_all(component) {
        let payload = payload.ok()?;
        match payload {
            Payload::ModuleSection {
                unchecked_range, ..
            } => {
                if depth == 0 {
                    if unchecked_range.contains(&offset) {
                        return Some(format!("core module {modules}"));
                    }
                    modules += 1;
                }
                depth += 1;
            }
            Payload::ComponentSection {
                unchecked_range, ..
            } => {
                if depth == 0 && unchecked_range.contains(&offset) {
                    return Some("a nested component".to_string());
                }
                depth += 1;
            }
            Payload::End(_) => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
            }
            _ if depth > 0 => {}
            Payload::ComponentImportSection(reader) => {
                for import in reader {
                    let import = import.ok()?;
                    let name = Some(import.name.full_name().into_owned());
                    match import.ty {
                        ComponentTypeRef::Instance(_) => instances.push(name),
                        ComponentTypeRef::Func(_) => funcs.push(name),
                        _ => {}
                    }
                }
            }
            Payload::ComponentInstanceSection(reader) => {
                for instance in reader {
                    instance.ok()?;
                    instances.push(None);
                }
            }
            Payload::ComponentAliasSection(reader) => {
                for alias in reader {
                    if let ComponentAlias::InstanceExport {
                        kind,
                        instance_index,
                        name,
                    } = alias.ok()?
                    {
                        let qualified = match instances.get(instance_index as usize) {
                            Some(Some(instance)) => Some(format!("{instance}#{name}")),
                            _ => None,
                        };
                        match kind {
                            ComponentExternalKind::Func => funcs.push(qualified),
                            ComponentExternalKind::Instance => instances.push(qualified),
                            _ => {}
                        }
                    }
                }
            }
            Payload::ComponentExportSection(reader) => {
                for export in reader {
                    let export = export.ok()?;
                    let name = Some(export.name.full_name().into_owned());
                    match export.kind {
                        ComponentExternalKind::Func => funcs.push(name),
                        ComponentExternalKind::Instance => instances.push(name),
                        _ => {}
                    }
                }
            }
            Payload::ComponentCanonicalSection(reader) => {
                let end = reader.range().end;
                let entries = reader
                    .into_iter_with_offsets()
                    .collect::<wasmparser::Result<Vec<_>>>()
                    .ok()?;
                for (i, (start, func)) in entries.iter().enumerate() {
                    let next = entries.get(i + 1).map_or(end, |(next, _)| *next);
                    let here = (*start..next).contains(&offset);
                    match func {
                        CanonicalFunction::Lower { func_index, .. } if here => {
                            return Some(match funcs.get(*func_index as usize) {
                                Some(Some(name)) => format!("the lowering of `{name}`"),
                                _ => format!("the lowering of component function {func_index}"),
                            });
                        }
                        CanonicalFunction::Lift {
                            core_func_index, ..
                        } => {
                            if here {
                                return Some(format!(
                                    "the lifting of core function {core_func_index}"
                                ));
                            }
                            funcs.push(None);
                        }
                        other if here => {
                            let debug = format!("{other:?}");
                            let kind = debug.split([' ', '{', '(']).next().unwrap_or(&debug);
                            return Some(format!("the canonical built-in `{kind}`"));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{ENV, check, enabled, locate};
    use std::ffi::OsStr;
    use std::path::Path;

    /// A component that lowers `test:sync/api#g`, a sync function, with the
    /// `async` option: what an `[async-lower]g` import becomes.
    const ASYNC_LOWER_OF_SYNC: &str = r#"
        (component
          (import "test:sync/api" (instance $api
            (export "g" (func (param "x" u32)))))
          (alias export $api "g" (func $g))
          (core module $m (memory (export "memory") 1))
          (core instance $i (instantiate $m))
          (alias core export $i "memory" (core memory $mem))
          (core func (canon lower (func $g) async (memory $mem))))
    "#;

    #[test]
    fn the_flag_decides_then_the_environment() {
        let zero = Some(OsStr::new("0"));
        let one = Some(OsStr::new("1"));
        assert!(enabled(None, None));
        assert!(enabled(None, one));
        assert!(enabled(None, Some(OsStr::new(""))));
        assert!(!enabled(None, zero));
        assert!(enabled(Some(None), zero));
        assert!(enabled(Some(Some(true)), zero));
        assert!(!enabled(Some(Some(false)), None));
        assert!(!enabled(Some(Some(false)), one));
    }

    #[test]
    fn an_async_lower_of_a_sync_function_is_named() {
        let component = wat::parse_str(ASYNC_LOWER_OF_SYNC).unwrap();
        let err = format!(
            "{:#}",
            check(&component, Path::new("out.wasm")).unwrap_err()
        );
        assert!(
            err.contains("the `async` canonical option requires an async function type"),
            "{err}"
        );
        assert!(
            err.contains("in the lowering of `test:sync/api#g`"),
            "{err}"
        );
        assert!(err.contains(&format!("{ENV}=0")), "{err}");
        assert!(err.contains("--validate-component=false"), "{err}");
        assert!(err.contains("\"out.wasm\" was not written"), "{err}");
    }

    #[test]
    fn a_valid_component_passes() {
        let valid = ASYNC_LOWER_OF_SYNC.replace(" async ", " ");
        let component = wat::parse_str(&valid).unwrap();
        check(&component, Path::new("out.wasm")).unwrap();
    }

    #[test]
    fn an_invalid_core_module_is_named_by_its_index() {
        let component = wat::parse_str(
            r#"(component
                 (core module)
                 (core module (func (result i32) i64.const 0)))"#,
        )
        .unwrap();
        let err = format!(
            "{:#}",
            check(&component, Path::new("out.wasm")).unwrap_err()
        );
        assert!(err.contains("type mismatch"), "{err}");
        assert!(err.contains("in core module 1"), "{err}");
    }

    #[test]
    fn an_offset_outside_any_known_item_names_nothing() {
        let component = wat::parse_str(ASYNC_LOWER_OF_SYNC).unwrap();
        assert_eq!(locate(&component, 0), None);
    }
}
