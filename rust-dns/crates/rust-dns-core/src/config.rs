// SPDX-License-Identifier: GPL-3.0-only
//! Generated runtime configuration model.
//!
//! The authoritative producer is `openwrt/rust-dns/files/config.sh`; this model
//! accepts the JSON it emits (and YAML for custom configs).

use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::error::{CoreError, Result};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub plugins: Vec<PluginConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApiConfig {
    #[serde(default)]
    pub http: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginConfig {
    pub tag: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub args: Value,
}

impl RuntimeConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| CoreError::Config(format!("cannot read {}: {e}", path.display())))?;

        match serde_json::from_str(&text) {
            Ok(config) => Ok(config),
            Err(json_err) => serde_yaml_ng::from_str(&text).map_err(|yaml_err| {
                CoreError::Config(format!(
                    "invalid config {}: JSON: {json_err}; YAML: {yaml_err}",
                    path.display()
                ))
            }),
        }
    }

    pub fn plugin(&self, tag: &str) -> Option<&PluginConfig> {
        self.plugins.iter().find(|p| p.tag == tag)
    }

    pub fn plugins_of_type<'a>(
        &'a self,
        kind: &'a str,
    ) -> impl Iterator<Item = &'a PluginConfig> + 'a {
        self.plugins.iter().filter(move |p| p.kind == kind)
    }
}
