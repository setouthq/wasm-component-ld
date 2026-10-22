use anyhow::{Context, Result, bail};
use clap::{ArgAction, CommandFactory, FromArgMatches};
use clap_lex::OsStrExt;
use lexopt::Arg;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::str::FromStr;
use wasmparser::Payload;
use wit_component::StringEncoding;
use wit_parser::{Resolve, WorldId};

mod argfile;
mod p3_adapter;
mod validate;

/// Representation of a flag passed to `wasm-ld`
///
/// Note that the parsing of flags in `wasm-ld` is not as uniform as parsing
/// arguments via `clap`. For example if `--foo bar` is supported that doesn't
/// mean that `--foo=bar` is supported. Similarly some options such as `--foo`
/// support optional values as `--foo=bar` but can't be specified as
/// `--foo bar`.
///
/// Finally there's currently only one "weird" flag which is `-shared` which has
/// a single dash but a long name. That's specially handled elsewhere.
///
/// The general goal here is that we want to inherit `wasm-ld`'s CLI but also
/// want to be able to reserve CLI flags for this linker itself, so `wasm-ld`'s
/// arguments are parsed where our own are intermixed.
struct LldFlag {
    clap_name: &'static str,
    long: Option<&'static str>,
    short: Option<char>,
    value: FlagValue,
    nonstandard: bool,
}

impl LldFlag {
    const fn nonstandard(self) -> Self {
        LldFlag {
            nonstandard: true,
            ..self
        }
    }
}

enum FlagValue {
    /// This option has no value, e.g. `-f` or `--foo`
    None,

    /// This option's value must be specified with `=`, for example `--foo=bar`
    RequiredEqual(&'static str),

    /// This option's value must be specified with ` `, for example `--foo bar`.
    ///
    /// I think that `wasm-ld` supports both `-f foo` and `-ffoo` for
    /// single-character flags, but I haven't tested as putting a space seems to
    /// work.
    RequiredSpace(&'static str),

    /// This option's value is optional but if specified it must use an `=` for
    /// example `--foo=bar` or `--foo`.
    Optional(&'static str),
}

/// This is a large macro which is intended to take CLI-looking syntax and turn
/// each individual flag into a `LldFlag` specified above.
macro_rules! flag {
    // Long options specified as:
    //
    //     -f / --foo
    //
    // or just
    //
    //     --foo
    //
    // Options can look like `--foo`, `--foo=bar`, `--foo[=bar]`, or
    // `--foo bar` to match the kinds of flags that LLD supports.
    ($(-$short:ident /)? --$($flag:tt)*) => {
        LldFlag {
            clap_name: concat!("long_", $(stringify!($flag),)*),
            long: Some(flag!(@name [] $($flag)*)),
            short: flag!(@short $($short)?),
            value: flag!(@value $($flag)*),
            nonstandard: false,
        }
    };

    // Short options specified as `-f` or `-f foo`.
    (-$flag:tt $($val:tt)*) => {
        LldFlag {
            clap_name: concat!("short_", stringify!($flag)),
            long: None,
            short: Some(flag!(@char $flag)),
            value: flag!(@value $flag $($val)*),
            nonstandard: false,
        }
    };

    // Generates the long name of a flag, collected within the `[]` argument to
    // this macro. This will iterate over the flag given as the rest of the
    // macro arguments and collect values into `[...]` and recurse.
    //
    // The first recursion case handles `foo-bar-baz=..` where Rust tokenizes
    // this as `foo` then `-` then `bar` then ... If this is found then `foo-`
    // is added to the name and then the macro recurses.
    (@name [$($name:tt)*] $n:ident-$($rest:tt)*) => (flag!(@name [$($name)* $n-] $($rest)*));
    // These are the ways options are represented, either `--foo bar`,
    // `--foo=bar`, `--foo=bar`, or `--foo`. In all these cases discard the
    // value itself and then recurse.
    (@name [$($name:tt)*] $n:ident $_value:ident) => (flag!(@name [$($name)* $n]));
    (@name [$($name:tt)*] $n:ident=$_value:ident) => (flag!(@name [$($name)* $n]));
    (@name [$($name:tt)*] $n:ident[=$_value:ident]) => (flag!(@name [$($name)* $n]));
    (@name [$($name:tt)*] $n:ident) => (flag!(@name [$($name)* $n]));
    // If there's nothing left then the `$name` has collected everything so
    // it's stringifyied and caoncatenated.
    (@name [$($name:tt)*]) => (concat!($(stringify!($name),)*));

    // This parses the value-style of the flag given. The recursion here looks
    // similar to `@name` above. except that the four terminal cases all
    // correspond to different variants of `FlagValue`.
    (@value $n:ident - $($rest:tt)*) => (flag!(@value $($rest)*));
    (@value $_flag:ident = $name:ident) => (FlagValue::RequiredEqual(stringify!($name)));
    (@value $_flag:ident $name:ident) => (FlagValue::RequiredSpace(stringify!($name)));
    (@value $_flag:ident [= $name:ident]) => (FlagValue::Optional(stringify!($name)));
    (@value $_flag:ident) => (FlagValue::None);

    // Helper for flags that have both a long and a short form to parse whether
    // a short form was provided.
    (@short) => (None);
    (@short $name:ident) => (Some(flag!(@char $name)));

    // Helper for getting the `char` of a short flag.
    (@char $name:ident) => ({
        let name = stringify!($name);
        assert!(name.len() == 1);
        name.as_bytes()[0] as char
    });
}

const LLD_FLAGS: &[LldFlag] = &[
    flag! { --allow-multiple-definition }.nonstandard(),
    flag! { --allow-undefined-file=PATH }.nonstandard(),
    flag! { --allow-undefined }.nonstandard(),
    flag! { --Bdynamic }.nonstandard(),
    flag! { --Bstatic }.nonstandard(),
    flag! { --Bsymbolic }.nonstandard(),
    flag! { --build-id[=VAL] }.nonstandard(),
    flag! { --call_shared }.nonstandard(),
    flag! { --check-features },
    flag! { --color-diagnostics[=VALUE] }.nonstandard(),
    flag! { --compress-relocations }.nonstandard(),
    flag! { --cooperative-threading },
    flag! { --demangle }.nonstandard(),
    flag! { --dn }.nonstandard(),
    flag! { --dy }.nonstandard(),
    flag! { --emit-relocs }.nonstandard(),
    flag! { --end-lib }.nonstandard(),
    flag! { --entry SYM }.nonstandard(),
    flag! { --error-limit=N },
    flag! { --error-unresolved-symbols }.nonstandard(),
    flag! { --experimental-pic },
    flag! { --export-all },
    flag! { -E / --export-dynamic }.nonstandard(),
    flag! { --export-if-defined=SYM }.nonstandard(),
    flag! { --export-memory[=NAME] },
    flag! { --export-table },
    flag! { --export=SYM }.nonstandard(),
    flag! { --extra-features=LIST }.nonstandard(),
    flag! { --fatal-warnings }.nonstandard(),
    flag! { --features=LIST }.nonstandard(),
    flag! { --gc-sections }.nonstandard(),
    flag! { --global-base=VALUE },
    flag! { --growable-table },
    flag! { --import-memory[=NAME] },
    flag! { --import-table },
    flag! { --import-undefined }.nonstandard(),
    flag! { --initial-heap=SIZE },
    flag! { --initial-memory=SIZE },
    flag! { --keep-section=NAME }.nonstandard(),
    flag! { --lto-CGO=LEVEL },
    flag! { --lto-debug-pass-manager },
    flag! { --lto-O=LEVEL },
    flag! { --lto-partitions=NUM },
    flag! { -L PATH },
    flag! { -l LIB },
    flag! { --Map=FILE }.nonstandard(),
    flag! { --max-memory=SIZE },
    flag! { --merge-data-segments },
    flag! { --mllvm=FLAG }.nonstandard(),
    flag! { -m ARCH },
    flag! { --no-allow-multiple-definition }.nonstandard(),
    flag! { --no-check-features },
    flag! { --no-color-diagnostics }.nonstandard(),
    flag! { --no-demangle }.nonstandard(),
    flag! { --no-entry }.nonstandard(),
    flag! { --no-export-dynamic }.nonstandard(),
    flag! { --no-fatal-warnings }.nonstandard(),
    flag! { --no-gc-sections }.nonstandard(),
    flag! { --no-growable-memory },
    flag! { --no-merge-data-segments },
    flag! { --no-pie }.nonstandard(),
    flag! { --no-print-gc-sections }.nonstandard(),
    flag! { --no-stack-first }.nonstandard(),
    flag! { --no-shlib-sigcheck },
    flag! { --no-whole-archive }.nonstandard(),
    flag! { --noinhibit-exec }.nonstandard(),
    flag! { --non_shared }.nonstandard(),
    flag! { -O LEVEL },
    flag! { --page-size=VALUE },
    flag! { --pie }.nonstandard(),
    flag! { --print-gc-sections }.nonstandard(),
    flag! { -M / --print-map }.nonstandard(),
    flag! { --relocatable }.nonstandard(),
    flag! { --reproduce=VALUE },
    flag! { --rpath=VALUE }.nonstandard(),
    flag! { --save-temps }.nonstandard(),
    flag! { --shared-memory },
    flag! { --shared }.nonstandard(),
    flag! { --soname=VALUE }.nonstandard(),
    flag! { --stack-first }.nonstandard(),
    flag! { --start-lib }.nonstandard(),
    flag! { --static }.nonstandard(),
    flag! { -s / --strip-all }.nonstandard(),
    flag! { -S / --strip-debug }.nonstandard(),
    flag! { --table-base=VALUE },
    flag! { --thinlto-cache-dir=PATH },
    flag! { --thinlto-cache-policy=VALUE },
    flag! { --thinlto-jobs=N },
    flag! { --threads=N }.nonstandard(),
    flag! { -y / --trace-symbol=SYM }.nonstandard(),
    flag! { -t / --trace }.nonstandard(),
    flag! { --undefined=SYM }.nonstandard(),
    flag! { --unresolved-symbols=VALUE }.nonstandard(),
    flag! { --warn-unresolved-symbols }.nonstandard(),
    flag! { --whole-archive }.nonstandard(),
    flag! { --why-extract=MEMBER },
    flag! { --wrap=VALUE }.nonstandard(),
    flag! { -z OPT },
];

#[derive(Default)]
struct App {
    component: ComponentLdArgs,
    lld_args: Vec<OsString>,
    /// fork: `SETOUT_WASM_COMPONENT_LD_VALIDATE` as it was when the
    /// arguments were parsed (see `validate::enabled`). Read once, with the
    /// arguments, so that a test can set it without the process environment.
    validate_env: Option<OsString>,
}

/// An invocation of the inner LLD linker that `wasm-component-ld` normally
/// spawns as a child process.
pub struct LldInvocation<'a> {
    exe: &'a Path,
    needs_flavor: bool,
    verbose: bool,
    output: Option<&'a Path>,
    args: &'a [&'a OsString],
}

impl LldInvocation<'_> {
    pub fn argv(&self) -> Vec<OsString> {
        let mut argv = Vec::new();
        argv.push(self.exe.as_os_str().to_owned());
        if self.needs_flavor {
            argv.push("-flavor".into());
            argv.push("wasm".into());
        }
        argv.extend(self.args.iter().map(|arg| (*arg).clone()));
        if self.verbose {
            argv.push("--verbose".into());
        }
        if let Some(output) = &self.output {
            argv.push("-o".into());
            argv.push(output.as_os_str().to_owned());
        }
        argv
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(self.exe);
        if self.needs_flavor {
            cmd.arg("-flavor").arg("wasm");
        }
        cmd.args(self.args);
        if self.verbose {
            cmd.arg("--verbose");
        }
        if let Some(output) = &self.output {
            cmd.arg("-o").arg(output);
        }
        cmd
    }
}

pub enum LldStatus {
    Process(ExitStatus),
    Code(i32),
}

impl LldStatus {
    pub fn from_code(code: i32) -> Self {
        Self::Code(code)
    }

    fn success(&self) -> bool {
        match self {
            Self::Process(status) => status.success(),
            Self::Code(code) => *code == 0,
        }
    }
}

impl From<ExitStatus> for LldStatus {
    fn from(status: ExitStatus) -> Self {
        Self::Process(status)
    }
}

impl fmt::Display for LldStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Process(status) => status.fmt(f),
            Self::Code(code) => write!(f, "exit status: {code}"),
        }
    }
}

/// A linker to create a Component from input object files and libraries.
///
/// This application is an equivalent of `wasm-ld` except that it produces a
/// component instead of a core wasm module. This application behaves very
/// similarly to `wasm-ld` in that it takes the same inputs and flags, and it
/// will internally invoke `wasm-ld`. After `wasm-ld` has been invoked the core
/// wasm module will be turned into a component using component tooling and
/// embedded information in the core wasm module.
#[derive(clap::Parser, Default)]
#[command(version, args_override_self = true)]
struct ComponentLdArgs {
    /// Which default WASI adapter, if any, to use when creating the output
    /// component.
    #[clap(long, name = "command|reactor|proxy|none")]
    wasi_adapter: Option<WasiAdapter>,

    /// Location of where to find `wasm-ld`.
    ///
    /// If not specified this is automatically detected.
    #[clap(long, name = "PATH")]
    wasm_ld_path: Option<PathBuf>,

    /// Quoting syntax for response files.
    #[clap(long, name = "STYLE")]
    rsp_quoting: Option<String>,

    /// Where to place the component output.
    #[clap(short, long)]
    output: PathBuf,

    /// Print verbose output.
    #[clap(short, long)]
    verbose: bool,

    /// Whether or not the output component is validated.
    ///
    /// This defaults to `true` (fork: upstream defaults to `false`). Without
    /// this flag, `SETOUT_WASM_COMPONENT_LD_VALIDATE=0` in the environment
    /// turns validation off.
    #[clap(long, require_equals = true, value_name = "true|false")]
    validate_component: Option<Option<bool>>,

    /// Whether or not imports are deduplicated based on semver in the final
    /// component.
    ///
    /// This defaults to `true`.
    #[clap(long, require_equals = true, value_name = "true|false")]
    merge_imports_based_on_semver: Option<Option<bool>>,

    /// Adapters to use when creating the final component.
    #[clap(long = "adapt", value_name = "[NAME=]MODULE", value_parser = parse_adapter)]
    adapters: Vec<(String, Vec<u8>)>,

    /// Whether or not "legacy" names are rejected during componentization.
    ///
    /// This option can be used to require the naming scheme outlined in
    /// <https://github.com/WebAssembly/component-model/pull/378> to be used
    /// and rejects all modules using the previous ad-hoc naming scheme.
    ///
    /// This defaults to `false`.
    #[clap(long)]
    reject_legacy_names: bool,

    /// Whether or not the `cabi_realloc` function used by the adapter is backed
    /// by `memory.grow`.
    ///
    /// By default the adapter will import `cabi_realloc` from the main module
    /// and use that, but this can be used to instead back memory allocation
    /// requests with `memory.grow` instead.
    ///
    /// This defaults to `false`.
    #[clap(long)]
    realloc_via_memory_grow: bool,

    /// WIT file or directory representing additional component type information to use.
    ///
    /// A world can be selected explicitly with `PATH#WORLD`, which is useful
    /// for WIT packages that define more than one world.
    ///
    /// May be specified more than once.
    ///
    /// See also the `--string-encoding` option.
    #[clap(long = "component-type", value_name = "WIT_PATH")]
    component_types: Vec<String>,

    /// String encoding to use when creating the final component.
    ///
    /// This may be either "utf8", "utf16", or "compact-utf16".  This value is
    /// only used when one or more `--component-type` options are specified.
    #[clap(long, value_parser = parse_encoding, default_value = "utf8")]
    string_encoding: StringEncoding,

    /// Skip the `wit-component`-based process to generate a component.
    #[clap(long)]
    skip_wit_component: bool,

    /// Raw flags to pass to the end of the `lld` invocation in case they're
    /// not already recognized by this wrapper executable.
    #[clap(long)]
    append_lld_flag: Vec<OsString>,

    /// Whether or not to use `return_call_ref` in the generated component.
    #[clap(long)]
    return_call_ref: bool,
}

fn parse_adapter(s: &str) -> Result<(String, Vec<u8>)> {
    let (name, path) = parse_optionally_name_file(s);
    let wasm = wat::parse_file(path)?;
    Ok((name.to_string(), wasm))
}

fn parse_encoding(s: &str) -> Result<StringEncoding> {
    Ok(match s {
        "utf8" => StringEncoding::UTF8,
        "utf16" => StringEncoding::UTF16,
        "compact-utf16" => StringEncoding::CompactUTF16,
        _ => bail!("unknown string encoding: {s:?}"),
    })
}

fn component_type_path_and_world(component_type: &str) -> (PathBuf, Option<String>) {
    match component_type.rsplit_once('#') {
        Some((path, world)) if !path.is_empty() && !world.is_empty() => {
            (PathBuf::from(path), Some(world.to_owned()))
        }
        _ => (PathBuf::from(component_type), None),
    }
}

fn parse_optionally_name_file(s: &str) -> (&str, &str) {
    let mut parts = s.splitn(2, '=');
    let name_or_path = parts.next().unwrap();
    match parts.next() {
        Some(path) => (name_or_path, path),
        None => {
            let name = Path::new(name_or_path)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap();
            let name = match name.find('.') {
                Some(i) => &name[..i],
                None => name,
            };
            (name, name_or_path)
        }
    }
}

#[derive(Debug, Copy, Clone)]
enum WasiAdapter {
    Command,
    Reactor,
    Proxy,
    None,
}

impl FromStr for WasiAdapter {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(WasiAdapter::None),
            "command" => Ok(WasiAdapter::Command),
            "reactor" => Ok(WasiAdapter::Reactor),
            "proxy" => Ok(WasiAdapter::Proxy),
            _ => bail!("unknown wasi adapter {s}, must be one of: none, command, reactor, proxy"),
        }
    }
}

pub fn main() {
    let err = match run() {
        Ok(()) => return,
        Err(e) => e,
    };
    eprintln!("error: {err}");
    if err.chain().len() > 1 {
        eprintln!("\nCaused by:");
        for (i, err) in err.chain().skip(1).enumerate() {
            eprintln!("{i:>5}: {}", err.to_string().replace("\n", "\n       "));
        }
    }

    std::process::exit(1);
}

fn run() -> Result<()> {
    App::parse()?.run()
}

/// Prints the link's warnings the way a linker does: `warning: ` before each.
fn print_warnings(warnings: &[String]) {
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
}

pub fn run_from_args_with_lld<F>(args: impl IntoIterator<Item = OsString>, run_lld: F) -> Result<()>
where
    F: for<'a> FnMut(LldInvocation<'a>) -> Result<LldStatus>,
{
    let warnings = App::parse_from(args)?.run_with_lld(run_lld)?;
    print_warnings(&warnings);
    Ok(())
}

/// fork: like [`run_from_args_with_lld`], but returns the link's warnings
/// instead of printing them, so a caller that links in-process (rustc on a
/// WASI host) reports them as it reports a spawned linker's stderr. Each
/// warning is one message without a `warning: ` prefix; a second line, when
/// present, starts with `note: `.
pub fn run_from_args_with_lld_warnings<F>(
    args: impl IntoIterator<Item = OsString>,
    run_lld: F,
) -> Result<Vec<String>>
where
    F: for<'a> FnMut(LldInvocation<'a>) -> Result<LldStatus>,
{
    App::parse_from(args)?.run_with_lld(run_lld)
}

impl App {
    /// Parse the CLI arguments into an `App` to run the linker.
    ///
    /// This is unfortunately nontrivial because the way `wasm-ld` takes
    /// arguments is not compatible with `clap`. Namely flags like
    /// `--whole-archive` are positional are processed in a stateful manner.
    /// This means that the relative ordering of flags to `wasm-ld` needs to be
    /// preserved. Additionally there are flags like `-shared` which clap does
    /// not support.
    ///
    /// To handle this the `lexopt` crate is used to perform low-level argument
    /// parsing. That's then used to determine whether the argument is intended
    /// for `wasm-component-ld` or `wasm-ld`, so arguments are filtered into two
    /// lists. Using these lists the arguments to `wasm-component-ld` are then
    /// parsed. On failure a help message is presented with all `wasm-ld`
    /// arguments added as well.
    ///
    /// This means that functionally it looks like `clap` parses everything when
    /// in fact `lexopt` is used to filter out `wasm-ld` arguments and `clap`
    /// only parses arguments specific to `wasm-component-ld`.
    fn parse() -> Result<App> {
        App::parse_from(std::env::args_os())
    }

    fn parse_from(args: impl IntoIterator<Item = OsString>) -> Result<App> {
        let mut args = argfile::expand_from(args).context("failed to expand @-response files")?;
        let argv0 = args
            .first()
            .cloned()
            .unwrap_or_else(|| OsString::from("wasm-component-ld"));

        // First remove `-flavor wasm` in case this is invoked as a generic LLD
        // driver. We can safely ignore that going forward.
        if let Some([flavor, wasm]) = args.get(1..3) {
            if flavor == "-flavor" && wasm == "wasm" {
                args.remove(1);
                args.remove(1);
            }
        }

        let mut command = ComponentLdArgs::command();
        let mut lld_args = Vec::new();
        let mut component_ld_args = vec![argv0];
        let mut parser = lexopt::Parser::from_iter(args);

        fn handle_lld_arg(
            lld: &LldFlag,
            parser: &mut lexopt::Parser,
            lld_args: &mut Vec<OsString>,
        ) -> Result<()> {
            let mut arg = OsString::new();
            match (lld.short, lld.long) {
                (_, Some(long)) => {
                    arg.push("--");
                    arg.push(long);
                }
                (Some(short), _) => {
                    arg.push("-");
                    arg.push(short.encode_utf8(&mut [0; 5]));
                }
                (None, None) => unreachable!(),
            }
            match lld.value {
                FlagValue::None => {
                    lld_args.push(arg);
                }

                FlagValue::RequiredSpace(_) => {
                    lld_args.push(arg);
                    lld_args.push(parser.value()?);
                }

                FlagValue::RequiredEqual(_) => {
                    arg.push("=");
                    arg.push(&parser.value()?);
                    lld_args.push(arg);
                }

                // If the value is optional then the argument must have an `=`
                // in the argument itself.
                FlagValue::Optional(_) => {
                    match parser.optional_value() {
                        Some(val) => {
                            arg.push("=");
                            arg.push(&val);
                        }
                        None => {}
                    }
                    lld_args.push(arg);
                }
            }
            Ok(())
        }

        loop {
            if let Some(mut args) = parser.try_raw_args() {
                if let Some(arg) = args.peek() {
                    // If this is a `-...` flag then check to see if this is a
                    // "nonstandard" flag like `-shared`. That's done by
                    // looking at all `nonstandard` flags and testing if the
                    // option is `-name...`. If so, then assume it's a nonstandard
                    // flag and give it to LLD then move on.
                    //
                    // Note that if any flag has a value it'll get passed to
                    // LLD below in `Arg::Value`, and otherwise if the option
                    // has an embedded `=` in it that'll be handled here via
                    // the `starts_with` check.
                    //
                    // Also note that `--foo` flags are all auto-skipped here
                    // since the `starts_with` check won't pass for any of them
                    // as `f.long` never starts with `-`.
                    if let Some(flag) = arg.strip_prefix("-") {
                        let for_lld = LLD_FLAGS
                            .iter()
                            .filter(|f| f.nonstandard)
                            .filter_map(|f| f.long)
                            .any(|f| flag.starts_with(f));
                        if for_lld {
                            lld_args.push(arg.to_owned());
                            args.next();
                            continue;
                        }
                    }
                }
            }

            match parser.next()? {
                Some(Arg::Value(obj)) => {
                    lld_args.push(obj);
                }
                Some(Arg::Short(c)) => match LLD_FLAGS.iter().find(|f| f.short == Some(c)) {
                    Some(lld) => {
                        handle_lld_arg(lld, &mut parser, &mut lld_args)?;
                    }
                    None => {
                        component_ld_args.push(format!("-{c}").into());
                        if let Some(arg) =
                            command.get_arguments().find(|a| a.get_short() == Some(c))
                        {
                            if let ArgAction::Set = arg.get_action() {
                                component_ld_args.push(parser.value()?);
                            }
                        }
                    }
                },
                Some(Arg::Long(c)) => match LLD_FLAGS.iter().find(|f| f.long == Some(c)) {
                    Some(lld) => {
                        handle_lld_arg(lld, &mut parser, &mut lld_args)?;
                    }
                    None => {
                        let mut flag = OsString::from(format!("--{c}"));
                        if let Some(arg) = command.get_arguments().find(|a| a.get_long() == Some(c))
                        {
                            match arg.get_action() {
                                // fork: a `--flag[=value]` option (declared
                                // `require_equals`) takes a value only from
                                // its own `=`; a bare `--validate-component`
                                // means `true` and never swallows the next
                                // argument (on wasm32-wasip3 that is
                                // `--cooperative-threading`).
                                ArgAction::Set if arg.is_require_equals_set() => {
                                    if let Some(value) = parser.optional_value() {
                                        flag.push("=");
                                        flag.push(value);
                                    }
                                }
                                ArgAction::Set | ArgAction::Append => {
                                    flag.push("=");
                                    flag.push(parser.value()?);
                                }
                                _ => (),
                            }
                        }
                        component_ld_args.push(flag);
                    }
                },
                None => break,
            }
        }

        match command.try_get_matches_from_mut(component_ld_args.clone()) {
            Ok(matches) => Ok(App {
                component: ComponentLdArgs::from_arg_matches(&matches)?,
                lld_args,
                validate_env: env::var_os(validate::ENV),
            }),
            Err(_) => {
                add_wasm_ld_options(ComponentLdArgs::command()).get_matches_from(component_ld_args);
                unreachable!();
            }
        }
    }

    fn run(&mut self) -> Result<()> {
        let warnings = self.run_with_lld_status(|lld, temp_dir, lld_flags| {
            lld.status(temp_dir, lld_flags).map(LldStatus::from)
        })?;
        print_warnings(&warnings);
        Ok(())
    }

    fn run_with_lld<F>(&mut self, mut run_lld: F) -> Result<Vec<String>>
    where
        F: for<'a> FnMut(LldInvocation<'a>) -> Result<LldStatus>,
    {
        self.run_with_lld_status(|lld, _temp_dir, lld_flags| run_lld(lld.invocation(lld_flags)))
    }

    /// Links, then componentizes; returns the link's warnings (none are
    /// printed here).
    fn run_with_lld_status<F>(&mut self, mut run_lld: F) -> Result<Vec<String>>
    where
        F: for<'a> FnMut(&'a Lld, &'a tempfile::TempDir, &'a [&'a OsString]) -> Result<LldStatus>,
    {
        let mut lld = self.lld();

        // If a temporary output is needed make sure it has the same file name
        // as the output of our command itself since LLD will embed this file
        // name in the name section of the output.
        let temp_dir = match self.component.output.parent() {
            Some(parent) => tempfile::TempDir::new_in(parent)?,
            None => tempfile::TempDir::new()?,
        };
        let temp_output = match self.component.output.file_name() {
            Some(name) => temp_dir.path().join(name),
            None => bail!(
                "output of {:?} does not have a file name",
                self.component.output
            ),
        };

        // Shared libraries don't get wit-component run below so place the
        // output directly at the desired output location. Otherwise output to a
        // temporary location for wit-component to read and then the real output
        // is created after wit-component runs.
        if self.skip_wit_component() {
            lld.output(&self.component.output);
        } else {
            lld.output(&temp_output);
        }

        let linker = &lld.exe;
        let lld_flags = self
            .lld_args
            .iter()
            .chain(&self.component.append_lld_flag)
            .collect::<Vec<_>>();
        let status = run_lld(&lld, &temp_dir, &lld_flags)
            .with_context(|| format!("failed to spawn {linker:?}"))?;
        if !status.success() {
            bail!("failed to invoke LLD: {status}");
        }

        if self.skip_wit_component() {
            return Ok(Vec::new());
        }

        let reactor_adapter =
            wasi_preview1_component_adapter_provider::WASI_SNAPSHOT_PREVIEW1_REACTOR_ADAPTER;
        let command_adapter =
            wasi_preview1_component_adapter_provider::WASI_SNAPSHOT_PREVIEW1_COMMAND_ADAPTER;
        let proxy_adapter =
            wasi_preview1_component_adapter_provider::WASI_SNAPSHOT_PREVIEW1_PROXY_ADAPTER;
        let mut core_module = std::fs::read(&temp_output)
            .with_context(|| format!("failed to read {linker:?} output: {temp_output:?}"))?;

        // fork (setouthq/rust#79): a wasm32-wasip3 link that is about to
        // embed the default wasip1 adapter says so. An adapter the caller
        // chose (`--wasi-adapter`, or `--adapt wasi_snapshot_preview1=...`)
        // is not a surprise, so it stays quiet then.
        let chose_adapter = self.component.wasi_adapter.is_some()
            || self
                .component
                .adapters
                .iter()
                .any(|(name, _)| name == p3_adapter::PREVIEW1);
        let mut warnings = Vec::new();
        if !chose_adapter {
            warnings.extend(p3_adapter::warning(&core_module, &lld_flags));
        }

        // Inspect the output module to see if it's a command or reactor.
        let mut exports_start = false;
        for payload in wasmparser::Parser::new(0).parse_all(&core_module) {
            match payload {
                Ok(Payload::ExportSection(e)) => {
                    for export in e {
                        if let Ok(e) = export {
                            if e.name == "_start" {
                                exports_start = true;
                                break;
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        if !self.component.component_types.is_empty() {
            let mut merged = None::<(Resolve, WorldId)>;
            for wit_file in &self.component.component_types {
                let (wit_file, world_name) = component_type_path_and_world(wit_file);
                let mut resolve = Resolve::default();
                let (package, _) = resolve
                    .push_path(&wit_file)
                    .with_context(|| format!("unable to add component type {wit_file:?}"))?;

                let world = resolve.select_world(&[package], world_name.as_deref())?;

                if let Some((merged_resolve, merged_world)) = &mut merged {
                    let world = merged_resolve
                        .merge(resolve)?
                        .map_world(world, Default::default())?;
                    merged_resolve.merge_worlds(world, *merged_world, &mut Default::default())?;
                } else {
                    merged = Some((resolve, world));
                }
            }

            let Some((resolve, world)) = merged else {
                unreachable!()
            };

            wit_component::embed_component_metadata(
                &mut core_module,
                &resolve,
                world,
                self.component.string_encoding,
                false,
            )?;
        }

        strip_incompatible_component_type_sections(&mut core_module)?;

        let mut encoder = wit_component::ComponentEncoder::default();
        encoder
            .reject_legacy_names(self.component.reject_legacy_names)
            .realloc_via_memory_grow(self.component.realloc_via_memory_grow)
            .shim_return_call_ref(self.component.return_call_ref);
        if let Some(merge) = self.component.merge_imports_based_on_semver {
            encoder.merge_imports_based_on_semver(merge.unwrap_or(true));
        }
        encoder
            .module(&core_module)
            .context("failed to parse core wasm for componentization")?;
        let adapter = self.component.wasi_adapter.unwrap_or(if exports_start {
            WasiAdapter::Command
        } else {
            WasiAdapter::Reactor
        });
        let adapter = match adapter {
            WasiAdapter::Command => Some(&command_adapter[..]),
            WasiAdapter::Reactor => Some(&reactor_adapter[..]),
            WasiAdapter::Proxy => Some(&proxy_adapter[..]),
            WasiAdapter::None => None,
        };

        if let Some(adapter) = adapter {
            encoder
                .adapter("wasi_snapshot_preview1", adapter)
                .context("failed to inject adapter")?;
        }

        for (name, adapter) in self.component.adapters.iter() {
            encoder
                .adapter(name, adapter)
                .with_context(|| format!("failed to inject adapter {name:?}"))?;
        }

        let component = encoder
            .encode()
            .map_err(|err| annotate_missing_export(err, &core_module))
            .context("failed to encode component")?;

        // fork: validate what is about to be written (see `validate`); the
        // encoder's own `validate` option is left off so that a failure
        // reads as the linker's, with what it found, not as an encode error.
        if validate::enabled(
            self.component.validate_component,
            self.validate_env.as_deref(),
        ) {
            validate::check(&component, &self.component.output)?;
        }

        std::fs::write(&self.component.output, &component).context(format!(
            "failed to write output file: {:?}",
            self.component.output
        ))?;

        Ok(warnings)
    }

    fn skip_wit_component(&self) -> bool {
        self.component.skip_wit_component
            // Skip componentization with `--shared` since that's creating a
            // shared library that's not a component yet.
            || self.lld_args.iter().any(|s| s == "-shared" || s == "--shared")
    }

    fn lld(&self) -> Lld {
        let mut lld = self.find_lld();
        if self.component.verbose {
            lld.verbose = true
        }
        lld
    }

    fn find_lld(&self) -> Lld {
        if let Some(path) = &self.component.wasm_ld_path {
            return Lld::new(path);
        }

        // Search for the first of `wasm-ld` or `rust-lld` in `$PATH`
        let wasm_ld = format!("wasm-ld{}", env::consts::EXE_SUFFIX);
        let rust_lld = format!("rust-lld{}", env::consts::EXE_SUFFIX);
        for entry in split_path_env(&env::var_os("PATH").unwrap_or_default()) {
            if entry.join(&wasm_ld).is_file() {
                return Lld::new(wasm_ld);
            }
            if entry.join(&rust_lld).is_file() {
                let mut lld = Lld::new(rust_lld);
                lld.needs_flavor = true;
                return lld;
            }
        }

        // Fall back to `wasm-ld` if the search failed to get an error message
        // that indicates that `wasm-ld` was attempted to be found but couldn't
        // be found.
        Lld::new("wasm-ld")
    }
}

fn split_path_env(path: &OsStr) -> Vec<PathBuf> {
    #[cfg(target_os = "wasi")]
    {
        path.to_string_lossy()
            .split(':')
            .filter(|entry| !entry.is_empty())
            .map(PathBuf::from)
            .collect()
    }

    #[cfg(not(target_os = "wasi"))]
    {
        env::split_paths(path).collect()
    }
}

/// rust#41: when the encoder fails with wit-component's
/// `failed to find export of interface `I` function `f`` (a dead end —
/// it names neither near-miss versions nor the nothing-implements-it
/// case), rescan the core module's export section, where component
/// exports carry canonical `interface#function` names, and say which of
/// the two failure faces this actually is. Any other error passes
/// through untouched.
fn annotate_missing_export(err: anyhow::Error, core_module: &[u8]) -> anyhow::Error {
    let Some((iface, func)) = err
        .chain()
        .find_map(|cause| parse_missing_export_error(&cause.to_string()))
    else {
        return err;
    };

    let iface_base = iface.split('@').next().unwrap_or(&iface);
    let near_misses: Vec<String> = module_export_names(core_module)
        .into_iter()
        .filter(|name| {
            let Some((export_iface, export_func)) = name.rsplit_once('#') else {
                return false;
            };
            export_func == func
                && export_iface != iface
                && export_iface.split('@').next() == Some(iface_base)
        })
        .collect();

    let hint = if near_misses.is_empty() {
        format!(
            "the world declares export `{iface}#{func}` but nothing in the linked \
             artifact implements it — the implementing code may not be part of this \
             build (for example a handler living in a binary or module that is not \
             built, or a bindings export macro that is never invoked)"
        )
    } else {
        format!(
            "module exports `{}` — version mismatch between the declaring world \
             (`{iface}`) and the implementing bindings; align the world's wit/deps \
             pin with the bindings crate's interface version",
            near_misses.join("`, `")
        )
    };
    err.context(hint)
}

/// Parse `failed to find export of interface `I` function `f`` into
/// (I, f). The text is wit-component's (pinned by our lock); if it ever
/// drifts, this returns `None` and the annotation silently disappears —
/// the corpus battery leg is what screams then.
fn parse_missing_export_error(message: &str) -> Option<(String, String)> {
    let rest = message.strip_prefix("failed to find export of interface `")?;
    let (iface, rest) = rest.split_once('`')?;
    let rest = rest.strip_prefix(" function `")?;
    let (func, _) = rest.split_once('`')?;
    Some((iface.to_string(), func.to_string()))
}

/// Names in the core module's export section — component-bound exports
/// carry the canonical `interface#function` naming scheme.
fn module_export_names(module: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(module) {
        if let Ok(Payload::ExportSection(reader)) = payload {
            for export in reader {
                if let Ok(export) = export {
                    names.push(export.name.to_string());
                }
            }
        }
    }
    names
}

fn strip_incompatible_component_type_sections(module: &mut Vec<u8>) -> Result<()> {
    if module.len() < 8 || &module[..4] != b"\0asm" {
        return Ok(());
    }

    let mut pos = 8;
    let mut out = Vec::with_capacity(module.len());
    out.extend_from_slice(&module[..8]);

    while pos < module.len() {
        let section_start = pos;
        let id = module[pos];
        pos += 1;

        let (payload_len, next) = read_u32_leb(module, pos)?;
        pos = next;

        let payload_end = pos
            .checked_add(payload_len as usize)
            .filter(|end| *end <= module.len())
            .context("malformed wasm section length")?;

        let strip = id == 0
            && custom_section_name(&module[pos..payload_end])
                .map(is_incompatible_component_type_section)
                .unwrap_or(false);

        if !strip {
            match split_component_type_section(id, &module[pos..payload_end]) {
                Some(sections) => {
                    for (name, data) in sections {
                        push_custom_section(&mut out, &name, data);
                    }
                }
                None => out.extend_from_slice(&module[section_start..payload_end]),
            }
        }

        pos = payload_end;
    }

    *module = out;
    Ok(())
}

fn custom_section_name(payload: &[u8]) -> Option<&str> {
    custom_section_name_and_data(payload).map(|(name, _)| name)
}

fn custom_section_name_and_data(payload: &[u8]) -> Option<(&str, &[u8])> {
    let (name_len, name_start) = read_u32_leb(payload, 0).ok()?;
    let name_end = name_start.checked_add(name_len as usize)?;
    let name = std::str::from_utf8(payload.get(name_start..name_end)?).ok()?;
    Some((name, &payload[name_end..]))
}

/// fork: a `component-type` section that holds more than one encoded world,
/// as one section per distinct world.
///
/// wasm-ld concatenates custom sections that share a name, so when two
/// objects carry the same crate's bindings section the linked module holds
/// both encodings back to back under one name, and wit-component fails on
/// it with "expected section, got wasm magic number". Two copies of a crate
/// get into one link when a binary links rlibs from two cargo builds: the
/// WASI-hosted rustc links rustc's `wasi` 0.14 crate and its tools' copy.
/// Fat LTO hides it (merging the modules keeps one copy of identical
/// `wasm.custom_sections` metadata); thin LTO and no LTO keep one object per
/// copy. Identical encodings are kept once, under the section's name; a
/// different encoding under the same name (two versions of a crate whose
/// section name carries no version) gets its own section, named
/// `<name>#<n>`, which wit-component merges like any other
/// `component-type` section.
///
/// Returns `None` (keep the section as it is) for any other section, and
/// for one that holds a single encoding or does not parse as a run of
/// encodings.
fn split_component_type_section(id: u8, payload: &[u8]) -> Option<Vec<(String, &[u8])>> {
    if id != 0 {
        return None;
    }
    let (name, data) = custom_section_name_and_data(payload)?;
    if !name.starts_with("component-type") {
        return None;
    }
    let mut distinct = Vec::<&[u8]>::new();
    for encoding in concatenated_binaries(data)? {
        if !distinct.contains(&encoding) {
            distinct.push(encoding);
        }
    }
    Some(
        distinct
            .into_iter()
            .enumerate()
            .map(|(i, data)| match i {
                0 => (name.to_string(), data),
                _ => (format!("{name}#{i}"), data),
            })
            .collect(),
    )
}

/// The wasm binaries `data` holds back to back, when it holds more than one.
///
/// A binary has no end marker: it ends where the next one's header stands in
/// place of a section. No section can start with the header's bytes (a
/// custom section, id 0, of size 0x61 whose name length, 0x73, overruns it),
/// so the split is exact. `None` for a single binary or for bytes that are
/// not a run of binaries with well-formed section framing.
fn concatenated_binaries(data: &[u8]) -> Option<Vec<&[u8]>> {
    const MAGIC: &[u8] = b"\0asm";
    const HEADER_LEN: usize = 8;
    if data.len() < HEADER_LEN || !data.starts_with(MAGIC) {
        return None;
    }
    let mut binaries = Vec::new();
    let mut start = 0;
    let mut pos = HEADER_LEN;
    while pos < data.len() {
        if data[pos..].starts_with(MAGIC) {
            if data.len() - pos < HEADER_LEN {
                return None;
            }
            binaries.push(&data[start..pos]);
            start = pos;
            pos += HEADER_LEN;
            continue;
        }
        let (size, next) = read_u32_leb(data, pos + 1).ok()?;
        pos = next
            .checked_add(size as usize)
            .filter(|end| *end <= data.len())?;
    }
    if binaries.is_empty() {
        return None;
    }
    binaries.push(&data[start..]);
    Some(binaries)
}

fn push_custom_section(out: &mut Vec<u8>, name: &str, data: &[u8]) {
    let mut payload = Vec::with_capacity(name.len() + data.len() + 5);
    push_u32_leb(&mut payload, name.len() as u32);
    payload.extend_from_slice(name.as_bytes());
    payload.extend_from_slice(data);
    out.push(0);
    push_u32_leb(out, payload.len() as u32);
    out.extend_from_slice(&payload);
}

fn push_u32_leb(dst: &mut Vec<u8>, mut value: u32) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            dst.push(byte);
            return;
        }
        dst.push(byte | 0x80);
    }
}

/// Whether `name` is one of std's own export-less component-type sections,
/// which this linker drops before componentizing.
///
/// std's bindings crates (wasip2 and wasip3, built with `rustc-dep-of-std`)
/// end their section names in `-in-libstd`, and the fork std's
/// devenv-subprocess guest emits the `devenv:subprocess` imports world
/// bare. Those worlds describe what std may import, not what this program
/// does: the program's actual imports get their types from std's
/// `wasi:cli` imports world (kept) and from `--component-type` (the
/// devenv WIT a session names in `WASM_COMPONENT_TYPE_WIT_PATHS`).
///
/// Every other component-type section is kept. A user's own bindings (a
/// wasi:http client crate such as wasip2, wasip3, wasi or wstd, or the
/// user's own wit-bindgen world) are the only type information for the
/// imports they make; dropping them fails the link with "module requires
/// an import interface named `wasi:http/types@...`".
fn is_incompatible_component_type_section(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("component-type:wit-bindgen:") else {
        return false;
    };
    let Some((_wit_bindgen_version, world)) = rest.split_once(':') else {
        return false;
    };
    let exportless = world.contains("-with-all-of-its-exports-removed:encoded world");
    exportless && (world.ends_with("-in-libstd") || is_std_devenv_subprocess_world(world))
}

/// std's devenv-subprocess guest world (library/devenv-subprocess-guest):
/// `devenv:subprocess@<version>:imports-with-all-of-its-exports-removed:encoded world`,
/// with no suffix after "encoded world".
fn is_std_devenv_subprocess_world(world: &str) -> bool {
    world
        .strip_prefix("devenv:subprocess@")
        .and_then(|rest| rest.split_once(':'))
        .is_some_and(|(_version, rest)| {
            rest == "imports-with-all-of-its-exports-removed:encoded world"
        })
}

fn read_u32_leb(bytes: &[u8], mut pos: usize) -> Result<(u32, usize)> {
    let mut result = 0u32;
    let mut shift = 0;

    for _ in 0..5 {
        let byte = *bytes.get(pos).context("truncated u32 LEB128")?;
        pos += 1;
        result |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok((result, pos));
        }
        shift += 7;
    }

    bail!("invalid u32 LEB128");
}

#[cfg(test)]
mod missing_export_annotation_tests {
    use super::{annotate_missing_export, module_export_names, parse_missing_export_error};

    /// A minimal core module whose export section carries `names` as
    /// func exports. Indices are dangling — wasmparser's parser (not
    /// validator) reads names regardless, which is all the rescan needs.
    fn module_with_exports(names: &[&str]) -> Vec<u8> {
        let mut m = b"\0asm\x01\0\0\0".to_vec();
        let mut payload = vec![names.len() as u8];
        for name in names {
            payload.push(name.len() as u8);
            payload.extend_from_slice(name.as_bytes());
            payload.push(0x00); // export kind: func
            payload.push(0x00); // func index 0
        }
        m.push(7); // export section id
        m.push(payload.len() as u8);
        m.extend_from_slice(&payload);
        m
    }

    #[test]
    fn parses_the_wit_component_missing_export_text() {
        assert_eq!(
            parse_missing_export_error(
                "failed to find export of interface `wasi:http/incoming-handler@0.2.9` function `handle`"
            ),
            Some((
                "wasi:http/incoming-handler@0.2.9".to_string(),
                "handle".to_string()
            ))
        );
        assert_eq!(parse_missing_export_error("module was not valid"), None);
    }

    #[test]
    fn export_names_are_read_from_the_export_section() {
        let m = module_with_exports(&["cabi_realloc", "wasi:http/incoming-handler@0.2.12#handle"]);
        assert_eq!(
            module_export_names(&m),
            vec!["cabi_realloc", "wasi:http/incoming-handler@0.2.12#handle"]
        );
    }

    #[test]
    fn near_miss_version_is_named_as_a_mismatch() {
        // The rust#41 field shape: world declares @0.2.9, bindings
        // (wstd 0.5.6 vintage) export @0.2.12.
        let m = module_with_exports(&["cabi_realloc", "wasi:http/incoming-handler@0.2.12#handle"]);
        let err = anyhow::anyhow!(
            "failed to find export of interface `wasi:http/incoming-handler@0.2.9` function `handle`"
        );
        let annotated = format!("{:#}", annotate_missing_export(err, &m));
        assert!(
            annotated.contains("module exports `wasi:http/incoming-handler@0.2.12#handle`"),
            "{annotated}"
        );
        assert!(annotated.contains("version mismatch"), "{annotated}");
        assert!(annotated.contains("wit/deps pin"), "{annotated}");
    }

    #[test]
    fn no_candidate_at_any_version_names_the_unimplemented_face() {
        // The sibling defect: the linker line carried only cabi_realloc
        // (handler impl in a never-built src/main.rs).
        let m = module_with_exports(&["cabi_realloc"]);
        let err = anyhow::anyhow!(
            "failed to find export of interface `wasi:http/incoming-handler@0.2.9` function `handle`"
        );
        let annotated = format!("{:#}", annotate_missing_export(err, &m));
        assert!(
            annotated.contains("nothing in the linked artifact implements it"),
            "{annotated}"
        );
        assert!(annotated.contains("never invoked"), "{annotated}");
    }

    #[test]
    fn a_different_function_at_the_same_interface_is_not_a_near_miss() {
        let m = module_with_exports(&["wasi:http/incoming-handler@0.2.12#other"]);
        let err = anyhow::anyhow!(
            "failed to find export of interface `wasi:http/incoming-handler@0.2.9` function `handle`"
        );
        let annotated = format!("{:#}", annotate_missing_export(err, &m));
        assert!(
            annotated.contains("nothing in the linked artifact implements it"),
            "{annotated}"
        );
    }

    #[test]
    fn unrelated_errors_pass_through_untouched() {
        let m = module_with_exports(&["cabi_realloc"]);
        let err = anyhow::anyhow!("failed to inject adapter");
        let annotated = format!("{:#}", annotate_missing_export(err, &m));
        assert_eq!(annotated, "failed to inject adapter");
    }
}

#[cfg(test)]
mod tests {
    use super::{component_type_path_and_world, strip_incompatible_component_type_sections};

    #[test]
    fn parses_component_type_world_suffix() {
        let (path, world) = component_type_path_and_world("/wit/wasi-http#client-imports");
        assert_eq!(path, std::path::PathBuf::from("/wit/wasi-http"));
        assert_eq!(world.as_deref(), Some("client-imports"));

        let (path, world) = component_type_path_and_world("/wit/devenv-subprocess.wit");
        assert_eq!(path, std::path::PathBuf::from("/wit/devenv-subprocess.wit"));
        assert_eq!(world, None);

        let (path, world) = component_type_path_and_world("/wit/http#");
        assert_eq!(path, std::path::PathBuf::from("/wit/http#"));
        assert_eq!(world, None);
    }

    // Section names as the crates spell them: wasip2 1.0.4, wasip3 0.7.0
    // and 0.9.0 append "-in-libstd" when built as std's dependency
    // (feature rustc-dep-of-std) and nothing otherwise; wasi 0.14 has one
    // spelling; std's devenv-subprocess guest emits the devenv:subprocess
    // world bare.
    const STD_P2_PROXY: &str = "component-type:wit-bindgen:0.58.0:wasi:http@0.2.12:proxy-with-all-of-its-exports-removed:encoded worldrust-wasip2-1.0.4+wasi-0.2.12-from-crates-io-proxy-world-in-libstd";
    const STD_P2_COMMAND: &str = "component-type:wit-bindgen:0.58.0:wasi:cli@0.2.12:command-with-all-of-its-exports-removed:encoded worldrust-wasip2-1.0.4+wasi-0.2.12-from-crates-io-command-world-in-libstd";
    const STD_P3_SERVICE: &str = "component-type:wit-bindgen:0.58.0:wasi:http@0.3.0:service-with-all-of-its-exports-removed:encoded worldrust-wasip3-0.7.0+wasi-0.3.0-from-crates-io-service-world-in-libstd";
    const STD_P3_COMMAND: &str = "component-type:wit-bindgen:0.58.0:wasi:cli@0.3.0:command-with-all-of-its-exports-removed:encoded worldrust-wasip3-0.7.0+wasi-0.3.0-from-crates-io-command-world-in-libstd";
    const STD_P2_IMPORTS: &str = "component-type:wit-bindgen:0.58.0:wasi:cli@0.2.12:imports:encoded worldrust-wasip2-1.0.4+wasi-0.2.12-from-crates-io-in-libstd";
    const STD_DEVENV_SUBPROCESS: &str = "component-type:wit-bindgen:0.58.0:devenv:subprocess@0.4.4:imports-with-all-of-its-exports-removed:encoded world";
    const USER_P2_PROXY: &str = "component-type:wit-bindgen:0.58.0:wasi:http@0.2.12:proxy-with-all-of-its-exports-removed:encoded worldrust-wasip2-1.0.4+wasi-0.2.12-from-crates-io-proxy-world";
    const USER_P2_COMMAND: &str = "component-type:wit-bindgen:0.58.0:wasi:cli@0.2.12:command-with-all-of-its-exports-removed:encoded worldrust-wasip2-1.0.4+wasi-0.2.12-from-crates-io-command-world";
    const USER_P3_SERVICE: &str = "component-type:wit-bindgen:0.62.0:wasi:http@0.3.0:service-with-all-of-its-exports-removed:encoded worldrust-wasip3-0.9.0+wasi-0.3.0-from-crates-io-service-world";
    const USER_WASI_014_PROXY: &str = "component-type:wit-bindgen:0.39.0:wasi:http@0.2.4:proxy-with-all-of-its-exports-removed:encoded worldrust-wasi-from-crates-io-proxy-world";
    const USER_WASI_014_BINDINGS: &str = "component-type:wit-bindgen:0.39.0:rust:wasi:bindings:encoded worldrust-wasi-from-crates-io";
    const USER_OWN_WORLD: &str = "component-type:wit-bindgen:0.62.0:my:app:client-with-all-of-its-exports-removed:encoded world";

    fn kept_after_strip(names: &[&str]) -> Vec<String> {
        let sections = names
            .iter()
            .map(|name| (*name, name.as_bytes()))
            .collect::<Vec<_>>();
        let mut module = wasm_with_custom_sections(&sections);
        strip_incompatible_component_type_sections(&mut module).unwrap();
        custom_section_names(&module)
    }

    #[test]
    fn strips_std_exportless_component_type_sections() {
        let kept = kept_after_strip(&[
            STD_P2_PROXY,
            STD_P2_COMMAND,
            STD_P3_SERVICE,
            STD_P3_COMMAND,
            STD_DEVENV_SUBPROCESS,
            "component-type:wit-bindgen:0.39.0:wasi:cli@0.2.4:command:imports and exports",
        ]);
        assert_eq!(
            kept,
            ["component-type:wit-bindgen:0.39.0:wasi:cli@0.2.4:command:imports and exports"]
        );
    }

    #[test]
    fn keeps_std_imports_worlds() {
        // std's wasi:cli imports world was never stripped: it carries the
        // types of every wasi import std makes.
        assert_eq!(kept_after_strip(&[STD_P2_IMPORTS]), [STD_P2_IMPORTS]);
    }

    #[test]
    fn keeps_a_users_component_type_sections() {
        // A user's wasi:http client crate (wasip2, wasip3, wasi 0.14, or its
        // own wit-bindgen world) carries the only type information for its
        // wasi:http imports: stripping it fails the link with "module
        // requires an import interface named `wasi:http/types@...`".
        let user = [
            USER_P2_PROXY,
            USER_P2_COMMAND,
            USER_P3_SERVICE,
            USER_WASI_014_PROXY,
            USER_WASI_014_BINDINGS,
            USER_OWN_WORLD,
        ];
        assert_eq!(kept_after_strip(&user), user);
    }

    #[test]
    fn mixed_std_and_user_sections_keep_only_the_users() {
        let kept = kept_after_strip(&[
            STD_P3_SERVICE,
            USER_P3_SERVICE,
            STD_P2_PROXY,
            USER_P2_PROXY,
            STD_DEVENV_SUBPROCESS,
            STD_P2_IMPORTS,
        ]);
        assert_eq!(kept, [USER_P3_SERVICE, USER_P2_PROXY, STD_P2_IMPORTS]);
    }

    #[test]
    fn leaves_non_wasm_inputs_alone() {
        let mut bytes = b"not wasm".to_vec();
        strip_incompatible_component_type_sections(&mut bytes).unwrap();
        assert_eq!(bytes, b"not wasm");
    }

    const DUP_WIT: &str = r#"
        package my:dup;
        interface a { ping: func(); }
        interface b { pong: func(); }
        world wa { import a; }
        world wb { import b; }
    "#;

    /// What wit-bindgen puts in a `component-type` section for `world`.
    fn encoded_world(world: &str) -> Vec<u8> {
        let mut resolve = wit_parser::Resolve::default();
        let package = resolve.push_str("dup.wit", DUP_WIT).unwrap();
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

    /// A core module importing `imports` (module, name) as `() -> ()`
    /// functions, then one custom section per `(name, data)`.
    fn module_importing(imports: &[(&str, &str)], sections: &[(&str, &[u8])]) -> Vec<u8> {
        let imports = imports
            .iter()
            .map(|(module, name)| format!("(import \"{module}\" \"{name}\" (func))"))
            .collect::<String>();
        let mut module = wat::parse_str(format!("(module {imports})")).unwrap();
        module.extend_from_slice(&wasm_with_custom_sections(sections)[8..]);
        module
    }

    fn componentize(module: &[u8]) -> anyhow::Result<Vec<u8>> {
        wit_component::ComponentEncoder::default()
            .module(module)?
            .encode()
    }

    // The section wasm-ld writes when two objects carry the wasi crate's
    // bindings section (rustc's copy of wasi 0.14 and its tools' copy in the
    // WASI-hosted rustc, linked without fat LTO).
    #[test]
    fn one_world_encoded_twice_under_one_name_is_kept_once() {
        let world = encoded_world("wa");
        let twice = [world.as_slice(), world.as_slice()].concat();
        let module = module_importing(&[("my:dup/a", "ping")], &[(USER_WASI_014_PROXY, &twice)]);

        let err = componentize(&module).unwrap_err();
        assert!(
            format!("{err:?}").contains("expected section, got wasm magic number"),
            "{err:?}"
        );

        let mut fixed = module.clone();
        strip_incompatible_component_type_sections(&mut fixed).unwrap();
        assert_eq!(custom_section_names(&fixed), [USER_WASI_014_PROXY]);
        componentize(&fixed).unwrap();
    }

    #[test]
    fn different_worlds_under_one_name_each_get_a_section() {
        let (wa, wb) = (encoded_world("wa"), encoded_world("wb"));
        let both = [wa.as_slice(), wb.as_slice(), wa.as_slice()].concat();
        let module = module_importing(
            &[("my:dup/a", "ping"), ("my:dup/b", "pong")],
            &[(USER_WASI_014_PROXY, &both)],
        );
        assert!(componentize(&module).is_err());

        let mut fixed = module.clone();
        strip_incompatible_component_type_sections(&mut fixed).unwrap();
        assert_eq!(
            custom_section_names(&fixed),
            [
                USER_WASI_014_PROXY.to_string(),
                format!("{USER_WASI_014_PROXY}#1")
            ]
        );
        let component = componentize(&fixed).unwrap();
        let imports = wasmparser::Parser::new(0)
            .parse_all(&component)
            .filter_map(|payload| match payload.unwrap() {
                wasmparser::Payload::ComponentImportSection(reader) => Some(reader),
                _ => None,
            })
            .flatten()
            .map(|import| import.unwrap().name.name.to_string())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            imports,
            ["my:dup/a".to_string(), "my:dup/b".to_string()].into()
        );
    }

    #[test]
    fn a_single_encoding_and_other_sections_pass_through_unchanged() {
        let world = encoded_world("wa");
        let module = module_importing(
            &[("my:dup/a", "ping")],
            &[
                (USER_WASI_014_PROXY, &world),
                ("producers-like", b"\0asm\x0d\0\x01\0\0asm"),
            ],
        );
        let mut same = module.clone();
        strip_incompatible_component_type_sections(&mut same).unwrap();
        assert_eq!(same, module);
    }

    fn custom_section_names(module: &[u8]) -> Vec<String> {
        wasmparser::Parser::new(0)
            .parse_all(module)
            .filter_map(|payload| match payload.unwrap() {
                wasmparser::Payload::CustomSection(section) => Some(section.name().to_string()),
                _ => None,
            })
            .collect()
    }

    fn wasm_with_custom_sections(sections: &[(&str, &[u8])]) -> Vec<u8> {
        let mut module = b"\0asm\x01\0\0\0".to_vec();
        for (name, data) in sections {
            let mut payload = Vec::new();
            push_leb(name.len() as u32, &mut payload);
            payload.extend_from_slice(name.as_bytes());
            payload.extend_from_slice(data);

            module.push(0);
            push_leb(payload.len() as u32, &mut module);
            module.extend_from_slice(&payload);
        }
        module
    }

    fn push_leb(mut value: u32, dst: &mut Vec<u8>) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            dst.push(byte);
            if value == 0 {
                break;
            }
        }
    }
}

/// Helper structure representing an `lld` invocation.
struct Lld {
    exe: PathBuf,
    needs_flavor: bool,
    verbose: bool,
    output: Option<PathBuf>,
}

impl Lld {
    fn new(exe: impl Into<PathBuf>) -> Lld {
        Lld {
            exe: exe.into(),
            needs_flavor: false,
            verbose: false,
            output: None,
        }
    }

    fn output(&mut self, dst: impl Into<PathBuf>) {
        self.output = Some(dst.into());
    }

    fn invocation<'a>(&'a self, args: &'a [&'a OsString]) -> LldInvocation<'a> {
        LldInvocation {
            exe: &self.exe,
            needs_flavor: self.needs_flavor,
            verbose: self.verbose,
            output: self.output.as_deref(),
            args,
        }
    }

    fn status(&self, tmpdir: &tempfile::TempDir, args: &[&OsString]) -> Result<ExitStatus> {
        // If we can probably pass `args` natively, try to do so. In some cases
        // though just skip this entirely and go straight to below.
        if !self.probably_too_big(args) {
            match self.run(args) {
                // If this subprocess failed to spawn because the arguments
                // were too large, fall through to below.
                Err(ref e) if self.command_line_too_big(e) => {
                    if self.verbose {
                        eprintln!("command line was too large, trying again...");
                    }
                }
                other => return Ok(other?),
            }
        } else if self.verbose {
            eprintln!("arguments probably too large {args:?}");
        }

        // The `args` are too big to be passed via the command line itself so
        // encode them using "posix quoting" into an "argfile". This gets
        // passed as `@foo` to LLD and we also pass `--rsp-quoting=posix` to
        // ensure that LLD always uses posix quoting. That means that we don't
        // have to implement the dual nature of both posix and windows encoding
        // here.
        let mut argfile = Vec::new();
        for arg in args {
            for byte in arg.as_encoded_bytes() {
                if *byte == b'\\' || *byte == b' ' {
                    argfile.push(b'\\');
                }
                argfile.push(*byte);
            }
            argfile.push(b'\n');
        }
        let path = tmpdir.path().join("argfile_tmp");
        std::fs::write(&path, &argfile).with_context(|| format!("failed to write {path:?}"))?;
        let mut argfile_arg = OsString::from("@");
        argfile_arg.push(&path);
        let status = self.run(&[&"--rsp-quoting=posix".into(), &argfile_arg])?;
        Ok(status)
    }

    /// Tests whether the `args` array is too large to execute natively.
    ///
    /// Windows `cmd.exe` has a very small limit of around 8k so perform a
    /// guess up to 6k. This isn't 100% accurate.
    fn probably_too_big(&self, args: &[&OsString]) -> bool {
        let args_size = args
            .iter()
            .map(|s| s.as_encoded_bytes().len())
            .sum::<usize>();
        cfg!(windows) && args_size > 6 * 1024
    }

    /// Test if the OS failed to spawn a process because the arguments were too
    /// long.
    fn command_line_too_big(&self, err: &std::io::Error) -> bool {
        #[cfg(unix)]
        return err.raw_os_error() == Some(libc::E2BIG);
        #[cfg(windows)]
        return err.raw_os_error()
            == Some(windows_sys::Win32::Foundation::ERROR_FILENAME_EXCED_RANGE as i32);
        #[cfg(not(any(unix, windows)))]
        {
            let _ = err;
            return false;
        }
    }

    fn run(&self, args: &[&OsString]) -> std::io::Result<ExitStatus> {
        let mut cmd = self.invocation(args).command();
        if self.verbose {
            eprintln!("running {cmd:?}");
        }
        cmd.status()
    }
}

fn add_wasm_ld_options(mut command: clap::Command) -> clap::Command {
    use clap::Arg;

    command = command.arg(
        Arg::new("objects")
            .action(ArgAction::Append)
            .help("objects to pass to `wasm-ld`"),
    );

    for flag in LLD_FLAGS {
        let mut arg = Arg::new(flag.clap_name).help("forwarded to `wasm-ld`");
        if let Some(short) = flag.short {
            arg = arg.short(short);
        }
        if let Some(long) = flag.long {
            arg = arg.long(long);
        }
        arg = match flag.value {
            FlagValue::RequiredEqual(name) | FlagValue::RequiredSpace(name) => {
                arg.action(ArgAction::Set).value_name(name)
            }
            FlagValue::Optional(name) => arg
                .action(ArgAction::Set)
                .value_name(name)
                .num_args(0..=1)
                .require_equals(true),
            FlagValue::None => arg.action(ArgAction::SetTrue),
        };
        arg = arg.help_heading("Options forwarded to `wasm-ld`");
        command = command.arg(arg);
    }

    command
}

#[test]
fn verify_app() {
    ComponentLdArgs::command().debug_assert();
    add_wasm_ld_options(ComponentLdArgs::command()).debug_assert();
}

#[test]
fn run_from_args_with_lld_uses_callback() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let output = temp.path().join("out.wasm");
    let mut saw_invocation = false;

    run_from_args_with_lld(
        [
            OsString::from("wasm-component-ld"),
            OsString::from("--skip-wit-component"),
            OsString::from("-o"),
            output.as_os_str().to_owned(),
            OsString::from("input.o"),
        ],
        |invocation| {
            let argv = invocation.argv();
            assert!(argv.iter().any(|arg| arg == "input.o"));
            assert!(argv.iter().any(|arg| arg == "-o"));
            assert!(argv.iter().any(|arg| arg == output.as_os_str()));
            saw_invocation = true;
            Ok(LldStatus::from_code(0))
        },
    )?;

    assert!(saw_invocation);
    Ok(())
}

#[test]
fn p3_link_with_preview1_imports_returns_the_adapter_warning() -> Result<()> {
    // setouthq/rust#79: a wasm32-wasip3 link (`--cooperative-threading`)
    // whose core module imports `wasi_snapshot_preview1.random_get` embeds
    // the wasip1 adapter, and the caller hears about it. The fake LLD writes
    // a ready core module where the real one would write its output.
    let core = wat::parse_str(
        r#"(module
             (import "wasi_snapshot_preview1" "random_get"
               (func $random_get (param i32 i32) (result i32)))
             (memory (export "memory") 1)
             (func $_ZN4wasi13lib_generated10random_get17h0123456789abcdefE
               (param i32 i32) (result i32)
               local.get 0 local.get 1 call $random_get)
             (func (export "_start")
               i32.const 0 i32.const 8
               call $_ZN4wasi13lib_generated10random_get17h0123456789abcdefE
               drop))"#,
    )?;
    let link = |extra: &[&str]| -> Result<(Vec<String>, Vec<u8>)> {
        let temp = tempfile::tempdir()?;
        let output = temp.path().join("out.wasm");
        let mut args = vec![
            OsString::from("wasm-component-ld"),
            OsString::from("--realloc-via-memory-grow"),
            OsString::from("-o"),
            output.as_os_str().to_owned(),
            OsString::from("input.o"),
        ];
        args.extend(extra.iter().map(OsString::from));
        let warnings = run_from_args_with_lld_warnings(args, |invocation| {
            let argv = invocation.argv();
            let o = argv.iter().position(|a| a == "-o").expect("-o");
            std::fs::write(&argv[o + 1], &core)?;
            Ok(LldStatus::from_code(0))
        })?;
        Ok((warnings, std::fs::read(&output)?))
    };

    let (warnings, component) = link(&["--cooperative-threading"])?;
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains(
            "wasi_snapshot_preview1 import: random_get (called from wasi::lib_generated::random_get)"
        ),
        "{}",
        warnings[0]
    );
    assert!(wasmparser::Parser::is_component(&component));

    // The same module on a wasip2-shaped link: no warning.
    let (warnings, _) = link(&[])?;
    assert!(warnings.is_empty(), "{warnings:?}");

    // An adapter the caller chose explicitly: no warning.
    let (warnings, _) = link(&["--cooperative-threading", "--wasi-adapter=command"])?;
    assert!(warnings.is_empty(), "{warnings:?}");
    Ok(())
}

#[test]
fn an_async_lowered_sync_import_fails_the_link() -> Result<()> {
    // fork: `g` is a sync WIT function, so lowering it with the `async`
    // option (an `[async-lower]g` import) makes a component no runtime
    // loads. wit-component encodes it anyway; the link must fail. The fake
    // LLD writes a ready core module where the real one would write its
    // output. Each link says what SETOUT_WASM_COMPONENT_LD_VALIDATE is (unset
    // for the default), so the ambient environment of `cargo test` does not
    // decide the outcome.
    let temp = tempfile::tempdir()?;
    let wit = temp.path().join("api.wit");
    std::fs::write(
        &wit,
        "package test:sync;\ninterface api { g: func(x: u32); }\nworld root { import api; }\n",
    )?;
    let core = |import: &str, results: &str| {
        wat::parse_str(format!(
            r#"(module
                 (import "test:sync/api" "{import}" (func (param i32) {results}))
                 (memory (export "memory") 1))"#
        ))
    };
    let sync_core = core("g", "")?;
    let async_core = core("[async-lower]g", "(result i32)")?;
    let link = |core: &[u8], extra: &[&str], var: Option<&str>| -> Result<Vec<u8>> {
        let output = temp.path().join("out.wasm");
        let _ = std::fs::remove_file(&output);
        let mut args = vec![
            OsString::from("wasm-component-ld"),
            OsString::from("--wasi-adapter=none"),
            OsString::from("--component-type"),
            wit.as_os_str().to_owned(),
            OsString::from("-o"),
            output.as_os_str().to_owned(),
            OsString::from("input.o"),
        ];
        args.extend(extra.iter().map(OsString::from));
        let mut app = App::parse_from(args)?;
        app.validate_env = var.map(OsString::from);
        app.run_with_lld(|invocation| {
            let argv = invocation.argv();
            let o = argv.iter().position(|a| a == "-o").expect("-o");
            std::fs::write(&argv[o + 1], core)?;
            Ok(LldStatus::from_code(0))
        })?;
        Ok(std::fs::read(&output)?)
    };

    // By default the invalid component fails the link, nothing is written,
    // and the error names the problem, the import and the escapes.
    let err = format!("{:#}", link(&async_core, &[], None).unwrap_err());
    assert!(
        err.contains("the `async` canonical option requires an async function type"),
        "{err}"
    );
    assert!(
        err.contains("in the lowering of `test:sync/api#g`"),
        "{err}"
    );
    assert!(err.contains("SETOUT_WASM_COMPONENT_LD_VALIDATE=0"), "{err}");
    assert!(!temp.path().join("out.wasm").exists());

    // `--validate-component=false` writes it, as upstream does by default,
    // and so does the environment escape; an explicit flag beats the
    // environment.
    for (extra, var) in [
        (&["--validate-component=false"][..], None),
        (&[][..], Some("0")),
        (&["--validate-component=false"][..], Some("1")),
    ] {
        let invalid = link(&async_core, extra, var)?;
        assert!(wasmparser::Parser::is_component(&invalid));
        assert!(
            wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
                .validate_all(&invalid)
                .is_err()
        );
    }
    for (extra, var) in [
        (&[][..], Some("1")),
        (&["--validate-component"][..], Some("0")),
        (&["--validate-component=true"][..], Some("0")),
    ] {
        let err = format!("{:#}", link(&async_core, extra, var).unwrap_err());
        assert!(err.contains("the linked component is invalid"), "{err}");
    }

    // A valid link writes the same bytes with or without validation.
    let validated = link(&sync_core, &[], None)?;
    let unvalidated = link(&sync_core, &["--validate-component=false"], None)?;
    assert_eq!(validated, unvalidated);
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&validated)?;
    Ok(())
}

#[test]
fn a_bare_optional_value_flag_does_not_take_the_next_argument() -> Result<()> {
    // fork: clang passes `--cooperative-threading` right after the user's
    // `-Wl,` flags on wasm32-wasip3; a bare `--validate-component` used to
    // take it as its value and stop the link.
    let app = App::parse_from(
        [
            "wasm-component-ld",
            "--validate-component",
            "--cooperative-threading",
            "--merge-imports-based-on-semver",
            "-o",
            "out.wasm",
            "input.o",
        ]
        .map(OsString::from),
    )?;
    assert_eq!(app.component.validate_component, Some(None));
    assert_eq!(app.component.merge_imports_based_on_semver, Some(None));
    assert!(app.lld_args.iter().any(|a| a == "--cooperative-threading"));
    assert!(app.lld_args.iter().any(|a| a == "input.o"));

    let app = App::parse_from(
        [
            "wasm-component-ld",
            "--validate-component=false",
            "-o",
            "out.wasm",
        ]
        .map(OsString::from),
    )?;
    assert_eq!(app.component.validate_component, Some(Some(false)));
    Ok(())
}
