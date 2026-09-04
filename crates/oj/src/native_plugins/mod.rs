// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The compiled-in plugin set. First-party native plugins live here behind
//! cargo features (default off); `factory()` hands the dev server and the
//! build a closure that, given the loaded config, registers the ones that are
//! active for this app. With no feature enabled there is no factory and both
//! hosts run exactly as before.

#[cfg(feature = "example-plugin")]
pub mod marker;

#[cfg(feature = "example-plugin")]
pub fn factory() -> Option<oj_server::NativePluginFactory> {
    Some(std::sync::Arc::new(|root: &std::path::Path, config: &oj_config::OjConfig| {
        let mut registry = oj_plugin::Registry::new(root);
        if let Some(plugin) = marker::MarkerPlugin::from_config(config) {
            if let Err(e) = registry.register(std::sync::Arc::new(plugin)) {
                eprintln!("oj: {e}");
            }
        }
        registry
    }))
}

#[cfg(not(feature = "example-plugin"))]
pub fn factory() -> Option<oj_server::NativePluginFactory> {
    None
}
