// Copyright 2021, 2022 Martin Pool

//! Run Cargo as a subprocess, including timeouts and propagating signals.

use std::collections::BTreeSet;
use std::env;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

#[allow(unused_imports)]
use anyhow::{anyhow, bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use globset::GlobSet;
use serde_json::Value;
#[allow(unused_imports)]
use tracing::{debug, error, info, span, trace, warn, Level};

use crate::console::Console;
use crate::log_file::LogFile;
use crate::path::TreeRelativePathBuf;
use crate::process::{get_command_output, Process, ProcessStatus};
use crate::*;

/// How frequently to check if cargo finished.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Run one `cargo` subprocess, with a timeout, and with appropriate handling of interrupts.
pub fn run_cargo(
    build_dir: &BuildDir,
    argv: &[String],
    log_file: &mut LogFile,
    timeout: Duration,
    console: &Console,
    rustflags: &str,
) -> Result<ProcessStatus> {
    let start = Instant::now();

    // The tests might use Insta <https://insta.rs>, and we don't want it to write
    // updates to the source tree, and we *certainly* don't want it to write
    // updates and then let the test pass.

    let env = [
        ("CARGO_ENCODED_RUSTFLAGS", rustflags),
        ("INSTA_UPDATE", "no"),
    ];
    debug!(?env);

    let mut child = Process::start(argv, &env, build_dir.path(), timeout, log_file)?;

    let process_status = loop {
        if let Some(exit_status) = child.poll()? {
            break exit_status;
        } else {
            console.tick();
            sleep(WAIT_POLL_INTERVAL);
        }
    };

    let message = format!(
        "cargo result: {:?} in {:.3}s",
        process_status,
        start.elapsed().as_secs_f64()
    );
    log_file.message(&message);
    debug!(cargo_result = ?process_status, elapsed = ?start.elapsed());
    check_interrupted()?;
    Ok(process_status)
}

/// Return the name of the cargo binary.
fn cargo_bin() -> String {
    // When run as a Cargo subcommand, which is the usual/intended case,
    // $CARGO tells us the right way to call back into it, so that we get
    // the matching toolchain etc.
    env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned())
}

/// Make up the argv for a cargo check/build/test invocation, including argv[0] as the
/// cargo binary itself.
pub fn cargo_argv(package_name: Option<&str>, phase: Phase, options: &Options) -> Vec<String> {
    let mut cargo_args = vec![cargo_bin(), phase.name().to_string()];
    if phase == Phase::Check || phase == Phase::Build {
        cargo_args.push("--tests".to_string());
    }
    if let Some(package_name) = package_name {
        cargo_args.push("--package".to_owned());
        cargo_args.push(package_name.to_owned());
    } else {
        cargo_args.push("--workspace".to_string());
    }
    cargo_args.extend(options.additional_cargo_args.iter().cloned());
    if phase == Phase::Test {
        cargo_args.extend(options.additional_cargo_test_args.iter().cloned());
    }
    cargo_args
}

/// A source tree where we can run cargo commands.
#[derive(Debug)]
pub struct CargoSourceTree {
    pub root: Utf8PathBuf,
    cargo_toml_path: Utf8PathBuf,
}

impl CargoSourceTree {
    /// Open the source tree enclosing the given path.
    ///
    /// Returns an error if it's not found.
    pub fn open(path: &Utf8Path) -> Result<CargoSourceTree> {
        let cargo_toml_path = locate_cargo_toml(path)?;
        let root = cargo_toml_path
            .parent()
            .expect("cargo_toml_path has a parent")
            .to_owned();
        assert!(root.is_dir());

        Ok(CargoSourceTree {
            root,
            cargo_toml_path,
        })
    }

    /// Return appropriate CARGO_ENCODED_RUSTFLAGS for building this tree, including any changes to cap-lints.
    ///
    /// See <https://doc.rust-lang.org/cargo/reference/environment-variables.html>
    /// <https://doc.rust-lang.org/rustc/lints/levels.html#capping-lints>
    pub fn rustflags(&self) -> String {
        let mut rustflags: Vec<String> =
            if let Some(rustflags) = env::var_os("CARGO_ENCODED_RUSTFLAGS") {
                rustflags
                    .to_str()
                    .expect("CARGO_ENCODED_RUSTFLAGS is not valid UTF-8")
                    .split(|c| c == '\x1f')
                    .map(|s| s.to_owned())
                    .collect()
            } else if let Some(rustflags) = env::var_os("RUSTFLAGS") {
                rustflags
                    .to_str()
                    .expect("RUSTFLAGS is not valid UTF-8")
                    .split(' ')
                    .map(|s| s.to_owned())
                    .collect()
            } else {
                // TODO: Determine the right target triple and profile?
                let config_paths = enclosing_config_files(&self.root);
                debug!("search config files {config_paths:?}");
                // TODO: All matching target.<triple>.rustflags and target.<cfg>.rustflags config entries joined together.
                // TODO: build.rustflags config value.
                Vec::new()
            };
        rustflags.push("--cap-lints=allow".to_owned());
        debug!("adjusted rustflags: {:?}", rustflags);
        rustflags.join("\x1f")
    }
}

/// Run `cargo locate-project` to find the path of the `Cargo.toml` enclosing this path.
fn locate_cargo_toml(path: &Utf8Path) -> Result<Utf8PathBuf> {
    let cargo_bin = cargo_bin();
    if !path.is_dir() {
        bail!("{} is not a directory", path);
    }
    let argv: Vec<&str> = vec![&cargo_bin, "locate-project"];
    let stdout = get_command_output(&argv, path)
        .with_context(|| format!("run cargo locate-project in {path:?}"))?;
    let val: Value = serde_json::from_str(&stdout).context("parse cargo locate-project output")?;
    let cargo_toml_path: Utf8PathBuf = val["root"]
        .as_str()
        .context("cargo locate-project output has no root: {stdout:?}")?
        .to_owned()
        .into();
    assert!(cargo_toml_path.is_file());
    Ok(cargo_toml_path)
}

impl SourceTree for CargoSourceTree {
    fn path(&self) -> &Utf8Path {
        &self.root
    }

    /// Find all source files that can be mutated within a tree, including their cargo packages.
    fn source_files(&self, options: &Options) -> Result<Vec<SourceFile>> {
        debug!("cargo_toml_path = {}", self.cargo_toml_path);
        check_interrupted()?;
        let metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path(&self.cargo_toml_path)
            .exec()
            .context("run cargo metadata")?;
        check_interrupted()?;

        let mut r = Vec::new();
        for package_metadata in &metadata.workspace_packages() {
            debug!("walk package {:?}", package_metadata.manifest_path);
            let top_sources = direct_package_sources(&self.root, package_metadata)?;
            let source_paths = indirect_source_paths(
                &self.root,
                top_sources,
                &options.examine_globset,
                &options.exclude_globset,
            )?;
            let package_name = Arc::new(package_metadata.name.to_string());
            for source_path in source_paths {
                check_interrupted()?;
                r.push(SourceFile::new(
                    &self.root,
                    source_path,
                    Arc::clone(&package_name),
                )?);
            }
        }
        Ok(r)
    }
}

/// Find all the `.rs` files, by starting from the sources identified by the manifest
/// and walking down.
///
/// This just walks the directory tree rather than following `mod` statements (for now)
/// so it may pick up some files that are not actually linked in.
fn indirect_source_paths(
    root: &Utf8Path,
    top_sources: impl IntoIterator<Item = TreeRelativePathBuf>,
    examine_globset: &Option<GlobSet>,
    exclude_globset: &Option<GlobSet>,
) -> Result<BTreeSet<TreeRelativePathBuf>> {
    let dirs: BTreeSet<TreeRelativePathBuf> = top_sources.into_iter().map(|p| p.parent()).collect();
    let mut files: BTreeSet<TreeRelativePathBuf> = BTreeSet::new();
    for top_dir in dirs {
        for p in walkdir::WalkDir::new(top_dir.within(root))
            .sort_by_file_name()
            .into_iter()
        {
            let p = p.with_context(|| "error walking source tree {top_dir}")?;
            if !p.file_type().is_file() {
                continue;
            }
            let path = p.into_path();
            if !path
                .extension()
                .map_or(false, |p| p.eq_ignore_ascii_case("rs"))
            {
                continue;
            }
            let relative_path = path.strip_prefix(root).expect("strip prefix").to_owned();
            if let Some(examine_globset) = examine_globset {
                if !examine_globset.is_match(&relative_path) {
                    continue;
                }
            }
            if let Some(exclude_globset) = exclude_globset {
                if exclude_globset.is_match(&relative_path) {
                    continue;
                }
            }
            files.insert(relative_path.into());
        }
    }
    Ok(files)
}

/// Find all the files that are named in the `path` of targets in a Cargo manifest that should be tested.
///
/// These are the starting points for discovering source files.
fn direct_package_sources(
    workspace_root: &Utf8Path,
    package_metadata: &cargo_metadata::Package,
) -> Result<Vec<TreeRelativePathBuf>> {
    let mut found = Vec::new();
    let pkg_dir = package_metadata.manifest_path.parent().unwrap();
    for target in &package_metadata.targets {
        if should_mutate_target(target) {
            if let Ok(relpath) = target.src_path.strip_prefix(workspace_root) {
                let relpath = TreeRelativePathBuf::new(relpath.into());
                debug!(
                    "found mutation target {} of kind {:?}",
                    relpath, target.kind
                );
                found.push(relpath);
            } else {
                warn!("{:?} is not in {:?}", target.src_path, pkg_dir);
            }
        } else {
            debug!(
                "skipping target {:?} of kinds {:?}",
                target.name, target.kind
            );
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

fn should_mutate_target(target: &cargo_metadata::Target) -> bool {
    target.kind.iter().any(|k| k.ends_with("lib") || k == "bin")
}

/// Return a list of cargo config.toml files enclosing a directory, and in the
/// cargo home directory.
///
/// Only actually existing files are returned.
fn enclosing_config_files(path: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    // https://doc.rust-lang.org/cargo/reference/config.html#hierarchical-structure
    // NOTE: The docs are ambiguous on what order the arrays are joined; but it
    // seems to make sense to put the most-specific (first-searched) one *last*
    // so that it can override earlier values.
    // TODO: Unit test this walking up some directory tree?
    let mut path = path.canonicalize_utf8().context("canonicalize path")?;
    let mut r: Vec<Utf8PathBuf> = Vec::new();
    loop {
        for suffix in &[".cargo/config.toml", ".cargo/config"] {
            let config_path = path.join(suffix);
            if config_path.exists() {
                r.push(config_path);
                break;
            }
        }
        if let Some(parent) = path.parent() {
            path = parent.to_owned();
        } else {
            break;
        }
    }
    if let Some(cargo_home) = cargo_home() {
        for filename in ["config.toml", "config"] {
            let config_path = cargo_home.join(filename);
            if config_path.exists() {
                if !r.contains(&config_path) {
                    r.push(config_path);
                }
                break;
            }
        }
    }
    Ok(r)
}

fn cargo_home() -> Option<Utf8PathBuf> {
    if let Some(home) = env::var_os("CARGO_HOME") {
        let home = home.to_str().expect("CARGO_HOME is not valid UTF-8");
        Some(Utf8PathBuf::from(home))
    } else if let Some(home) = dirs::home_dir() {
        let home: Utf8PathBuf = home.try_into().expect("home_dir is not valid UTF-8");
        Some(home.join(".cargo"))
    } else {
        None
    }
}

#[cfg(test)]
mod test {
    use std::ffi::OsStr;

    use pretty_assertions::assert_eq;

    use crate::{Options, Phase};

    use super::*;

    #[test]
    fn generate_cargo_args_for_baseline_with_default_options() {
        let options = Options::default();
        assert_eq!(
            cargo_argv(None, Phase::Check, &options)[1..],
            ["check", "--tests", "--workspace"]
        );
        assert_eq!(
            cargo_argv(None, Phase::Build, &options)[1..],
            ["build", "--tests", "--workspace"]
        );
        assert_eq!(
            cargo_argv(None, Phase::Test, &options)[1..],
            ["test", "--workspace"]
        );
    }

    #[test]
    fn generate_cargo_args_with_additional_cargo_test_args_and_package_name() {
        let mut options = Options::default();
        let package_name = "cargo-mutants-testdata-something";
        options
            .additional_cargo_test_args
            .extend(["--lib", "--no-fail-fast"].iter().map(|s| s.to_string()));
        assert_eq!(
            cargo_argv(Some(package_name), Phase::Check, &options)[1..],
            ["check", "--tests", "--package", package_name]
        );
        assert_eq!(
            cargo_argv(Some(package_name), Phase::Build, &options)[1..],
            ["build", "--tests", "--package", package_name]
        );
        assert_eq!(
            cargo_argv(Some(package_name), Phase::Test, &options)[1..],
            ["test", "--package", package_name, "--lib", "--no-fail-fast"]
        );
    }

    #[test]
    fn generate_cargo_args_with_additional_cargo_args_and_test_args() {
        let mut options = Options::default();
        options
            .additional_cargo_test_args
            .extend(["--lib", "--no-fail-fast"].iter().map(|s| s.to_string()));
        options
            .additional_cargo_args
            .extend(["--release".to_owned()]);
        assert_eq!(
            cargo_argv(None, Phase::Check, &options)[1..],
            ["check", "--tests", "--workspace", "--release"]
        );
        assert_eq!(
            cargo_argv(None, Phase::Build, &options)[1..],
            ["build", "--tests", "--workspace", "--release"]
        );
        assert_eq!(
            cargo_argv(None, Phase::Test, &options)[1..],
            [
                "test",
                "--workspace",
                "--release",
                "--lib",
                "--no-fail-fast"
            ]
        );
    }

    #[test]
    fn error_opening_outside_of_crate() {
        CargoSourceTree::open(Utf8Path::new("/")).unwrap_err();
    }

    #[test]
    fn source_files_in_testdata_factorial() {
        let source_paths = CargoSourceTree::open(Utf8Path::new("testdata/tree/factorial"))
            .unwrap()
            .source_files(&Options::default())
            .unwrap();
        assert_eq!(source_paths.len(), 1);
        assert_eq!(
            source_paths[0].tree_relative_path().to_string(),
            "src/bin/factorial.rs",
        );
    }

    #[test]
    fn open_subdirectory_of_crate_opens_the_crate() {
        let source_tree = CargoSourceTree::open(Utf8Path::new("testdata/tree/factorial/src"))
            .expect("open source tree from subdirectory");
        let path = source_tree.path();
        assert!(path.is_dir());
        assert!(path.join("Cargo.toml").is_file());
        assert!(path.join("src/bin/factorial.rs").is_file());
        assert_eq!(path.file_name().unwrap(), OsStr::new("factorial"));
    }

    /// Either CARGO_HOME is set, or at least it can be found in HOME.
    #[test]
    fn cargo_home_is_found_in_test_environment() {
        assert!(super::cargo_home().is_some());
    }

    /// In the common case where the source is inside HOME, we still don't get duplicated config paths.
    #[test]
    fn enclosing_config_files_has_no_duplicates() {
        let paths = enclosing_config_files("testdata/tree/small_well_tested".into()).unwrap();
        for i in 0..paths.len() {
            for j in (i + 1)..(paths.len()) {
                assert_ne!(
                    paths[i], paths[j],
                    "duplicate config file found in {paths:?}"
                );
            }
        }
    }
}
