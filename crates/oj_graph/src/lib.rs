// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The server-side module graph and HMR update propagation.
//!
//! Semantics follow Vite's model: on a file change, walk up the importer
//! chain from the changed module looking for accepting boundaries. If every
//! path terminates in a boundary, send a targeted hot update; if any path
//! dead-ends at an entry without acceptance, fall back to a full reload.
//!
//! For React, a module becomes self-accepting when the Fast Refresh glue
//! decides every export is a component (the `validateRefreshBoundary` rule).
//! That wiring lands in milestone 1.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct ModuleNode {
    pub importers: HashSet<PathBuf>,
    pub imports: HashSet<PathBuf>,
    /// True when the module accepts its own hot updates
    /// (for React: all exports are components).
    pub is_self_accepting: bool,
}

#[derive(Debug, Default)]
pub struct ModuleGraph {
    modules: HashMap<PathBuf, ModuleNode>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct UpdatePlan {
    /// Modules whose instances must be discarded before re-execution
    /// (the changed module plus every non-boundary module above it,
    /// plus the boundaries themselves).
    pub dirty: Vec<PathBuf>,
    pub boundaries: Vec<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HmrDecision {
    /// Hot-update these boundary modules (deduped, deterministic order).
    Update { boundaries: Vec<PathBuf> },
    FullReload { reason: String },
}

impl ModuleGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ensure_module(&mut self, path: &Path) -> &mut ModuleNode {
        self.modules.entry(path.to_path_buf()).or_default()
    }

    pub fn add_import(&mut self, importer: &Path, imported: &Path) {
        self.ensure_module(importer).imports.insert(imported.to_path_buf());
        self.ensure_module(imported).importers.insert(importer.to_path_buf());
    }

    pub fn set_self_accepting(&mut self, path: &Path, accepting: bool) {
        self.ensure_module(path).is_self_accepting = accepting;
    }

    /// Replace `importer`'s outgoing edges with exactly `imports`,
    /// dropping reverse edges that no longer exist. Called on every
    /// (re)compile so refactored-away imports don't keep propagating.
    pub fn set_imports(&mut self, importer: &Path, imports: &[PathBuf]) {
        let stale: Vec<PathBuf> = self
            .ensure_module(importer)
            .imports
            .iter()
            .filter(|old| !imports.contains(old))
            .cloned()
            .collect();
        for old in stale {
            if let Some(node) = self.modules.get_mut(&old) {
                node.importers.remove(importer);
            }
            self.ensure_module(importer).imports.remove(&old);
        }
        for import in imports {
            self.add_import(importer, import);
        }
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.modules.contains_key(path)
    }

    /// All known module paths, sorted for deterministic output
    /// (used to emit modulepreload links).
    pub fn module_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self.modules.keys().cloned().collect();
        paths.sort();
        paths
    }

    /// Decide what to do about a change to `changed`.
    ///
    /// Mirrors Vite's `propagateUpdate`: climb the importer edges upward looking
    /// for accepting boundaries; a path ending at a non-accepting entry, or a
    /// circular import among non-accepting modules, means full reload. The climb
    /// colors each module once (see `collect_boundaries`), so a dense diamond is
    /// walked in O(V+E), not once per distinct root-to-node path.
    pub fn propagate_update(&self, changed: &Path) -> HmrDecision {
        if !self.modules.contains_key(changed) {
            return HmrDecision::FullReload {
                reason: format!("{} is not in the module graph", changed.display()),
            };
        }
        match self.collect_boundaries(&[changed], &[]) {
            Ok(mut boundaries) => {
                boundaries.sort();
                boundaries.dedup();
                HmrDecision::Update { boundaries }
            }
            Err(reason) => HmrDecision::FullReload { reason },
        }
    }

    /// Bundle-mode patch planning: like `propagate_update`, but also returns
    /// every module on the paths from `changed` to its boundaries. The
    /// runtime must invalidate that whole set; intermediate modules hold
    /// references to the old instances' export namespaces.
    pub fn update_plan(&self, changed: &Path) -> Result<UpdatePlan, String> {
        match self.propagate_update(changed) {
            HmrDecision::FullReload { reason } => Err(reason),
            HmrDecision::Update { boundaries } => {
                // Re-walk collecting the visited chain (boundaries stop it).
                let mut dirty: Vec<PathBuf> = vec![changed.to_path_buf()];
                let mut queue = vec![changed.to_path_buf()];
                let mut seen: HashSet<PathBuf> = queue.iter().cloned().collect();
                while let Some(current) = queue.pop() {
                    let node = &self.modules[&current];
                    if node.is_self_accepting && current != changed {
                        continue; // boundary: re-executes, but stop climbing
                    }
                    for importer in &node.importers {
                        if seen.insert(importer.clone()) {
                            dirty.push(importer.clone());
                            queue.push(importer.clone());
                        }
                    }
                }
                dirty.sort();
                Ok(UpdatePlan { dirty, boundaries })
            }
        }
    }

    /// Propagation for `hot.invalidate`: the module rejected its own update
    /// at runtime (e.g. export shape changed), so restart the walk from its
    /// importers as if it were not self-accepting.
    pub fn propagate_update_from_importers(&self, changed: &Path) -> HmrDecision {
        let Some(node) = self.modules.get(changed) else {
            return HmrDecision::FullReload {
                reason: format!("{} is not in the module graph", changed.display()),
            };
        };
        if node.importers.is_empty() {
            return HmrDecision::FullReload {
                reason: format!("{} invalidated at an entry", changed.display()),
            };
        }

        // Seed the walk at `changed`'s importers with `changed` itself already
        // on the stack (gray), so a cycle back into it is caught and it is never
        // re-expanded as a boundary (it just rejected its own update).
        let seeds: Vec<&Path> = node.importers.iter().map(PathBuf::as_path).collect();
        match self.collect_boundaries(&seeds, &[changed]) {
            Ok(mut boundaries) => {
                boundaries.sort();
                boundaries.dedup();
                HmrDecision::Update { boundaries }
            }
            Err(reason) => HmrDecision::FullReload { reason },
        }
    }

    /// Climb importer edges from every seed, recording each self-accepting
    /// module as a boundary and stopping there. A single coloring visits each
    /// module at most once: `Gray` marks a module on the current DFS stack, so
    /// reaching one is a back-edge — a cycle among non-accepting modules, which
    /// forces a full reload; `Black` marks a fully-resolved module whose upward
    /// closure is already cycle-free and boundary-terminated, so it is skipped
    /// on any later path. A non-accepting module with no importers is an entry
    /// dead-end. `pre_stack` modules start gray (the changed module for the
    /// `hot.invalidate` walk): a cycle back into them is caught, and they are
    /// never treated as boundaries. This is the same decision the old per-chain
    /// DFS made, computed in O(V+E) instead of once per distinct path.
    fn collect_boundaries<'a>(
        &'a self,
        seeds: &[&'a Path],
        pre_stack: &[&'a Path],
    ) -> Result<Vec<PathBuf>, String> {
        let mut colors: HashMap<&'a Path, Color> = HashMap::new();
        for module in pre_stack {
            colors.insert(module, Color::Gray);
        }
        let mut boundaries: Vec<PathBuf> = Vec::new();
        for seed in seeds {
            self.climb(seed, &mut colors, &mut boundaries)?;
        }
        Ok(boundaries)
    }

    fn climb<'a>(
        &'a self,
        current: &'a Path,
        colors: &mut HashMap<&'a Path, Color>,
        boundaries: &mut Vec<PathBuf>,
    ) -> Result<(), String> {
        match colors.get(current) {
            // Already fully resolved: its boundaries were recorded on first
            // visit and its closure is clean, so nothing to redo.
            Some(Color::Black) => return Ok(()),
            // Back-edge onto the current stack: an import cycle among
            // non-accepting modules.
            Some(Color::Gray) => {
                return Err(format!("circular import involving {}", current.display()));
            }
            None => {}
        }
        let node = &self.modules[current];
        if node.is_self_accepting {
            boundaries.push(current.to_path_buf());
            colors.insert(current, Color::Black);
            return Ok(());
        }
        if node.importers.is_empty() {
            return Err(format!(
                "update reached entry {} with no accepting boundary",
                current.display()
            ));
        }
        colors.insert(current, Color::Gray);
        // Deterministic order so a full-reload reason (first cycle/entry hit) is
        // stable across runs.
        let mut importers: Vec<&Path> = node.importers.iter().map(PathBuf::as_path).collect();
        importers.sort();
        for importer in importers {
            self.climb(importer, colors, boundaries)?;
        }
        colors.insert(current, Color::Black);
        Ok(())
    }
}

/// DFS coloring for `collect_boundaries`: `Gray` is on the current stack (a
/// back-edge to it is a cycle), `Black` is fully resolved (skip on revisit).
#[derive(Clone, Copy, PartialEq)]
enum Color {
    Gray,
    Black,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// main.tsx imports App.tsx imports Button.tsx, where the components self-accept.
    fn graph() -> ModuleGraph {
        let mut g = ModuleGraph::new();
        g.add_import(&p("main.tsx"), &p("App.tsx"));
        g.add_import(&p("App.tsx"), &p("Button.tsx"));
        g.set_self_accepting(&p("App.tsx"), true);
        g.set_self_accepting(&p("Button.tsx"), true);
        g
    }

    #[test]
    fn change_to_self_accepting_leaf_updates_in_place() {
        let decision = graph().propagate_update(&p("Button.tsx"));
        assert_eq!(decision, HmrDecision::Update { boundaries: vec![p("Button.tsx")] });
    }

    #[test]
    fn change_propagates_up_to_nearest_boundary() {
        let mut g = graph();
        g.add_import(&p("Button.tsx"), &p("utils.ts"));
        let decision = g.propagate_update(&p("utils.ts"));
        assert_eq!(decision, HmrDecision::Update { boundaries: vec![p("Button.tsx")] });
    }

    #[test]
    fn dead_end_at_entry_forces_full_reload() {
        let mut g = graph();
        g.set_self_accepting(&p("App.tsx"), false);
        g.add_import(&p("App.tsx"), &p("config.ts"));
        // config.ts up through App.tsx (not accepting) to main.tsx (entry, not accepting)
        assert!(matches!(
            g.propagate_update(&p("config.ts")),
            HmrDecision::FullReload { .. }
        ));
    }

    #[test]
    fn unknown_module_forces_full_reload() {
        assert!(matches!(
            graph().propagate_update(&p("never-imported.ts")),
            HmrDecision::FullReload { .. }
        ));
    }

    #[test]
    fn update_plan_collects_dirty_chain_through_non_boundaries() {
        // utils.ts up to Button.tsx (boundary): dirty = {utils, Button}
        let mut g = graph();
        g.add_import(&p("Button.tsx"), &p("utils.ts"));
        let plan = g.update_plan(&p("utils.ts")).unwrap();
        assert_eq!(plan.boundaries, vec![p("Button.tsx")]);
        assert_eq!(plan.dirty, vec![p("Button.tsx"), p("utils.ts")]);
        // Boundary stops the climb: App.tsx is not dirty.
        assert!(!plan.dirty.contains(&p("App.tsx")));
        // Dead end: Err(reason) for full reload.
        assert!(g.update_plan(&p("main.tsx")).is_err());
    }

    #[test]
    fn invalidate_skips_own_acceptance_and_climbs_to_importer() {
        // Button invalidates itself, so App (accepting) becomes the boundary.
        let decision = graph().propagate_update_from_importers(&p("Button.tsx"));
        assert_eq!(decision, HmrDecision::Update { boundaries: vec![p("App.tsx")] });
        // App invalidates itself, so main.tsx is a non-accepting entry.
        assert!(matches!(
            graph().propagate_update_from_importers(&p("App.tsx")),
            HmrDecision::FullReload { .. }
        ));
    }

    #[test]
    fn set_imports_drops_stale_reverse_edges() {
        let mut g = graph();
        g.add_import(&p("App.tsx"), &p("old-dep.ts"));
        // App refactored: now only imports Button.
        g.set_imports(&p("App.tsx"), &[p("Button.tsx")]);
        // old-dep no longer reaches a boundary through App: full reload,
        // because nothing imports it anymore.
        assert!(matches!(
            g.propagate_update(&p("old-dep.ts")),
            HmrDecision::FullReload { .. }
        ));
        // Button still propagates normally.
        assert_eq!(
            g.propagate_update(&p("Button.tsx")),
            HmrDecision::Update { boundaries: vec![p("Button.tsx")] }
        );
    }

    #[test]
    fn diamond_graphs_dedupe_boundaries() {
        // shared.ts imported by both A and B (accepting); A,B under main.
        let mut g = ModuleGraph::new();
        for parent in ["A.tsx", "B.tsx"] {
            g.add_import(&p("main.tsx"), &p(parent));
            g.add_import(&p(parent), &p("shared.ts"));
            g.set_self_accepting(&p(parent), true);
        }
        let decision = g.propagate_update(&p("shared.ts"));
        assert_eq!(
            decision,
            HmrDecision::Update { boundaries: vec![p("A.tsx"), p("B.tsx")] }
        );
        let plan = g.update_plan(&p("shared.ts")).unwrap();
        assert_eq!(plan.dirty, vec![p("A.tsx"), p("B.tsx"), p("shared.ts")]);
    }

    #[test]
    fn diamond_ladder_is_linear_not_exponential() {
        // A ladder of stacked diamonds: leaf M0 is imported by A0 and B0, both
        // imported by M1, imported by A1/B1, ... up to Mn under an accepting
        // Root. There are 2^n distinct paths from M0 to Root, so the old
        // per-chain DFS could not finish; the colored walk visits each node once.
        let n = 60;
        let mut g = ModuleGraph::new();
        let m = |i: usize| p(&format!("M{i}.ts"));
        for i in 0..n {
            for side in ["A", "B"] {
                let mid = p(&format!("{side}{i}.ts"));
                g.add_import(&mid, &m(i)); // mid imports Mi
                g.add_import(&m(i + 1), &mid); // M(i+1) imports mid
            }
        }
        g.add_import(&p("Root.tsx"), &m(n));
        g.set_self_accepting(&p("Root.tsx"), true);

        // Resolves (fast) to the single boundary, no exponential blow-up.
        assert_eq!(
            g.propagate_update(&m(0)),
            HmrDecision::Update { boundaries: vec![p("Root.tsx")] }
        );
    }

    #[test]
    fn import_cycles_do_not_hang_propagation() {
        let mut g = ModuleGraph::new();
        g.add_import(&p("a.ts"), &p("b.ts"));
        g.add_import(&p("b.ts"), &p("a.ts"));
        assert!(matches!(g.propagate_update(&p("a.ts")), HmrDecision::FullReload { .. }));
    }
}
