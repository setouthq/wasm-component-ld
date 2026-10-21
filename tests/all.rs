use anyhow::{Context, Result, bail};
use std::env;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

fn compile(args: &[&str], src: &str) -> Vec<u8> {
    Project::new().compile(args, src)
}

struct Project {
    tempdir: tempfile::TempDir,
}

impl Project {
    fn new() -> Project {
        Project {
            tempdir: tempfile::TempDir::new().unwrap(),
        }
    }
    fn file(&self, name: &str, contents: &str) {
        fs::write(self.tempdir.path().join(name), contents.as_bytes()).unwrap();
    }

    fn compile(&self, args: &[&str], src: &str) -> Vec<u8> {
        self.try_compile(args, src, true).unwrap()
    }

    fn try_compile(&self, args: &[&str], src: &str, inherit_stderr: bool) -> Result<Vec<u8>> {
        let myself = env!("CARGO_BIN_EXE_wasm-component-ld");
        let mut rustc = Command::new("rustc");
        rustc
            .arg("--target")
            .arg("wasm32-wasip1")
            .arg("-")
            .arg("-o")
            .arg("-")
            .arg("-C")
            .arg(&format!("linker={myself}"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .current_dir(self.tempdir.path());
        if !inherit_stderr {
            rustc.stderr(Stdio::piped());
        }
        let mut rustc = rustc.spawn().context("failed to spawn rustc")?;

        rustc
            .stdin
            .take()
            .context("stdin should be present")?
            .write_all(src.as_bytes())
            .context("failed to write stdin")?;
        let output = rustc
            .wait_with_output()
            .context("failed to wait for subprocess")?;
        if !output.status.success() {
            let mut error = format!("subprocess failed: {:?}", output.status);
            if !output.stdout.is_empty() {
                error.push_str(&format!(
                    "\nstdout: {}",
                    String::from_utf8_lossy(&output.stdout)
                ));
            }
            if !output.stderr.is_empty() {
                error.push_str(&format!(
                    "\nstderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            bail!("{error}")
        }
        Ok(output.stdout)
    }
}

fn assert_component(bytes: &[u8]) {
    assert!(wasmparser::Parser::is_component(&bytes));
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .unwrap();
}

fn assert_module(bytes: &[u8]) {
    assert!(wasmparser::Parser::is_core_wasm(&bytes));
    wasmparser::Validator::new().validate_all(&bytes).unwrap();
}

#[test]
fn empty() {
    let output = compile(&["--crate-type", "cdylib"], "");
    assert_component(&output);
}

#[test]
fn empty_main() {
    let output = compile(&[], "fn main() {}");
    assert_component(&output);
}

#[test]
fn hello_world() {
    let output = compile(
        &[],
        r#"
fn main() {
    println!("hello!");
}
"#,
    );
    assert_component(&output);
}

#[test]
fn cdylib_arbitrary_export() {
    let output = compile(
        &["--crate-type", "cdylib"],
        r#"
#[no_mangle]
pub extern "C" fn foo() {
    println!("x");
}
        "#,
    );
    assert_component(&output);
}

#[test]
fn can_access_badfd() {
    let output = compile(
        &[],
        r#"
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn adapter_open_badfd(fd: &mut u32) -> u32;
}

fn main() {
    let mut fd = 0;
    let rc = unsafe {
        adapter_open_badfd(&mut fd)
    };
    assert_eq!(rc, 0);
    assert_eq!(fd, 3);
}
        "#,
    );
    assert_component(&output);
}

#[test]
fn linker_flags() {
    let output = compile(
        &[
            "-Clink-arg=--max-memory=65536",
            "-Clink-arg=-zstack-size=32",
            "-Clink-arg=--global-base=2048",
            "-Clink-arg=--append-lld-flag=--no-merge-data-segments",
        ],
        r#"
fn main() {
}
        "#,
    );
    assert_component(&output);

    let output = compile(
        &[
            "-Clink-arg=--max-memory=65536",
            "-Clink-arg=-zstack-size=32",
            "-Clink-arg=--global-base=2048",
            "-Clink-arg=--append-lld-flag=--no-merge-data-segments",
            "-Clink-arg=-allow-multiple-definition",
            "-Clink-arg=-soname",
            "-Clink-arg=foo",
        ],
        r#"
fn main() {
}
        "#,
    );
    assert_component(&output);
}

#[test]
fn invalid_lld_flag() {
    let result = Project::new().try_compile(
        &["-Clink-arg=--append-lld-flag=--nonexistent-lld-flag"],
        r#"
fn main() {
    println!("hello!");
}
"#,
        false,
    );
    let err = result.unwrap_err();
    let err = format!("{err:?}");
    println!("error: {err}");
    assert!(err.contains("error: unknown argument: --nonexistent-lld-flag"));
}

#[test]
fn component_type_wit_file() {
    let project = Project::new();
    project.file(
        "foo.wit",
        r#"
package foo:bar;

interface foo {
  bar: func(s: string) -> string;
  baz: get() -> s32;
}

world root {
  import foo;
  export foo;
}
"#,
    );
    let output = project.compile(
        &[
            "-Clink-arg=--component-type",
            "-Clink-arg=foo.wit",
            "-Clink-arg=--string-encoding",
            "-Clink-arg=utf16",
            "--crate-type",
            "cdylib",
        ],
        r#"
#[no_mangle]
pub extern "C" fn cabi_realloc(ptr: *mut u8, old_size: i32, align: i32, new_size: i32) -> *mut u8 {
    _ = (ptr, old_size, align, new_size);
    unreachable!()
}

#[link(wasm_import_module = "foo:bar/foo")]
extern "C" {
    #[link_name = "bar"]
    fn import1(ptr: *mut u8, len: i32, return_ptr: *mut *mut u8);
    #[link_name = "[get]baz"]
    fn import2() -> i32;
}

#[export_name = "foo:bar/foo#bar"]
pub unsafe extern "C" fn export1(ptr: *mut u8, len: i32) -> *mut u8 {
    let mut result = std::ptr::null_mut();
    import1(ptr, len, &mut result);
    result
}

#[export_name = "foo:bar/foo#[get]baz"]
pub unsafe extern "C" fn export2() -> i32 {
    import2()
}
"#,
    );
    assert_component(&output);
}

#[test]
fn skip_component() {
    let output = compile(
        &["-Clink-arg=--skip-wit-component"],
        r#"
fn main() {
}
        "#,
    );
    assert_module(&output);
}

#[test]
fn rustc_using_argfile() {
    let prefix = (0..200).map(|_| 'a').collect::<String>();
    let p = Project {
        tempdir: tempfile::TempDir::with_prefix(&prefix).unwrap(),
    };

    let mut src = String::new();
    for i in 0..1000 {
        src.push_str(&format!("mod m{i};\n"));
        p.file(
            &format!("m{i}.rs"),
            &format!("#[no_mangle] pub extern \"C\" fn f{i}() {{}}"),
        );
    }
    src.push_str("fn main() {}");

    p.file("args", "@args2");
    p.file("args2", "--skip-wit-component");
    let output = p.compile(&["-Ccodegen-units=1000", "-Clink-arg=@args"], &src);
    assert_module(&output);
}

#[test]
fn hello_world_with_realloc_as_memory_grow() {
    let output = compile(
        &["-Clink-arg=--realloc-via-memory-grow"],
        r#"
fn main() {
    println!("hello!");
}
"#,
    );
    assert_component(&output);
}

// The adapter uses legacy names, so this option will always result in an error
// right now.
#[test]
fn legacy_names_currently_required() {
    let result = Project::new().try_compile(
        &["-Clink-arg=--reject-legacy-names"],
        r#"
fn main() {
    println!("hello!");
}
"#,
        false,
    );
    let err = result.unwrap_err();
    let err = format!("{err:?}");
    println!("error: {err}");
    assert!(err.contains("unknown or invalid component model import syntax"));
}

#[test]
fn multiple_flags_accepted() {
    let output = compile(
        &[
            "-Clink-arg=--skip-wit-component",
            "-Clink-arg=--skip-wit-component",
        ],
        r#"
fn main() {
}
        "#,
    );
    assert_module(&output);
}

/// Encodes `world` of `wit` as a wit-bindgen component-type section payload.
fn component_type_payload(wit: &str, world: &str) -> Vec<u8> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("client.wit", wit).unwrap();
    let world = resolve.select_world(&[package], Some(world)).unwrap();
    wit_component::metadata::encode(
        &resolve,
        world,
        wit_component::StringEncoding::UTF8,
        None,
        false,
    )
    .unwrap()
}

/// A program whose only type information for its `my:client/types` import
/// is the component-type section `section`, as a wasi:http client crate's
/// bindings carry theirs.
fn program_with_component_type_section(section: &str) -> String {
    let payload = component_type_payload(
        r#"
package my:client;

interface types {
  resource fields;
}

world client {
  import types;
}
"#,
        "client",
    );
    let bytes = payload
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"
#[link(wasm_import_module = "my:client/types")]
extern "C" {{
    #[link_name = "[resource-drop]fields"]
    fn drop_fields(handle: i32);
}}

#[link_section = "{section}"]
#[used]
static COMPONENT_TYPE: [u8; {len}] = [{bytes}];

fn main() {{
    unsafe {{ drop_fields(std::env::args().count() as i32) }}
}}
"#,
        len = payload.len(),
    )
}

fn component_import_names(bytes: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        if let wasmparser::Payload::ComponentImportSection(reader) = payload.unwrap() {
            for import in reader {
                names.push(import.unwrap().name.name.to_string());
            }
        }
    }
    names
}

// The section name wit-bindgen gives a crate's own world with its exports
// removed (`<world>-with-all-of-its-exports-removed`). A wasi:http client
// crate (wasip2, wasip3, wstd) names its wasi:http world this way, and it
// carries the only type information for the crate's wasi:http imports.
#[test]
fn keeps_a_users_exportless_component_type_section() {
    let output = compile(
        &[],
        &program_with_component_type_section(
            "component-type:wit-bindgen:0.62.0:my:client:client-with-all-of-its-exports-removed:encoded worldmy-client-from-crates-io-client-world",
        ),
    );
    assert_component(&output);
    let imports = component_import_names(&output);
    assert!(
        imports.iter().any(|name| name == "my:client/types"),
        "imports: {imports:?}"
    );
}

// std's own bindings (wasip2/wasip3 built as std's dependency) end their
// section names in `-in-libstd`; those are still stripped, so the same
// program fails to link when its only type information is spelled as std's.
#[test]
fn strips_std_exportless_component_type_section() {
    let result = Project::new().try_compile(
        &[],
        &program_with_component_type_section(
            "component-type:wit-bindgen:0.62.0:my:client:client-with-all-of-its-exports-removed:encoded worldmy-client-from-crates-io-client-world-in-libstd",
        ),
        false,
    );
    let err = format!("{:?}", result.unwrap_err());
    assert!(
        err.contains("module requires an import interface named `my:client/types`"),
        "{err}"
    );
}

/// fork: links `src` (wasm32-wasip1, through this linker) to `out.wasm` in
/// `project`'s directory with `vars` set for the link, and returns the bytes
/// or the failure with rustc's stderr.
fn link_with_env(
    project: &Project,
    args: &[&str],
    src: &str,
    vars: &[(&str, &str)],
) -> Result<Vec<u8>> {
    let myself = env!("CARGO_BIN_EXE_wasm-component-ld");
    let out = project.tempdir.path().join("out.wasm");
    let _ = fs::remove_file(&out);
    let mut rustc = Command::new("rustc");
    rustc
        .arg("--target")
        .arg("wasm32-wasip1")
        .arg("-")
        .arg("-o")
        .arg(&out)
        .arg("-C")
        .arg(&format!("linker={myself}"))
        .args(args)
        .env_remove("SETOUT_WASM_COMPONENT_LD_VALIDATE")
        .envs(vars.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(project.tempdir.path());
    let mut child = rustc.spawn().context("failed to spawn rustc")?;
    child
        .stdin
        .take()
        .context("stdin should be present")?
        .write_all(src.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "subprocess failed: {:?}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(fs::read(&out)?)
}

#[test]
fn validation_leaves_a_valid_link_byte_identical() {
    // fork: the default validation only reads the component, so a valid
    // link writes the same bytes as with validation off (upstream's
    // default, here SETOUT_WASM_COMPONENT_LD_VALIDATE=0).
    let project = Project::new();
    let src = r#"
fn main() {
    println!("hello!");
}
"#;
    let validated = link_with_env(&project, &[], src, &[]).unwrap();
    let unvalidated = link_with_env(
        &project,
        &[],
        src,
        &[("SETOUT_WASM_COMPONENT_LD_VALIDATE", "0")],
    )
    .unwrap();
    assert_component(&validated);
    assert!(validated == unvalidated, "validation changed the output");
}

const SYNC_API_WIT: &str = r#"
package test:sync;

interface api {
  g: func(x: u32);
}

world root {
  import api;
}
"#;

// `g` is a sync WIT function; importing it as `[async-lower]g` asks for an
// async lowering, which no runtime accepts.
const ASYNC_LOWER_OF_SYNC_SRC: &str = r#"
#[link(wasm_import_module = "test:sync/api")]
extern "C" {
    #[link_name = "[async-lower]g"]
    fn g_async(x: i32) -> i32;
}

fn main() {
    unsafe {
        g_async(1);
    }
}
"#;

#[test]
fn an_async_lowered_sync_import_fails_at_link_time() {
    let project = Project::new();
    project.file("api.wit", SYNC_API_WIT);
    let args = ["-Clink-arg=--component-type", "-Clink-arg=api.wit"];

    // Default: the link fails and says why, naming the import.
    let err = link_with_env(&project, &args, ASYNC_LOWER_OF_SYNC_SRC, &[]).unwrap_err();
    let err = format!("{err:?}");
    println!("error: {err}");
    assert!(err.contains("the linked component is invalid"), "{err}");
    assert!(
        err.contains("the `async` canonical option requires an async function type"),
        "{err}"
    );
    assert!(
        err.contains("in the lowering of `test:sync/api#g`"),
        "{err}"
    );
    assert!(!project.tempdir.path().join("out.wasm").exists());

    // The escapes write the (invalid) component, as upstream does.
    for (extra, vars) in [
        (None, &[("SETOUT_WASM_COMPONENT_LD_VALIDATE", "0")][..]),
        (Some("-Clink-arg=--validate-component=false"), &[][..]),
    ] {
        let mut args = args.to_vec();
        args.extend(extra);
        let bytes = link_with_env(&project, &args, ASYNC_LOWER_OF_SYNC_SRC, vars).unwrap();
        assert!(wasmparser::Parser::is_component(&bytes));
        assert!(
            wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
                .validate_all(&bytes)
                .is_err()
        );
    }
}
