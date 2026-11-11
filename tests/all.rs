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

/// A program that binds `devenv:subprocess@<version>` itself, as a crate
/// built on devenv's guest crate (wit-bindgen with `pub_export_macro`)
/// does: its only type information for the `concurrency` import is the
/// component-type section `section`.
fn program_binding_devenv_subprocess(section: &str, version: &str) -> String {
    let payload = component_type_payload(
        &format!(
            r#"
package devenv:subprocess@{version};

interface concurrency {{
  hint: func() -> u32;
}}

world imports {{
  import concurrency;
}}
"#
        ),
        "imports",
    );
    let bytes = payload
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"
#[link(wasm_import_module = "devenv:subprocess/concurrency@{version}")]
extern "C" {{
    #[link_name = "hint"]
    fn hint() -> i32;
}}

#[link_section = "{section}"]
#[used]
static COMPONENT_TYPE: [u8; {len}] = [{bytes}];

fn main() {{
    std::process::exit(unsafe {{ hint() }});
}}
"#,
        len = payload.len(),
    )
}

// fork: a crate that binds devenv:subprocess itself names its world the way
// std's guest did through rust-1.99-b6 (no suffix after "encoded world").
// Through rust-1.99-b6 this linker dropped every such world, whatever its
// version, and the link failed for want of the import's types.
#[test]
fn keeps_a_users_devenv_subprocess_section_at_any_version() {
    for version in ["0.4.4", "0.4.6", "0.5.0"] {
        let section = format!(
            "component-type:wit-bindgen:0.58.0:devenv:subprocess@{version}:imports-with-all-of-its-exports-removed:encoded world"
        );
        let output = compile(&[], &program_binding_devenv_subprocess(&section, version));
        assert_component(&output);
        let imports = component_import_names(&output);
        let want = format!("devenv:subprocess/concurrency@{version}");
        assert!(imports.contains(&want), "{version}: imports: {imports:?}");
    }
}

/// std's own copy of the devenv-subprocess section name
/// (library/devenv-subprocess-guest built with rustc-dep-of-std).
const STD_DEVENV_SUBPROCESS_SECTION: &str = "component-type:wit-bindgen:0.58.0:devenv:subprocess@0.4.6:imports-with-all-of-its-exports-removed:encoded worldrust-devenv-subprocess-guest-0.4.6-in-libstd";

// fork: with no devenv WIT on `--component-type` (this linker run by hand,
// by a native rustc's `-C linker=`, or by the c multicall's clang over a
// Rust staticlib), std's devenv-subprocess section is the only type
// information for std's devenv imports, so it is kept and the program
// links. Through rust-1.99-b6's usage-error rev (adbce7efd666) it was
// dropped here too: a devenv wasm32-wasip3 std program failed with "failed
// to resolve import `devenv:unix-sockets/sockets@0.3.0::[resource-drop]socket`".
#[test]
fn keeps_std_devenv_subprocess_section_without_the_devenv_wit() {
    let output = compile(
        &[],
        &program_binding_devenv_subprocess(STD_DEVENV_SUBPROCESS_SECTION, "0.4.6"),
    );
    assert_component(&output);
    let imports = component_import_names(&output);
    assert!(
        imports
            .iter()
            .any(|name| name == "devenv:subprocess/concurrency@0.4.6"),
        "imports: {imports:?}"
    );
}

// fork: a session passes the devenv WIT as `--component-type`
// (WASM_COMPONENT_TYPE_WIT_PATHS, rustc on WASI), and then std's copy is
// dropped and the WIT gives the types. Here std's section describes `hint`
// with another result type than the WIT and the program, so the link only
// succeeds if the section is gone.
#[test]
fn strips_std_devenv_subprocess_section_when_the_devenv_wit_is_given() {
    let project = Project::new();
    project.file(
        "subprocess.wit",
        r#"
package devenv:subprocess@0.4.6;

interface concurrency {
  hint: func() -> u32;
}

world imports {
  import concurrency;
}
"#,
    );
    let program = program_binding_devenv_subprocess(STD_DEVENV_SUBPROCESS_SECTION, "0.4.6");
    let stale = component_type_payload(
        r#"
package devenv:subprocess@0.4.6;

interface concurrency {
  hint: func() -> u64;
}

world imports {
  import concurrency;
}
"#,
        "imports",
    );
    let stale_program = swap_component_type_payload(&program, &stale);

    // The stale section alone does not type the program's import.
    let err = format!(
        "{:?}",
        project.try_compile(&[], &stale_program, false).unwrap_err()
    );
    assert!(err.contains("devenv:subprocess/concurrency@0.4.6"), "{err}");

    let output = project.compile(
        &["-Clink-arg=--component-type", "-Clink-arg=subprocess.wit"],
        &stale_program,
    );
    assert_component(&output);
    let imports = component_import_names(&output);
    assert!(
        imports
            .iter()
            .any(|name| name == "devenv:subprocess/concurrency@0.4.6"),
        "imports: {imports:?}"
    );
}

/// A user crate's own devenv:subprocess@0.4.6 section (devenv's guest crate,
/// no `-in-libstd` suffix).
const USER_DEVENV_SUBPROCESS_046_SECTION: &str = "component-type:wit-bindgen:0.58.0:devenv:subprocess@0.4.6:imports-with-all-of-its-exports-removed:encoded world";

/// devenv:subprocess@0.4.6's `imports` world with `hint` returning `result`,
/// as a component-type payload.
fn devenv_subprocess_046_payload(result: &str) -> Vec<u8> {
    component_type_payload(
        &format!(
            r#"
package devenv:subprocess@0.4.6;

interface concurrency {{
  hint: func() -> {result};
}}

world imports {{
  import concurrency;
}}
"#
        ),
        "imports",
    )
}

/// `program` with one more component-type section, `section` holding
/// `payload`.
fn add_component_type_section(program: &str, section: &str, payload: &[u8]) -> String {
    let bytes = payload
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{program}\n#[link_section = \"{section}\"]\n#[used]\nstatic COMPONENT_TYPE_USER: [u8; {}] = [{bytes}];\n",
        payload.len()
    )
}

// fork: with no devenv WIT on `--component-type`, std's devenv-subprocess
// section is kept (above), so it now sits beside a user crate's own
// devenv:subprocess@0.4.6 section, where through rust-1.99-b6's usage-error
// rev std's was dropped. The two describe the same package version; when
// their contents agree the worlds merge and the program links.
#[test]
fn keeps_std_and_a_users_devenv_subprocess_046_sections_together() {
    let program = add_component_type_section(
        &program_binding_devenv_subprocess(STD_DEVENV_SUBPROCESS_SECTION, "0.4.6"),
        USER_DEVENV_SUBPROCESS_046_SECTION,
        &devenv_subprocess_046_payload("u32"),
    );
    let output = compile(&[], &program);
    assert_component(&output);
    let imports = component_import_names(&output);
    assert!(
        imports
            .iter()
            .any(|name| name == "devenv:subprocess/concurrency@0.4.6"),
        "imports: {imports:?}"
    );
}

// fork: the same pair with differing contents for the one package version
// fails the merge, as it does upstream (which keeps every section). Both
// copies of devenv:subprocess@0.4.6 come from one WIT in practice (std's
// library/devenv-subprocess-guest and devenv's guest crate), so this needs
// a package published twice under one version.
#[test]
fn differing_std_and_user_devenv_subprocess_046_sections_fail_the_merge() {
    let program = add_component_type_section(
        &program_binding_devenv_subprocess(STD_DEVENV_SUBPROCESS_SECTION, "0.4.6"),
        USER_DEVENV_SUBPROCESS_046_SECTION,
        &devenv_subprocess_046_payload("u64"),
    );
    let err = format!(
        "{:?}",
        Project::new()
            .try_compile(&[], &program, false)
            .unwrap_err()
    );
    assert!(
        err.contains("failed to merge package `devenv:subprocess@0.4.6` into existing copy"),
        "{err}"
    );
    assert!(err.contains("mismatch in function `hint`"), "{err}");
}

/// `program` (from [`program_binding_devenv_subprocess`]) with its section's
/// bytes replaced by `payload`.
fn swap_component_type_payload(program: &str, payload: &[u8]) -> String {
    let start = program.find("static COMPONENT_TYPE: [u8; ").unwrap();
    let end = start + program[start..].find("];\n").unwrap() + 3;
    let bytes = payload
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{}static COMPONENT_TYPE: [u8; {}] = [{bytes}];\n{}",
        &program[..start],
        payload.len(),
        &program[end..]
    )
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
        .env_remove("DEVENV_WASM_COMPONENT_LD_VALIDATE")
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
    // default, here DEVENV_WASM_COMPONENT_LD_VALIDATE=0).
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
        &[("DEVENV_WASM_COMPONENT_LD_VALIDATE", "0")],
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

    // The escapes write the (invalid) component, as upstream does; the
    // flag's value may also come as the next argument.
    for (extra, vars) in [
        (&[][..], &[("DEVENV_WASM_COMPONENT_LD_VALIDATE", "0")][..]),
        (&["-Clink-arg=--validate-component=false"][..], &[][..]),
        (
            &["-Clink-arg=--validate-component", "-Clink-arg=false"][..],
            &[][..],
        ),
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

#[test]
fn the_validation_escapes_old_name_fails_the_link() {
    // fork: 1.99.1-b6 renamed SETOUT_WASM_COMPONENT_LD_VALIDATE to
    // DEVENV_WASM_COMPONENT_LD_VALIDATE. The old name is never an alias,
    // whatever its value: a link with it fails and names the new setting,
    // even beside the new name.
    let project = Project::new();
    let src = r#"
fn main() {
    println!("hello!");
}
"#;
    for vars in [
        &[("SETOUT_WASM_COMPONENT_LD_VALIDATE", "0")][..],
        &[
            ("SETOUT_WASM_COMPONENT_LD_VALIDATE", "1"),
            ("DEVENV_WASM_COMPONENT_LD_VALIDATE", "1"),
        ][..],
    ] {
        let err = format!("{:?}", link_with_env(&project, &[], src, vars).unwrap_err());
        assert!(
            err.contains("is refused: the setting was renamed to DEVENV_WASM_COMPONENT_LD_VALIDATE in 1.99.1-b6"),
            "{err}"
        );
        assert!(
            err.contains("Details: cargo help devenv renamed-setting"),
            "{err}"
        );
    }
    let bytes = link_with_env(
        &project,
        &[],
        src,
        &[("DEVENV_WASM_COMPONENT_LD_VALIDATE", "1")],
    )
    .unwrap();
    assert_component(&bytes);
}
