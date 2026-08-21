// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct ModuleNode {
    pub importers: HashSet<PathBuf>,
    pub imports: HashSet<PathBuf>,
    pub is_self_accepting: bool,
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
            Ok(mut boundaries) => {
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
            Ok(mut boundaries) => {
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
        seed: &'a Path,
        colors: &mut HashMap<&'a Path, Color>,
        boundaries: &mut Vec<PathBuf>,
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
                Some(Color::Gray) => {
                    return Err(format!("circular import involving {}", current.display()));
                }
                None => {}
            }
            let Some(node) = self.modules.get(current) else {
                return Err(format!("{} is not in the module graph", current.display()));
            };
            if node.is_self_accepting {
                boundaries.push(current.to_path_buf());
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
        let mut g = ModuleGraph::new();
        g.add_import(&p("a.ts"), &p("b.ts"));
        g.add_import(&p("b.ts"), &p("a.ts"));
        assert!(matches!(
            g.propagate_update(&p("a.ts")),
            HmrDecision::FullReload { .. }
        ));
    }
}
