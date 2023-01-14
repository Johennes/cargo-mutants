// Copyright 2022 Martin Pool.

//! `.cargo/mutants.toml` configuration file.
//!
//! The config file is read after parsing command line arguments,
//! and after finding the source tree, because these together
//! determine its location.
//!
//! The config file is then merged in to the [Options].

use std::default::Default;
use std::fs::read_to_string;

use anyhow::Context;
use camino::Utf8Path;
use serde::Deserialize;

use crate::Result;

/// Configuration read from a config file.
///
/// This is similar to [Options], and eventually merged into it, but separate because it
/// can be deserialized.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Generate mutants from source files matching these globs.
    pub examine_globs: Vec<String>,
    /// Exclude mutants from source files matching these globs.
    pub exclude_globs: Vec<String>,
    /// Exclude mutants from source files matches these regexps.
    pub exclude_re: Vec<String>,
    /// Examine only mutants matching these regexps.
    pub examine_re: Vec<String>,
    /// Pass extra args to every cargo invocation.
    pub additional_cargo_args: Vec<String>,
    /// Pass extra args to cargo test.
    pub additional_cargo_test_args: Vec<String>,
}

impl Config {
    pub fn read_file(path: &Utf8Path) -> Result<Config> {
        let toml = read_to_string(path).with_context(|| format!("read config {path:?}"))?;
        toml::de::from_str(&toml).with_context(|| format!("parse toml from {path:?}"))
    }

    /// Read the config from a tree's `.cargo/mutants.toml`, and return a default (empty)
    /// Config is the file does not exist.
    pub fn read_tree_config(source_tree_root: &Utf8Path) -> Result<Config> {
        let path = source_tree_root.join(".cargo").join("mutants.toml");
        if path.exists() {
            Config::read_file(&path)
        } else {
            Ok(Config::default())
        }
    }
}
