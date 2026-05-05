//! Implementation of governance coverage audits.
//!
//! Coverage reports which tracked paths are governed by rules and which
//! inventoried documents are reachable through configuration.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use miette::Result;
use serde::Serialize;

use crate::AppExit;
use crate::cli::{CoverageArgs, CoverageOutputFormat};
use crate::config::{
    ImpactFileDescriptor, LoadedCoverageConfig, LoadedDocInventoryConfig, LoadedRule,
    list_impact_files, load_coverage_configs, load_doc_inventory_configs, load_impact_files,
    resolve_rule_path, root_dir_from_option,
};
use crate::git::get_tracked_paths;
use crate::reporters::OutputWarning;
use crate::rules::matches_pattern;

pub const COVERAGE_AUDIT_SCHEMA_VERSION: &str = "docpact.coverage.v1";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CoverageReport {
    pub schema_version: String,
    pub tool_name: String,
    pub tool_version: String,
    pub command: String,
    pub warnings: Vec<OutputWarning>,
    pub rule_coverage: RuleCoverageReport,
    pub doc_reachability: DocReachabilityReport,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RuleCoverageReport {
    pub governed_path_count: usize,
    pub covered_path_count: usize,
    pub uncovered_path_count: usize,
    pub matched_rule_count: usize,
    pub dead_rule_count: usize,
    pub coverage_ratio: f64,
    pub uncovered_paths: Vec<String>,
    pub uncovered_hotspots: Vec<PathCount>,
    pub dead_rules: Vec<DeadRuleRecord>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DocReachabilityReport {
    pub inventoried_doc_count: usize,
    pub reachable_doc_count: usize,
    pub orphan_doc_count: usize,
    pub reachable_ratio: f64,
    pub orphan_docs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PathCount {
    pub pattern: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeadRuleRecord {
    pub rule_id: String,
    pub source: String,
    pub trigger_patterns: Vec<String>,
}

pub fn run(args: CoverageArgs) -> Result<AppExit> {
    let report = execute(&args)?;
    emit_report(&report, args.format);
    Ok(AppExit::Success)
}

pub fn execute(args: &CoverageArgs) -> Result<CoverageReport> {
    let root_dir = root_dir_from_option(args.root.as_deref())?;
    let impact_files = list_impact_files(&root_dir, args.config.as_deref())?;
    let tracked_paths = collect_tracked_paths(&root_dir, &impact_files)?;
    let loaded_rules = load_impact_files(&root_dir, args.config.as_deref())?;
    let coverage_configs = load_coverage_configs(&root_dir, args.config.as_deref())?;
    let doc_inventory_configs = load_doc_inventory_configs(&root_dir, args.config.as_deref())?;

    let governed_paths = tracked_paths
        .iter()
        .filter(|path| path_in_scope(path, &coverage_configs, ScopeSelector::Coverage))
        .cloned()
        .collect::<Vec<_>>();
    let covered_paths = governed_paths
        .iter()
        .filter(|path| path_matches_any_rule_trigger(path, &loaded_rules))
        .cloned()
        .collect::<Vec<_>>();
    let covered_set = covered_paths.iter().cloned().collect::<HashSet<_>>();
    let uncovered_paths = governed_paths
        .iter()
        .filter(|path| !covered_set.contains(path.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let dead_rules = loaded_rules
        .iter()
        .filter(|loaded| {
            !governed_paths.iter().any(|path| {
                loaded.rule.triggers.iter().any(|trigger| {
                    matches_pattern(path, &resolve_rule_path(&loaded.base_dir, &trigger.path))
                })
            })
        })
        .map(|loaded| DeadRuleRecord {
            rule_id: loaded.rule.id.clone(),
            source: loaded.source.clone(),
            trigger_patterns: loaded
                .rule
                .triggers
                .iter()
                .map(|trigger| resolve_rule_path(&loaded.base_dir, &trigger.path))
                .collect(),
        })
        .collect::<Vec<_>>();

    let required_docs = loaded_rules
        .iter()
        .flat_map(|loaded| {
            loaded
                .rule
                .required_docs
                .iter()
                .map(|doc| resolve_rule_path(&loaded.base_dir, &doc.path))
        })
        .collect::<BTreeSet<_>>();
    let inventoried_docs = tracked_paths
        .iter()
        .filter(|path| is_doc_path(path))
        .filter(|path| path_in_scope(path, &doc_inventory_configs, ScopeSelector::DocInventory))
        .cloned()
        .collect::<Vec<_>>();
    let orphan_docs = inventoried_docs
        .iter()
        .filter(|path| !required_docs.contains(path.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    Ok(CoverageReport {
        schema_version: COVERAGE_AUDIT_SCHEMA_VERSION.into(),
        tool_name: env!("CARGO_PKG_NAME").into(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        command: "coverage".into(),
        warnings: Vec::new(),
        rule_coverage: RuleCoverageReport {
            governed_path_count: governed_paths.len(),
            covered_path_count: covered_paths.len(),
            uncovered_path_count: uncovered_paths.len(),
            matched_rule_count: loaded_rules.len().saturating_sub(dead_rules.len()),
            dead_rule_count: dead_rules.len(),
            coverage_ratio: ratio(covered_paths.len(), governed_paths.len()),
            uncovered_paths: uncovered_paths.clone(),
            uncovered_hotspots: path_hotspots(&uncovered_paths),
            dead_rules,
        },
        doc_reachability: DocReachabilityReport {
            inventoried_doc_count: inventoried_docs.len(),
            reachable_doc_count: inventoried_docs.len().saturating_sub(orphan_docs.len()),
            orphan_doc_count: orphan_docs.len(),
            reachable_ratio: ratio(
                inventoried_docs.len().saturating_sub(orphan_docs.len()),
                inventoried_docs.len(),
            ),
            orphan_docs,
        },
    })
}

fn emit_report(report: &CoverageReport, format: CoverageOutputFormat) {
    match format {
        CoverageOutputFormat::Text => emit_text_report(report),
        CoverageOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(report).expect("coverage report should serialize")
        ),
    }
}

fn emit_text_report(report: &CoverageReport) {
    let status = if report.rule_coverage.uncovered_path_count > 0
        || report.rule_coverage.dead_rule_count > 0
        || report.doc_reachability.orphan_doc_count > 0
    {
        "attention required"
    } else {
        "pass"
    };
    println!("Docpact coverage: {status}.");
    println!(
        "Rule coverage: governed_paths={}, covered_paths={}, uncovered_paths={}, dead_rules={}, coverage_ratio={:.3}",
        report.rule_coverage.governed_path_count,
        report.rule_coverage.covered_path_count,
        report.rule_coverage.uncovered_path_count,
        report.rule_coverage.dead_rule_count,
        report.rule_coverage.coverage_ratio,
    );
    emit_top_path_counts(
        "Uncovered hotspots",
        &report.rule_coverage.uncovered_hotspots,
    );
    emit_top_dead_rules(&report.rule_coverage.dead_rules);
    println!(
        "Doc reachability: inventoried_docs={}, reachable_docs={}, orphan_docs={}, reachable_ratio={:.3}",
        report.doc_reachability.inventoried_doc_count,
        report.doc_reachability.reachable_doc_count,
        report.doc_reachability.orphan_doc_count,
        report.doc_reachability.reachable_ratio,
    );
    emit_top_paths("Orphan docs", &report.doc_reachability.orphan_docs);
    if status == "pass" {
        println!("Next: no coverage action required.");
    } else {
        println!(
            "Next: add missing rule triggers, remove dead rules, or exclude intentional orphan docs."
        );
    }
}

fn emit_top_path_counts(label: &str, items: &[PathCount]) {
    println!("{label}:");
    if items.is_empty() {
        println!("- none");
        return;
    }

    for item in items.iter().take(10) {
        println!(
            "- {} matched {} uncovered path(s)",
            item.pattern, item.count
        );
    }

    if items.len() > 10 {
        println!("- ... {} more", items.len() - 10);
    }
}

fn emit_top_dead_rules(dead_rules: &[DeadRuleRecord]) {
    println!("Dead rules:");
    if dead_rules.is_empty() {
        println!("- none");
        return;
    }

    for rule in dead_rules.iter().take(10) {
        println!(
            "- remove or update rule {} from {} (triggers: {})",
            rule.rule_id,
            rule.source,
            rule.trigger_patterns.join(",")
        );
    }

    if dead_rules.len() > 10 {
        println!("- ... {} more", dead_rules.len() - 10);
    }
}

fn emit_top_paths(label: &str, items: &[String]) {
    println!("{label}:");
    if items.is_empty() {
        println!("- none");
        return;
    }

    for item in items.iter().take(10) {
        println!("- {item}");
    }

    if items.len() > 10 {
        println!("- ... {} more", items.len() - 10);
    }
}

fn collect_tracked_paths(
    root_dir: &Path,
    impact_files: &[ImpactFileDescriptor],
) -> Result<Vec<String>> {
    let mut tracked = BTreeSet::new();

    for descriptor in impact_files {
        let repo_root = if descriptor.base_dir.is_empty() {
            root_dir.to_path_buf()
        } else {
            root_dir.join(&descriptor.base_dir)
        };

        for path in get_tracked_paths(&repo_root)? {
            let normalized = if descriptor.base_dir.is_empty() {
                path
            } else {
                resolve_rule_path(&descriptor.base_dir, &path)
            };
            tracked.insert(normalized);
        }
    }

    Ok(tracked.into_iter().collect())
}

fn path_in_scope<T>(path: &str, loaded_configs: &[T], selector: ScopeSelector) -> bool
where
    T: ScopedPatterns,
{
    if matches_any_scope_pattern(path, loaded_configs, selector, PatternKind::Exclude) {
        return false;
    }

    let has_include = loaded_configs
        .iter()
        .any(|loaded| !loaded.patterns(PatternKind::Include).is_empty());
    if !has_include {
        return true;
    }

    matches_any_scope_pattern(path, loaded_configs, selector, PatternKind::Include)
}

fn matches_any_scope_pattern<T>(
    path: &str,
    loaded_configs: &[T],
    selector: ScopeSelector,
    pattern_kind: PatternKind,
) -> bool
where
    T: ScopedPatterns,
{
    loaded_configs.iter().any(|loaded| {
        let _ = selector;
        loaded
            .patterns(pattern_kind)
            .iter()
            .any(|pattern| matches_pattern(path, &resolve_rule_path(loaded.base_dir(), pattern)))
    })
}

fn path_matches_any_rule_trigger(path: &str, loaded_rules: &[LoadedRule]) -> bool {
    loaded_rules.iter().any(|loaded| {
        loaded.rule.triggers.iter().any(|trigger| {
            matches_pattern(path, &resolve_rule_path(&loaded.base_dir, &trigger.path))
        })
    })
}

fn path_hotspots(paths: &[String]) -> Vec<PathCount> {
    let mut counts = BTreeMap::new();

    for path in paths {
        let pattern = hotspot_pattern(path);
        *counts.entry(pattern).or_insert(0usize) += 1;
    }

    let mut hotspots = counts
        .into_iter()
        .map(|(pattern, count)| PathCount { pattern, count })
        .collect::<Vec<_>>();
    hotspots.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.pattern.cmp(&right.pattern))
    });
    hotspots
}

fn hotspot_pattern(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((parent, _)) => format!("{parent}/**"),
        None => path.to_string(),
    }
}

fn is_doc_path(path: &str) -> bool {
    path.ends_with(".md") || path.ends_with(".yaml") || path.ends_with(".yml")
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        1.0
    } else {
        numerator as f64 / denominator as f64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeSelector {
    Coverage,
    DocInventory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternKind {
    Include,
    Exclude,
}

trait ScopedPatterns {
    fn base_dir(&self) -> &str;
    fn patterns(&self, pattern_kind: PatternKind) -> &[String];
}

impl ScopedPatterns for LoadedCoverageConfig {
    fn base_dir(&self) -> &str {
        &self.base_dir
    }

    fn patterns(&self, pattern_kind: PatternKind) -> &[String] {
        match pattern_kind {
            PatternKind::Include => &self.coverage.include,
            PatternKind::Exclude => &self.coverage.exclude,
        }
    }
}

impl ScopedPatterns for LoadedDocInventoryConfig {
    fn base_dir(&self) -> &str {
        &self.base_dir
    }

    fn patterns(&self, pattern_kind: PatternKind) -> &[String] {
        match pattern_kind {
            PatternKind::Include => &self.doc_inventory.include,
            PatternKind::Exclude => &self.doc_inventory.exclude,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::AppExit;
    use crate::cli::{CoverageArgs, CoverageOutputFormat};

    use super::{COVERAGE_AUDIT_SCHEMA_VERSION, execute, run};

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be valid")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{nanos}-{}", std::process::id()));
        fs::create_dir_all(&path).expect("temp dir should be created");
        path
    }

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("git should run");
        assert!(
            status.success(),
            "git command failed: git {}",
            args.join(" ")
        );
    }

    fn init_git_repo(root: &Path) {
        git(root, &["init"]);
        git(root, &["config", "user.name", "Codex"]);
        git(root, &["config", "user.email", "codex@example.com"]);
    }

    fn base_args(root: PathBuf) -> CoverageArgs {
        CoverageArgs {
            root: Some(root),
            config: None,
            format: CoverageOutputFormat::Json,
        }
    }

    #[test]
    fn repository_coverage_reports_uncovered_paths_dead_rules_and_orphans() {
        let root = temp_dir("docpact-coverage-repo");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src/api")).expect("src api dir");
        fs::create_dir_all(root.join("src/payments")).expect("src payments dir");
        fs::create_dir_all(root.join("src/commands")).expect("src commands dir");
        fs::create_dir_all(root.join("docs")).expect("docs dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
lastReviewedAt: "2026-04-21"
lastReviewedCommit: "abc"
coverage:
  include:
    - src/**
docInventory:
  include:
    - docs/**
rules:
  - id: api-rule
    scope: repo
    repo: example
    triggers:
      - path: src/api/**
        kind: code
    requiredDocs:
      - path: docs/api.md
        mode: review_or_update
    reason: api
  - id: dead-rule
    scope: repo
    repo: example
    triggers:
      - path: src/commands/**
        kind: code
    requiredDocs:
      - path: docs/commands.md
        mode: review_or_update
    reason: commands
"#,
        )
        .expect("config");

        fs::write(root.join("src/api/client.ts"), "export const api = 1;\n").expect("api");
        fs::write(
            root.join("src/payments/charge.ts"),
            "export const charge = 1;\n",
        )
        .expect("payments");
        fs::write(root.join("docs/api.md"), "# API\n").expect("api doc");
        fs::write(root.join("docs/old.md"), "# Old\n").expect("old doc");

        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);

        let report = execute(&base_args(root)).expect("coverage should execute");

        assert_eq!(report.schema_version, COVERAGE_AUDIT_SCHEMA_VERSION);
        assert_eq!(report.command, "coverage");
        assert!(report.warnings.is_empty());
        assert_eq!(report.rule_coverage.governed_path_count, 2);
        assert_eq!(report.rule_coverage.covered_path_count, 1);
        assert_eq!(
            report.rule_coverage.uncovered_paths,
            vec!["src/payments/charge.ts"]
        );
        assert_eq!(report.rule_coverage.dead_rule_count, 1);
        assert_eq!(report.rule_coverage.dead_rules[0].rule_id, "dead-rule");
        assert_eq!(report.doc_reachability.inventoried_doc_count, 2);
        assert_eq!(report.doc_reachability.orphan_docs, vec!["docs/old.md"]);
    }

    #[test]
    fn workspace_coverage_uses_repo_relative_scopes() {
        let root = temp_dir("docpact-coverage-workspace");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("root doc dir");
        fs::create_dir_all(root.join("subrepo/.docpact")).expect("subrepo doc dir");
        fs::create_dir_all(root.join("subrepo/src/api")).expect("subrepo src api dir");
        fs::create_dir_all(root.join("subrepo/src/payments")).expect("subrepo src payments dir");
        fs::create_dir_all(root.join("subrepo/docs")).expect("subrepo docs dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: workspace
lastReviewedAt: "2026-04-21"
lastReviewedCommit: "abc"
rules: []
"#,
        )
        .expect("root config");

        fs::write(
            root.join("subrepo/.docpact/config.yaml"),
            r#"
version: 1
layout: repo
lastReviewedAt: "2026-04-21"
lastReviewedCommit: "abc"
coverage:
  include:
    - src/**
docInventory:
  include:
    - docs/**
repo:
  id: subrepo
rules:
  - id: api-rule
    scope: repo
    repo: subrepo
    triggers:
      - path: src/api/**
        kind: code
    requiredDocs:
      - path: docs/api.md
        mode: review_or_update
    reason: api
"#,
        )
        .expect("subrepo config");

        fs::write(
            root.join("subrepo/src/api/client.ts"),
            "export const api = 1;\n",
        )
        .expect("api");
        fs::write(
            root.join("subrepo/src/payments/charge.ts"),
            "export const charge = 1;\n",
        )
        .expect("payments");
        fs::write(root.join("subrepo/docs/api.md"), "# API\n").expect("api doc");
        fs::write(root.join("subrepo/docs/notes.md"), "# Notes\n").expect("notes doc");

        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);

        let report = execute(&base_args(root)).expect("coverage should execute");

        assert_eq!(
            report.rule_coverage.uncovered_paths,
            vec!["subrepo/src/payments/charge.ts"]
        );
        assert_eq!(
            report.doc_reachability.orphan_docs,
            vec!["subrepo/docs/notes.md"]
        );
    }

    #[test]
    fn coverage_command_returns_success_exit() {
        let root = temp_dir("docpact-coverage-run");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
lastReviewedAt: "2026-04-21"
lastReviewedCommit: "abc"
rules: []
"#,
        )
        .expect("config");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);

        let exit = run(base_args(root)).expect("coverage run should execute");
        assert_eq!(exit, AppExit::Success);
    }
}
