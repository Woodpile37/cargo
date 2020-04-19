use crate::core::compiler::CompileKind;
use crate::core::compiler::CompileTarget;
use crate::core::{Dependency, TargetKind, Workspace};
use crate::util::config::{Config, StringList, TargetConfig};
use crate::util::{CargoResult, CargoResultExt, ProcessBuilder, Rustc};
use cargo_platform::{Cfg, CfgExpr};
use std::cell::RefCell;
use std::collections::hash_map::{Entry, HashMap};
use std::env;
use std::path::PathBuf;
use std::str::{self, FromStr};

/// Information about the platform target gleaned from querying rustc.
///
/// `RustcTargetData` keeps two of these, one for the host and one for the
/// target. If no target is specified, it uses a clone from the host.
#[derive(Clone)]
pub struct TargetInfo {
    /// A base process builder for discovering crate type information. In
    /// particular, this is used to determine the output filename prefix and
    /// suffix for a crate type.
    crate_type_process: ProcessBuilder,
    /// Cache of output filename prefixes and suffixes.
    ///
    /// The key is the crate type name (like `cdylib`) and the value is
    /// `Some((prefix, suffix))`, for example `libcargo.so` would be
    /// `Some(("lib", ".so")). The value is `None` if the crate type is not
    /// supported.
    crate_types: RefCell<HashMap<String, Option<(String, String)>>>,
    /// `cfg` information extracted from `rustc --print=cfg`.
    cfg: Vec<Cfg>,
    /// Path to the sysroot.
    pub sysroot: PathBuf,
    /// Path to the "lib" or "bin" directory that rustc uses for its dynamic
    /// libraries.
    pub sysroot_host_libdir: PathBuf,
    /// Path to the "lib" directory in the sysroot which rustc uses for linking
    /// target libraries.
    pub sysroot_target_libdir: PathBuf,
    /// Extra flags to pass to `rustc`, see `env_args`.
    pub rustflags: Vec<String>,
    /// Extra flags to pass to `rustdoc`, see `env_args`.
    pub rustdocflags: Vec<String>,
    /// Remove this when it hits stable (1.44)
    pub supports_bitcode_in_rlib: Option<bool>,
}

/// Kind of each file generated by a Unit, part of `FileType`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FileFlavor {
    /// Not a special file type.
    Normal,
    /// Like `Normal`, but not directly executable
    Auxiliary,
    /// Something you can link against (e.g., a library).
    Linkable { rmeta: bool },
    /// Piece of external debug information (e.g., `.dSYM`/`.pdb` file).
    DebugInfo,
}

/// Type of each file generated by a Unit.
pub struct FileType {
    /// The kind of file.
    pub flavor: FileFlavor,
    /// The suffix for the file (for example, `.rlib`).
    /// This is an empty string for executables on Unix-like platforms.
    suffix: String,
    /// The prefix for the file (for example, `lib`).
    /// This is an empty string for things like executables.
    prefix: String,
    /// Flag to convert hyphen to underscore.
    ///
    /// wasm bin targets will generate two files in deps such as
    /// "web-stuff.js" and "web_stuff.wasm". Note the different usages of "-"
    /// and "_". This flag indicates that the stem "web-stuff" should be
    /// converted to "web_stuff".
    should_replace_hyphens: bool,
}

impl FileType {
    pub fn filename(&self, stem: &str) -> String {
        let stem = if self.should_replace_hyphens {
            stem.replace("-", "_")
        } else {
            stem.to_string()
        };
        format!("{}{}{}", self.prefix, stem, self.suffix)
    }
}

impl TargetInfo {
    pub fn new(
        config: &Config,
        requested_kind: CompileKind,
        rustc: &Rustc,
        kind: CompileKind,
    ) -> CargoResult<TargetInfo> {
        let rustflags = env_args(config, requested_kind, &rustc.host, None, kind, "RUSTFLAGS")?;
        let mut process = rustc.process();
        process
            .arg("-")
            .arg("--crate-name")
            .arg("___")
            .arg("--print=file-names")
            .args(&rustflags)
            .env_remove("RUSTC_LOG");

        let mut bitcode_in_rlib_test = process.clone();
        bitcode_in_rlib_test.arg("-Cbitcode-in-rlib");
        let supports_bitcode_in_rlib = match kind {
            CompileKind::Host => Some(rustc.cached_output(&bitcode_in_rlib_test).is_ok()),
            _ => None,
        };

        if let CompileKind::Target(target) = kind {
            process.arg("--target").arg(target.rustc_target());
        }

        let crate_type_process = process.clone();
        const KNOWN_CRATE_TYPES: &[&str] =
            &["bin", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];
        for crate_type in KNOWN_CRATE_TYPES.iter() {
            process.arg("--crate-type").arg(crate_type);
        }

        process.arg("--print=sysroot");
        process.arg("--print=cfg");

        let (output, error) = rustc
            .cached_output(&process)
            .chain_err(|| "failed to run `rustc` to learn about target-specific information")?;

        let mut lines = output.lines();
        let mut map = HashMap::new();
        for crate_type in KNOWN_CRATE_TYPES {
            let out = parse_crate_type(crate_type, &process, &output, &error, &mut lines)?;
            map.insert(crate_type.to_string(), out);
        }

        let line = match lines.next() {
            Some(line) => line,
            None => anyhow::bail!(
                "output of --print=sysroot missing when learning about \
                 target-specific information from rustc\n{}",
                output_err_info(&process, &output, &error)
            ),
        };
        let sysroot = PathBuf::from(line);
        let sysroot_host_libdir = if cfg!(windows) {
            sysroot.join("bin")
        } else {
            sysroot.join("lib")
        };
        let mut sysroot_target_libdir = sysroot.clone();
        sysroot_target_libdir.push("lib");
        sysroot_target_libdir.push("rustlib");
        sysroot_target_libdir.push(match &kind {
            CompileKind::Host => rustc.host.as_str(),
            CompileKind::Target(target) => target.short_name(),
        });
        sysroot_target_libdir.push("lib");

        let cfg = lines
            .map(|line| Ok(Cfg::from_str(line)?))
            .filter(TargetInfo::not_user_specific_cfg)
            .collect::<CargoResult<Vec<_>>>()
            .chain_err(|| {
                format!(
                    "failed to parse the cfg from `rustc --print=cfg`, got:\n{}",
                    output
                )
            })?;

        Ok(TargetInfo {
            crate_type_process,
            crate_types: RefCell::new(map),
            sysroot,
            sysroot_host_libdir,
            sysroot_target_libdir,
            // recalculate `rustflags` from above now that we have `cfg`
            // information
            rustflags: env_args(
                config,
                requested_kind,
                &rustc.host,
                Some(&cfg),
                kind,
                "RUSTFLAGS",
            )?,
            rustdocflags: env_args(
                config,
                requested_kind,
                &rustc.host,
                Some(&cfg),
                kind,
                "RUSTDOCFLAGS",
            )?,
            cfg,
            supports_bitcode_in_rlib,
        })
    }

    fn not_user_specific_cfg(cfg: &CargoResult<Cfg>) -> bool {
        if let Ok(Cfg::Name(cfg_name)) = cfg {
            // This should also include "debug_assertions", but it causes
            // regressions. Maybe some day in the distant future it can be
            // added (and possibly change the warning to an error).
            if cfg_name == "proc_macro" {
                return false;
            }
        }
        true
    }

    /// All the target `cfg` settings.
    pub fn cfg(&self) -> &[Cfg] {
        &self.cfg
    }

    /// Returns the list of file types generated by the given crate type.
    ///
    /// Returns `None` if the target does not support the given crate type.
    pub fn file_types(
        &self,
        crate_type: &str,
        flavor: FileFlavor,
        kind: &TargetKind,
        target_triple: &str,
    ) -> CargoResult<Option<Vec<FileType>>> {
        let mut crate_types = self.crate_types.borrow_mut();
        let entry = crate_types.entry(crate_type.to_string());
        let crate_type_info = match entry {
            Entry::Occupied(o) => &*o.into_mut(),
            Entry::Vacant(v) => {
                let value = self.discover_crate_type(v.key())?;
                &*v.insert(value)
            }
        };
        let (prefix, suffix) = match *crate_type_info {
            Some((ref prefix, ref suffix)) => (prefix, suffix),
            None => return Ok(None),
        };
        let mut ret = vec![FileType {
            suffix: suffix.clone(),
            prefix: prefix.clone(),
            flavor,
            should_replace_hyphens: false,
        }];

        // See rust-lang/cargo#4500.
        if target_triple.ends_with("-windows-msvc")
            && crate_type.ends_with("dylib")
            && suffix == ".dll"
        {
            ret.push(FileType {
                suffix: ".dll.lib".to_string(),
                prefix: prefix.clone(),
                flavor: FileFlavor::Normal,
                should_replace_hyphens: false,
            })
        }

        // See rust-lang/cargo#4535.
        if target_triple.starts_with("wasm32-") && crate_type == "bin" && suffix == ".js" {
            ret.push(FileType {
                suffix: ".wasm".to_string(),
                prefix: prefix.clone(),
                flavor: FileFlavor::Auxiliary,
                should_replace_hyphens: true,
            })
        }

        // See rust-lang/cargo#4490, rust-lang/cargo#4960.
        // Only uplift debuginfo for binaries.
        // - Tests are run directly from `target/debug/deps/` with the
        //   metadata hash still in the filename.
        // - Examples are only uplifted for apple because the symbol file
        //   needs to match the executable file name to be found (i.e., it
        //   needs to remove the hash in the filename). On Windows, the path
        //   to the .pdb with the hash is embedded in the executable.
        let is_apple = target_triple.contains("-apple-");
        if *kind == TargetKind::Bin || (*kind == TargetKind::ExampleBin && is_apple) {
            if is_apple {
                ret.push(FileType {
                    suffix: ".dSYM".to_string(),
                    prefix: prefix.clone(),
                    flavor: FileFlavor::DebugInfo,
                    should_replace_hyphens: false,
                })
            } else if target_triple.ends_with("-msvc") {
                ret.push(FileType {
                    suffix: ".pdb".to_string(),
                    prefix: prefix.clone(),
                    flavor: FileFlavor::DebugInfo,
                    // rustc calls the linker with underscores, and the
                    // filename is embedded in the executable.
                    should_replace_hyphens: true,
                })
            }
        }

        Ok(Some(ret))
    }

    fn discover_crate_type(&self, crate_type: &str) -> CargoResult<Option<(String, String)>> {
        let mut process = self.crate_type_process.clone();

        process.arg("--crate-type").arg(crate_type);

        let output = process.exec_with_output().chain_err(|| {
            format!(
                "failed to run `rustc` to learn about crate-type {} information",
                crate_type
            )
        })?;

        let error = str::from_utf8(&output.stderr).unwrap();
        let output = str::from_utf8(&output.stdout).unwrap();
        Ok(parse_crate_type(
            crate_type,
            &process,
            output,
            error,
            &mut output.lines(),
        )?)
    }
}

/// Takes rustc output (using specialized command line args), and calculates the file prefix and
/// suffix for the given crate type, or returns `None` if the type is not supported. (e.g., for a
/// Rust library like `libcargo.rlib`, we have prefix "lib" and suffix "rlib").
///
/// The caller needs to ensure that the lines object is at the correct line for the given crate
/// type: this is not checked.
///
/// This function can not handle more than one file per type (with wasm32-unknown-emscripten, there
/// are two files for bin (`.wasm` and `.js`)).
fn parse_crate_type(
    crate_type: &str,
    cmd: &ProcessBuilder,
    output: &str,
    error: &str,
    lines: &mut str::Lines<'_>,
) -> CargoResult<Option<(String, String)>> {
    let not_supported = error.lines().any(|line| {
        (line.contains("unsupported crate type") || line.contains("unknown crate type"))
            && line.contains(&format!("crate type `{}`", crate_type))
    });
    if not_supported {
        return Ok(None);
    }
    let line = match lines.next() {
        Some(line) => line,
        None => anyhow::bail!(
            "malformed output when learning about crate-type {} information\n{}",
            crate_type,
            output_err_info(cmd, output, error)
        ),
    };
    let mut parts = line.trim().split("___");
    let prefix = parts.next().unwrap();
    let suffix = match parts.next() {
        Some(part) => part,
        None => anyhow::bail!(
            "output of --print=file-names has changed in the compiler, cannot parse\n{}",
            output_err_info(cmd, output, error)
        ),
    };

    Ok(Some((prefix.to_string(), suffix.to_string())))
}

/// Helper for creating an error message when parsing rustc output fails.
fn output_err_info(cmd: &ProcessBuilder, stdout: &str, stderr: &str) -> String {
    let mut result = format!("command was: {}\n", cmd);
    if !stdout.is_empty() {
        result.push_str("\n--- stdout\n");
        result.push_str(stdout);
    }
    if !stderr.is_empty() {
        result.push_str("\n--- stderr\n");
        result.push_str(stderr);
    }
    if stdout.is_empty() && stderr.is_empty() {
        result.push_str("(no output received)");
    }
    result
}

/// Acquire extra flags to pass to the compiler from various locations.
///
/// The locations are:
///
///  - the `RUSTFLAGS` environment variable
///
/// then if this was not found
///
///  - `target.*.rustflags` from the config (.cargo/config)
///  - `target.cfg(..).rustflags` from the config
///
/// then if neither of these were found
///
///  - `build.rustflags` from the config
///
/// Note that if a `target` is specified, no args will be passed to host code (plugins, build
/// scripts, ...), even if it is the same as the target.
fn env_args(
    config: &Config,
    requested_kind: CompileKind,
    host_triple: &str,
    target_cfg: Option<&[Cfg]>,
    kind: CompileKind,
    name: &str,
) -> CargoResult<Vec<String>> {
    // We *want* to apply RUSTFLAGS only to builds for the
    // requested target architecture, and not to things like build
    // scripts and plugins, which may be for an entirely different
    // architecture. Cargo's present architecture makes it quite
    // hard to only apply flags to things that are not build
    // scripts and plugins though, so we do something more hacky
    // instead to avoid applying the same RUSTFLAGS to multiple targets
    // arches:
    //
    // 1) If --target is not specified we just apply RUSTFLAGS to
    // all builds; they are all going to have the same target.
    //
    // 2) If --target *is* specified then we only apply RUSTFLAGS
    // to compilation units with the Target kind, which indicates
    // it was chosen by the --target flag.
    //
    // This means that, e.g., even if the specified --target is the
    // same as the host, build scripts in plugins won't get
    // RUSTFLAGS.
    if !requested_kind.is_host() && kind.is_host() {
        // This is probably a build script or plugin and we're
        // compiling with --target. In this scenario there are
        // no rustflags we can apply.
        return Ok(Vec::new());
    }

    // First try RUSTFLAGS from the environment
    if let Ok(a) = env::var(name) {
        let args = a
            .split(' ')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        return Ok(args.collect());
    }

    let mut rustflags = Vec::new();

    let name = name
        .chars()
        .flat_map(|c| c.to_lowercase())
        .collect::<String>();
    // Then the target.*.rustflags value...
    let target = match &kind {
        CompileKind::Host => host_triple,
        CompileKind::Target(target) => target.short_name(),
    };
    let key = format!("target.{}.{}", target, name);
    if let Some(args) = config.get::<Option<StringList>>(&key)? {
        rustflags.extend(args.as_slice().iter().cloned());
    }
    // ...including target.'cfg(...)'.rustflags
    if let Some(target_cfg) = target_cfg {
        config
            .target_cfgs()?
            .iter()
            .filter_map(|(key, cfg)| {
                cfg.rustflags
                    .as_ref()
                    .map(|rustflags| (key, &rustflags.val))
            })
            .filter(|(key, _rustflags)| CfgExpr::matches_key(key, target_cfg))
            .for_each(|(_key, cfg_rustflags)| {
                rustflags.extend(cfg_rustflags.as_slice().iter().cloned());
            });
    }

    if !rustflags.is_empty() {
        return Ok(rustflags);
    }

    // Then the `build.rustflags` value.
    let build = config.build_config()?;
    let list = if name == "rustflags" {
        &build.rustflags
    } else {
        &build.rustdocflags
    };
    if let Some(list) = list {
        return Ok(list.as_slice().to_vec());
    }

    Ok(Vec::new())
}

/// Collection of information about `rustc` and the host and target.
pub struct RustcTargetData {
    /// Information about `rustc` itself.
    pub rustc: Rustc,
    /// Build information for the "host", which is information about when
    /// `rustc` is invoked without a `--target` flag. This is used for
    /// procedural macros, build scripts, etc.
    host_config: TargetConfig,
    host_info: TargetInfo,

    /// Build information for targets that we're building for. This will be
    /// empty if the `--target` flag is not passed, and currently also only ever
    /// has at most one entry, but eventually we'd like to support multi-target
    /// builds with Cargo.
    target_config: HashMap<CompileTarget, TargetConfig>,
    target_info: HashMap<CompileTarget, TargetInfo>,
}

impl RustcTargetData {
    pub fn new(ws: &Workspace<'_>, requested_kind: CompileKind) -> CargoResult<RustcTargetData> {
        let config = ws.config();
        let rustc = config.load_global_rustc(Some(ws))?;
        let host_config = config.target_cfg_triple(&rustc.host)?;
        let host_info = TargetInfo::new(config, requested_kind, &rustc, CompileKind::Host)?;
        let mut target_config = HashMap::new();
        let mut target_info = HashMap::new();
        if let CompileKind::Target(target) = requested_kind {
            let tcfg = config.target_cfg_triple(target.short_name())?;
            target_config.insert(target, tcfg);
            target_info.insert(
                target,
                TargetInfo::new(config, requested_kind, &rustc, CompileKind::Target(target))?,
            );
        }

        Ok(RustcTargetData {
            rustc,
            target_config,
            target_info,
            host_config,
            host_info,
        })
    }

    /// Returns a "short" name for the given kind, suitable for keying off
    /// configuration in Cargo or presenting to users.
    pub fn short_name<'a>(&'a self, kind: &'a CompileKind) -> &'a str {
        match kind {
            CompileKind::Host => &self.rustc.host,
            CompileKind::Target(target) => target.short_name(),
        }
    }

    /// Whether a dependency should be compiled for the host or target platform,
    /// specified by `CompileKind`.
    pub fn dep_platform_activated(&self, dep: &Dependency, kind: CompileKind) -> bool {
        // If this dependency is only available for certain platforms,
        // make sure we're only enabling it for that platform.
        let platform = match dep.platform() {
            Some(p) => p,
            None => return true,
        };
        let name = self.short_name(&kind);
        platform.matches(name, self.cfg(kind))
    }

    /// Gets the list of `cfg`s printed out from the compiler for the specified kind.
    pub fn cfg(&self, kind: CompileKind) -> &[Cfg] {
        self.info(kind).cfg()
    }

    /// Information about the given target platform, learned by querying rustc.
    pub fn info(&self, kind: CompileKind) -> &TargetInfo {
        match kind {
            CompileKind::Host => &self.host_info,
            CompileKind::Target(s) => &self.target_info[&s],
        }
    }

    /// Gets the target configuration for a particular host or target.
    pub fn target_config(&self, kind: CompileKind) -> &TargetConfig {
        match kind {
            CompileKind::Host => &self.host_config,
            CompileKind::Target(s) => &self.target_config[&s],
        }
    }
}
