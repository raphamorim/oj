// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct ModuleNode {
    pub importers: HashSet<PathBuf>,
    pub imports: HashSet<PathBuf>,
    pub is_self_accepting: bool,
    /// When this module was last invalidated by an HMR update (Vite's
    /// `lastHMRTimestamp`); 0 until the first update touches it. An importer
    /// re-fetched during HMR appends `?t=<timestamp>` to its import of a stamped
    /// module, so the browser loads the new version instead of its cached one.
    pub last_hmr_timestamp: u64,
    /// Dependencies this module accepts updates for via
    /// `import.meta.hot.accept(deps, cb)` (Vite's `acceptedHmrDeps`): an update
    /// of one of them stops here with this module as the boundary.
    pub accepted_hmr_deps: HashSet<PathBuf>,
}

#[derive(Debug, Default)]
pub struct ModuleGraph {
    modules: HashMap<PathBuf, ModuleNode>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct UpdatePlan {
    pub dirty: Vec<PathBuf>,
    pub boundaries: Vec<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HmrDecision {
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
        self.ensure_module(importer)
            .imports
            .insert(imported.to_path_buf());
        self.ensure_module(imported)
            .importers
            .insert(importer.to_path_buf());
    }

    pub fn set_self_accepting(&mut self, path: &Path, accepting: bool) {
        self.ensure_module(path).is_self_accepting = accepting;
    }

    pub fn set_accepted_deps(&mut self, path: &Path, deps: &[PathBuf]) {
        self.ensure_module(path).accepted_hmr_deps = deps.iter().cloned().collect();
    }

    fn accepts_dep(&self, importer: &Path, dep: &Path) -> bool {
        self.modules
            .get(importer)
            .is_some_and(|n| n.accepted_hmr_deps.contains(dep))
    }

    /// The update targets for a change: `(boundary, acceptedPath)` pairs. A
    /// self-accepting boundary accepts itself; an importer that declared the
    /// changed module (or a module on the way up) in `hot.accept(deps)` is the
    /// boundary for that dependency. `Err` means a full reload.
    pub fn update_targets(&self, changed: &Path) -> Result<Vec<(PathBuf, PathBuf)>, String> {
        if !self.modules.contains_key(changed) {
            return Err(format!("{} is not in the module graph", changed.display()));
        }
        let mut targets = self.collect_boundaries(&[changed], &[])?;
        targets.sort();
        targets.dedup();
        Ok(targets)
    }

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

    pub fn module_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self.modules.keys().cloned().collect();
        paths.sort();
        paths
    }

    pub fn propagate_update(&self, changed: &Path) -> HmrDecision {
        if !self.modules.contains_key(changed) {
            return HmrDecision::FullReload {
                reason: format!("{} is not in the module graph", changed.display()),
            };
        }
        match self.collect_boundaries(&[changed], &[]) {
            Ok(targets) => {
                let mut boundaries: Vec<PathBuf> = targets.into_iter().map(|(b, _)| b).collect();
                boundaries.sort();
                boundaries.dedup();
                HmrDecision::Update { boundaries }
            }
            Err(reason) => HmrDecision::FullReload { reason },
        }
    }

    pub fn node(&self, path: &Path) -> Option<&ModuleNode> {
        self.modules.get(path)
    }

    /// Record an HMR update of `changed`: stamp it and every module it dirties
    /// (its importers up to and including the accepting boundaries) with
    /// `timestamp`, and return that dirty set. Mirrors Vite's
    /// `moduleGraph.invalidateModule` walking importers.
    pub fn stamp_update(&mut self, changed: &Path, timestamp: u64) -> Vec<PathBuf> {
        let dirty = self.dirty_closure(changed);
        for path in &dirty {
            if let Some(node) = self.modules.get_mut(path) {
                node.last_hmr_timestamp = timestamp;
            }
        }
        dirty
    }

    pub fn hmr_timestamp(&self, path: &Path) -> u64 {
        self.modules
            .get(path)
            .map(|n| n.last_hmr_timestamp)
            .unwrap_or(0)
    }

    /// The newest HMR timestamp among a module's direct imports: part of the
    /// module's compile cache key, so a cached importer is recompiled (and its
    /// import specifiers re-stamped) after one of its imports updates.
    pub fn imports_timestamp(&self, path: &Path) -> u64 {
        self.modules
            .get(path)
            .map(|n| {
                n.imports
                    .iter()
                    .map(|i| self.hmr_timestamp(i))
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    }

    pub fn propagate_from_seeds(&self, seeds: &[&Path]) -> HmrDecision {
        for seed in seeds {
            if !self.modules.contains_key(*seed) {
                return HmrDecision::FullReload {
                    reason: format!("{} is not in the module graph", seed.display()),
                };
            }
        }
        match self.collect_boundaries(seeds, &[]) {
            Ok(targets) => {
                let mut boundaries: Vec<PathBuf> = targets.into_iter().map(|(b, _)| b).collect();
                boundaries.sort();
                boundaries.dedup();
                HmrDecision::Update { boundaries }
            }
            Err(reason) => HmrDecision::FullReload { reason },
        }
    }

    pub fn update_plan(&self, changed: &Path) -> Result<UpdatePlan, String> {
        match self.propagate_update(changed) {
            HmrDecision::FullReload { reason } => Err(reason),
            HmrDecision::Update { boundaries } => Ok(UpdatePlan {
                dirty: self.dirty_closure(changed),
                boundaries,
            }),
        }
    }

    pub fn update_plan_from_importers(&self, changed: &Path) -> Result<UpdatePlan, String> {
        match self.propagate_update_from_importers(changed) {
            HmrDecision::FullReload { reason } => Err(reason),
            HmrDecision::Update { boundaries } => Ok(UpdatePlan {
                dirty: self.dirty_closure(changed),
                boundaries,
            }),
        }
    }

    fn dirty_closure(&self, changed: &Path) -> Vec<PathBuf> {
        let mut dirty: Vec<PathBuf> = vec![changed.to_path_buf()];
        let mut queue = vec![changed.to_path_buf()];
        let mut seen: HashSet<PathBuf> = queue.iter().cloned().collect();
        while let Some(current) = queue.pop() {
            let Some(node) = self.modules.get(&current) else {
                continue;
            };
            if node.is_self_accepting && current != changed {
                continue;
            }
            for importer in &node.importers {
                // A dep-accepting importer is not re-fetched: its callback receives
                // the new dependency module instead.
                if self.accepts_dep(importer, &current) {
                    continue;
                }
                if seen.insert(importer.clone()) {
                    dirty.push(importer.clone());
                    queue.push(importer.clone());
                }
            }
        }
        dirty.sort();
        dirty
    }

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

        let seeds: Vec<&Path> = node.importers.iter().map(PathBuf::as_path).collect();
        match self.collect_boundaries(&seeds, &[changed]) {
            Ok(targets) => {
                let mut boundaries: Vec<PathBuf> = targets.into_iter().map(|(b, _)| b).collect();
                boundaries.sort();
                boundaries.dedup();
                HmrDecision::Update { boundaries }
            }
            Err(reason) => HmrDecision::FullReload { reason },
        }
    }

    fn collect_boundaries<'a>(
        &'a self,
        seeds: &[&'a Path],
        pre_stack: &[&'a Path],
    ) -> Result<Vec<(PathBuf, PathBuf)>, String> {
        let mut colors: HashMap<&'a Path, Color> = HashMap::new();
        for module in pre_stack {
            colors.insert(module, Color::Gray);
        }
        let mut boundaries: Vec<(PathBuf, PathBuf)> = Vec::new();
        for seed in seeds {
            self.climb(seed, &mut colors, &mut boundaries)?;
        }
        Ok(boundaries)
    }

    fn climb<'a>(
        &'a self,
        seed: &'a Path,
        colors: &mut HashMap<&'a Path, Color>,
        boundaries: &mut Vec<(PathBuf, PathBuf)>,
    ) -> Result<(), String> {
        enum Step<'s> {
            Enter(&'s Path),
            Blacken(&'s Path),
        }

        let mut stack = vec![Step::Enter(seed)];
        while let Some(step) = stack.pop() {
            let current = match step {
                Step::Blacken(path) => {
                    colors.insert(path, Color::Black);
                    continue;
                }
                Step::Enter(path) => path,
            };
            match colors.get(current) {
                Some(Color::Black) => continue,
                // An importer already on the current path is a circular import.
                // Vite skips it (flagging `isWithinCircularImport`) and keeps
                // searching the other importers for a boundary; only reaching an
                // entry with no accepting module forces a full reload. Barrel-file
                // cycles are common and must not turn every edit into a reload.
                Some(Color::Gray) => continue,
                None => {}
            }
            let Some(node) = self.modules.get(current) else {
                return Err(format!("{} is not in the module graph", current.display()));
            };
            if node.is_self_accepting {
                boundaries.push((current.to_path_buf(), current.to_path_buf()));
                colors.insert(current, Color::Black);
                continue;
            }
            if node.importers.is_empty() {
                return Err(format!(
                    "update reached entry {} with no accepting boundary",
                    current.display()
                ));
            }
            colors.insert(current, Color::Gray);
            stack.push(Step::Blacken(current));
            let mut importers: Vec<&Path> = node.importers.iter().map(PathBuf::as_path).collect();
            importers.sort_unstable();
            for importer in importers.into_iter().rev() {
                // An importer that accepts `current` via hot.accept(deps) is the
                // boundary for this change; the walk does not continue above it.
                if self.accepts_dep(importer, current) {
                    boundaries.push((importer.to_path_buf(), current.to_path_buf()));
                    continue;
                }
                stack.push(Step::Enter(importer));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Color {
    Gray,
    Black,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_update_marks_the_chain_up_to_the_boundary() {
        // main -> App (boundary) -> hooks -> utils
        let mut g = ModuleGraph::new();
        let (main, app, hooks, utils) = (
            Path::new("/src/main.tsx"),
            Path::new("/src/App.tsx"),
            Path::new("/src/hooks.ts"),
            Path::new("/src/utils.ts"),
        );
        g.add_import(main, app);
        g.add_import(app, hooks);
        g.add_import(hooks, utils);
        g.set_self_accepting(app, true);

        let mut dirty = g.stamp_update(utils, 7);
        dirty.sort();
        assert_eq!(dirty, vec![app.to_path_buf(), hooks.to_path_buf(), utils.to_path_buf()]);
        assert_eq!(g.hmr_timestamp(utils), 7);
        assert_eq!(g.hmr_timestamp(hooks), 7);
        assert_eq!(g.hmr_timestamp(app), 7, "the boundary is re-fetched, so it is stamped");
        assert_eq!(g.hmr_timestamp(main), 0, "above the boundary nothing is re-fetched");
        assert_eq!(g.hmr_timestamp(Path::new("/nope.ts")), 0);
    }

    #[test]
    fn imports_timestamp_is_the_newest_direct_import_stamp() {
        let mut g = ModuleGraph::new();
        let (app, a, b) = (Path::new("/App.tsx"), Path::new("/a.ts"), Path::new("/b.ts"));
        g.add_import(app, a);
        g.add_import(app, b);
        g.set_self_accepting(app, true);
        assert_eq!(g.imports_timestamp(app), 0);
        g.stamp_update(a, 3);
        g.stamp_update(b, 9);
        assert_eq!(g.imports_timestamp(app), 9);
        // A later update of `a` moves the key again even though `b` is newer than 3 was.
        g.stamp_update(a, 12);
        assert_eq!(g.imports_timestamp(app), 12);
        assert_eq!(g.imports_timestamp(a), 0, "leaf has no imports");
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

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
        assert_eq!(
            decision,
            HmrDecision::Update {
                boundaries: vec![p("Button.tsx")]
            }
        );
    }

    #[test]
    fn change_propagates_up_to_nearest_boundary() {
        let mut g = graph();
        g.add_import(&p("Button.tsx"), &p("utils.ts"));
        let decision = g.propagate_update(&p("utils.ts"));
        assert_eq!(
            decision,
            HmrDecision::Update {
                boundaries: vec![p("Button.tsx")]
            }
        );
    }

    #[test]
    fn propagate_from_seeds_matches_single_seed_propagation() {
        let mut g = graph();
        g.add_import(&p("Button.tsx"), &p("utils.ts"));
        let seed = p("utils.ts");
        let via_seeds = g.propagate_from_seeds(&[seed.as_path()]);
        assert_eq!(
            via_seeds,
            HmrDecision::Update {
                boundaries: vec![p("Button.tsx")]
            }
        );
    }

    #[test]
    fn propagate_from_seeds_dedupes_shared_boundaries() {
        let mut g = graph();
        g.add_import(&p("Button.tsx"), &p("a.ts"));
        g.add_import(&p("Button.tsx"), &p("b.ts"));
        let a = p("a.ts");
        let b = p("b.ts");
        let decision = g.propagate_from_seeds(&[a.as_path(), b.as_path()]);
        assert_eq!(
            decision,
            HmrDecision::Update {
                boundaries: vec![p("Button.tsx")]
            }
        );
    }

    #[test]
    fn propagate_from_seeds_unknown_seed_forces_full_reload() {
        let g = graph();
        let missing = p("does-not-exist.ts");
        assert!(matches!(
            g.propagate_from_seeds(&[missing.as_path()]),
            HmrDecision::FullReload { .. }
        ));
    }

    #[test]
    fn node_returns_known_modules_only() {
        let g = graph();
        assert!(g.node(&p("App.tsx")).is_some());
        assert!(g.node(&p("never-seen.ts")).is_none());
    }

    #[test]
    fn dead_end_at_entry_forces_full_reload() {
        let mut g = graph();
        g.set_self_accepting(&p("App.tsx"), false);
        g.add_import(&p("App.tsx"), &p("config.ts"));
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
        let mut g = graph();
        g.add_import(&p("Button.tsx"), &p("utils.ts"));
        let plan = g.update_plan(&p("utils.ts")).unwrap();
        assert_eq!(plan.boundaries, vec![p("Button.tsx")]);
        assert_eq!(plan.dirty, vec![p("Button.tsx"), p("utils.ts")]);
        assert!(!plan.dirty.contains(&p("App.tsx")));
        assert!(g.update_plan(&p("main.tsx")).is_err());
    }

    #[test]
    fn invalidate_skips_own_acceptance_and_climbs_to_importer() {
        let decision = graph().propagate_update_from_importers(&p("Button.tsx"));
        assert_eq!(
            decision,
            HmrDecision::Update {
                boundaries: vec![p("App.tsx")]
            }
        );
        assert!(matches!(
            graph().propagate_update_from_importers(&p("App.tsx")),
            HmrDecision::FullReload { .. }
        ));
    }

    #[test]
    fn set_imports_drops_stale_reverse_edges() {
        let mut g = graph();
        g.add_import(&p("App.tsx"), &p("old-dep.ts"));
        g.set_imports(&p("App.tsx"), &[p("Button.tsx")]);
        assert!(matches!(
            g.propagate_update(&p("old-dep.ts")),
            HmrDecision::FullReload { .. }
        ));
        assert_eq!(
            g.propagate_update(&p("Button.tsx")),
            HmrDecision::Update {
                boundaries: vec![p("Button.tsx")]
            }
        );
    }

    #[test]
    fn diamond_graphs_dedupe_boundaries() {
        let mut g = ModuleGraph::new();
        for parent in ["A.tsx", "B.tsx"] {
            g.add_import(&p("main.tsx"), &p(parent));
            g.add_import(&p(parent), &p("shared.ts"));
            g.set_self_accepting(&p(parent), true);
        }
        let decision = g.propagate_update(&p("shared.ts"));
        assert_eq!(
            decision,
            HmrDecision::Update {
                boundaries: vec![p("A.tsx"), p("B.tsx")]
            }
        );
        let plan = g.update_plan(&p("shared.ts")).unwrap();
        assert_eq!(plan.dirty, vec![p("A.tsx"), p("B.tsx"), p("shared.ts")]);
    }

    #[test]
    fn invalidate_plan_escalates_to_importer_boundaries() {
        let plan = graph()
            .update_plan_from_importers(&p("Button.tsx"))
            .unwrap();
        assert_eq!(plan.boundaries, vec![p("App.tsx")]);
        assert!(
            plan.dirty.contains(&p("Button.tsx")),
            "changed module is dirty"
        );
        assert!(
            plan.dirty.contains(&p("App.tsx")),
            "importer boundary is dirty"
        );
        assert!(graph().update_plan_from_importers(&p("App.tsx")).is_err());
    }

    #[test]
    fn diamond_ladder_is_linear_not_exponential() {
        let n = 60;
        let mut g = ModuleGraph::new();
        let m = |i: usize| p(&format!("M{i}.ts"));
        for i in 0..n {
            for side in ["A", "B"] {
                let mid = p(&format!("{side}{i}.ts"));
                g.add_import(&mid, &m(i));
                g.add_import(&m(i + 1), &mid);
            }
        }
        g.add_import(&p("Root.tsx"), &m(n));
        g.set_self_accepting(&p("Root.tsx"), true);

        assert_eq!(
            g.propagate_update(&m(0)),
            HmrDecision::Update {
                boundaries: vec![p("Root.tsx")]
            }
        );
    }

    #[test]
    fn import_cycles_do_not_hang_propagation() {
        // A pure cycle with no entry and no boundary: nothing is loaded from it,
        // so, like Vite, there is nothing to update and nothing to reload.
        let mut g = ModuleGraph::new();
        g.add_import(&p("a.ts"), &p("b.ts"));
        g.add_import(&p("b.ts"), &p("a.ts"));
        assert_eq!(
            g.propagate_update(&p("a.ts")),
            HmrDecision::Update { boundaries: vec![] }
        );
    }

    #[test]
    fn dep_accepting_importer_is_the_boundary_for_that_dep() {
        // main -> util ; main declares hot.accept(['/util.ts'], cb) but is not self-accepting.
        let mut g = ModuleGraph::new();
        g.add_import(&p("main.ts"), &p("util.ts"));
        g.set_accepted_deps(&p("main.ts"), &[p("util.ts")]);
        assert_eq!(
            g.update_targets(&p("util.ts")).unwrap(),
            vec![(p("main.ts"), p("util.ts"))]
        );
        assert_eq!(
            g.propagate_update(&p("util.ts")),
            HmrDecision::Update { boundaries: vec![p("main.ts")] }
        );
        // The importer is not re-fetched, so it is not dirty.
        assert_eq!(g.stamp_update(&p("util.ts"), 1), vec![p("util.ts")]);
        // Editing main itself still reaches the entry with no boundary.
        assert!(matches!(g.propagate_update(&p("main.ts")), HmrDecision::FullReload { .. }));
        // A self-accepting module reports itself as acceptedPath.
        g.set_self_accepting(&p("main.ts"), true);
        assert_eq!(g.update_targets(&p("main.ts")).unwrap(), vec![(p("main.ts"), p("main.ts"))]);
    }

    #[test]
    fn a_cycle_on_the_way_to_a_boundary_does_not_force_a_reload() {
        // App (boundary) -> a <-> b ; editing b must hot-update App.
        let mut g = ModuleGraph::new();
        g.add_import(&p("App.tsx"), &p("a.ts"));
        g.add_import(&p("a.ts"), &p("b.ts"));
        g.add_import(&p("b.ts"), &p("a.ts"));
        g.set_self_accepting(&p("App.tsx"), true);
        assert_eq!(
            g.propagate_update(&p("b.ts")),
            HmrDecision::Update { boundaries: vec![p("App.tsx")] }
        );
        // ...but a cycle whose other importer path reaches an entry still reloads.
        g.add_import(&p("main.ts"), &p("b.ts"));
        assert!(matches!(g.propagate_update(&p("b.ts")), HmrDecision::FullReload { .. }));
    }
}
