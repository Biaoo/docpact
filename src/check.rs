//! Implementation of `docpact lint`.
//!
//! Lint evaluates an explicit diff source against configured documentation
//! rules, writes a diagnostics artifact, and returns an exit status suitable for
//! CI gates.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use miette::Result;
use yaml_serde::Value;

use crate::AppExit;
use crate::baseline::{apply_baseline, read_baseline_file};
use crate::cli::LintArgs;
use crate::config::{
    LoadedCoverageConfig, load_coverage_configs, load_yaml_value, parse_yaml_value,
    resolve_rule_path, root_dir_from_option,
};
use crate::diagnostics::{
    display_report_path, resolve_report_output_path, write_diagnostics_artifact,
};
use crate::freshness::execute_lint_for_matched_rules;
use crate::git::{FileComparison, get_changed_paths, get_file_comparison};
use crate::metadata::{
    build_doc_problems, markdown_body, missing_markdown_review_metadata,
    missing_yaml_review_metadata_from_value, parse_frontmatter_scalar_values,
};
use crate::reporters::{
    Problem, build_diagnostics_artifact_with_freshness, emit_lint_output, emit_report_hint,
};
use crate::rules::{MatchedRule, RequiredDocMode, match_rules, matches_pattern};
use crate::waiver::{apply_waivers, current_local_date, read_waiver_file};

#[derive(Debug, Clone)]
pub struct CheckRun {
    pub problems: Vec<Problem>,
    pub changed_paths: Vec<String>,
    pub matched_rules: Vec<MatchedRule>,
}

pub fn run(args: LintArgs) -> Result<AppExit> {
    let run = execute(&args)?;
    let root_dir = root_dir_from_option(args.root.as_deref())?;
    let lint_freshness =
        execute_lint_for_matched_rules(&root_dir, args.config.as_deref(), &run.matched_rules)?;
    let artifact = build_diagnostics_artifact_with_freshness(
        &run.problems,
        &run.changed_paths,
        run.matched_rules.len(),
        Some(&lint_freshness),
    );
    let mut artifact = artifact;
    if let Some(baseline_path) = args.baseline.as_deref() {
        let baseline = read_baseline_file(baseline_path)?;
        apply_baseline(&mut artifact, &baseline);
    }
    if let Some(waivers_path) = args.waivers.as_deref() {
        let waivers = read_waiver_file(waivers_path)?;
        let current_date = current_local_date()?;
        apply_waivers(&mut artifact, &waivers, &current_date)?;
    }
    let report_path = resolve_report_output_path(&root_dir, args.output.as_deref())?;
    write_diagnostics_artifact(&report_path, &artifact)?;

    let report = emit_lint_output(
        &artifact,
        args.mode,
        args.format,
        args.detail,
        args.diagnostics_page,
        args.diagnostics_page_size,
    );
    let display_path = display_report_path(&report_path)?;
    let drilldown_id = report
        .items
        .first()
        .map(|item| item.diagnostic_id.as_str())
        .or_else(|| {
            artifact
                .diagnostics
                .first()
                .map(|item| item.diagnostic_id.as_str())
        });
    emit_report_hint(args.format, &display_path, drilldown_id);

    let has_uncovered_change = artifact.diagnostics.iter().any(|diagnostic| {
        diagnostic.problem_type == "uncovered-change" && diagnostic.finding_state == "active"
    });
    let has_active_diagnostics = artifact
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.finding_state == "active");
    let has_critical_stale_doc = artifact.freshness_summary.critical_count > 0;

    if (args.mode == crate::cli::LintMode::Enforce && has_active_diagnostics)
        || (args.fail_on_uncovered_change && has_uncovered_change)
        || (args.fail_on_stale_docs && has_critical_stale_doc)
    {
        Ok(AppExit::LintFailure)
    } else {
        Ok(AppExit::Success)
    }
}

pub fn execute(args: &LintArgs) -> Result<CheckRun> {
    let root_dir = root_dir_from_option(args.root.as_deref())?;
    let changed_paths = get_changed_paths(&root_dir, args)?;
    if changed_paths.is_empty() {
        return Ok(CheckRun {
            problems: Vec::new(),
            changed_paths,
            matched_rules: Vec::new(),
        });
    }

    let loaded_rules = crate::config::load_impact_files(&root_dir, args.config.as_deref())?;
    let loaded_coverage_configs = load_coverage_configs(&root_dir, args.config.as_deref())?;
    let matched_rules = match_rules(&changed_paths, &loaded_rules);
    let uncovered_changed_paths =
        collect_uncovered_changed_paths(&changed_paths, &matched_rules, &loaded_coverage_configs);
    let mut problems =
        build_required_doc_problems(&root_dir, args, &changed_paths, &matched_rules)?;
    let governed_required_docs = collect_governed_required_doc_paths(&matched_rules);
    problems.extend(build_doc_problems(
        &root_dir,
        &changed_paths,
        &governed_required_docs,
    )?);
    problems.extend(
        uncovered_changed_paths
            .into_iter()
            .map(Problem::uncovered_change),
    );

    Ok(CheckRun {
        problems,
        changed_paths,
        matched_rules,
    })
}

pub fn build_required_doc_problems(
    root_dir: &Path,
    args: &LintArgs,
    changed_paths: &[String],
    matched_rules: &[MatchedRule],
) -> Result<Vec<Problem>> {
    let changed = changed_paths.iter().cloned().collect::<HashSet<_>>();
    let mut problems = Vec::new();

    for seed in collect_required_problem_seeds(matched_rules).values() {
        let exists = root_dir.join(&seed.path).exists();
        let touched = changed.contains(&seed.path);
        let problem = match seed.required_mode {
            RequiredDocMode::MustExist if !exists => Some(Problem::missing_review(
                seed.path.clone(),
                seed.rule_id.clone(),
                seed.rule_source.clone(),
                seed.required_mode.as_str().into(),
                "required_doc_missing".into(),
                "create_required_doc".into(),
                seed.trigger_paths.iter().cloned().collect(),
                seed.rule_reason.clone(),
                format!(
                    "Required doc does not exist for mode `must_exist`. Triggered by {} via rule `{}`.",
                    join_sorted(&seed.trigger_paths),
                    seed.rule_id
                ),
            )),
            RequiredDocMode::MustExist => None,
            _ if !touched => Some(Problem::missing_review(
                seed.path.clone(),
                seed.rule_id.clone(),
                seed.rule_source.clone(),
                seed.required_mode.as_str().into(),
                "required_doc_not_touched".into(),
                "touch_required_doc".into(),
                seed.trigger_paths.iter().cloned().collect(),
                seed.rule_reason.clone(),
                format!(
                    "Required doc was not touched for mode `{}`. Triggered by {} via rule `{}`.",
                    seed.required_mode,
                    join_sorted(&seed.trigger_paths),
                    seed.rule_id
                ),
            )),
            _ if !exists => Some(Problem::missing_review(
                seed.path.clone(),
                seed.rule_id.clone(),
                seed.rule_source.clone(),
                seed.required_mode.as_str().into(),
                "required_doc_missing_after_change".into(),
                "restore_required_doc".into(),
                seed.trigger_paths.iter().cloned().collect(),
                seed.rule_reason.clone(),
                format!(
                    "Required doc was touched but does not exist after the change for mode `{}`. Triggered by {} via rule `{}`.",
                    seed.required_mode,
                    join_sorted(&seed.trigger_paths),
                    seed.rule_id
                ),
            )),
            _ if seed.required_mode == RequiredDocMode::MetadataRefreshRequired
                && !metadata_refresh_satisfied(root_dir, args, &seed.path)? =>
            {
                Some(Problem::missing_review(
                    seed.path.clone(),
                    seed.rule_id.clone(),
                    seed.rule_source.clone(),
                    seed.required_mode.as_str().into(),
                    "review_metadata_not_refreshed".into(),
                    "refresh_review_metadata".into(),
                    seed.trigger_paths.iter().cloned().collect(),
                    seed.rule_reason.clone(),
                    format!(
                        "review metadata was not refreshed with a substantive review marker change. Triggered by {} via rule `{}`.",
                        join_sorted(&seed.trigger_paths),
                        seed.rule_id
                    ),
                ))
            }
            _ if seed.required_mode == RequiredDocMode::BodyUpdateRequired
                && !body_update_satisfied(root_dir, args, &seed.path)? =>
            {
                Some(Problem::missing_review(
                    seed.path.clone(),
                    seed.rule_id.clone(),
                    seed.rule_source.clone(),
                    seed.required_mode.as_str().into(),
                    "doc_body_not_updated".into(),
                    "update_doc_body".into(),
                    seed.trigger_paths.iter().cloned().collect(),
                    seed.rule_reason.clone(),
                    format!(
                        "Doc body was not updated beyond review metadata changes for mode `body_update_required`. Triggered by {} via rule `{}`.",
                        join_sorted(&seed.trigger_paths),
                        seed.rule_id
                    ),
                ))
            }
            _ => None,
        };

        if let Some(problem) = problem {
            problems.push(problem);
        }
    }

    Ok(problems)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RequiredProblemKey {
    path: String,
    rule_id: String,
    required_mode: RequiredDocMode,
    rule_source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequiredProblemSeed {
    path: String,
    rule_id: String,
    required_mode: RequiredDocMode,
    rule_source: String,
    rule_reason: String,
    trigger_paths: BTreeSet<String>,
}

fn collect_required_problem_seeds(
    matched_rules: &[MatchedRule],
) -> BTreeMap<RequiredProblemKey, RequiredProblemSeed> {
    let mut seeds = BTreeMap::new();

    for matched in matched_rules {
        for doc in &matched.rule.required_docs {
            let required_mode = RequiredDocMode::from_option(doc.mode.as_deref());
            let path = resolve_rule_path(&matched.base_dir, &doc.path);
            let key = RequiredProblemKey {
                path: path.clone(),
                rule_id: matched.rule.id.clone(),
                required_mode,
                rule_source: matched.source.clone(),
            };

            let entry = seeds.entry(key).or_insert_with(|| RequiredProblemSeed {
                path,
                rule_id: matched.rule.id.clone(),
                required_mode,
                rule_source: matched.source.clone(),
                rule_reason: matched.rule.reason.clone(),
                trigger_paths: BTreeSet::new(),
            });

            entry.trigger_paths.insert(matched.changed_path.clone());
        }
    }

    seeds
}

fn collect_governed_required_doc_paths(matched_rules: &[MatchedRule]) -> BTreeSet<String> {
    let mut governed = BTreeSet::new();

    for matched in matched_rules {
        for doc in &matched.rule.required_docs {
            governed.insert(resolve_rule_path(&matched.base_dir, &doc.path));
        }
    }

    governed
}

fn collect_uncovered_changed_paths(
    changed_paths: &[String],
    matched_rules: &[MatchedRule],
    loaded_coverage_configs: &[LoadedCoverageConfig],
) -> Vec<String> {
    let matched = matched_rules
        .iter()
        .map(|entry| entry.changed_path.as_str())
        .collect::<HashSet<_>>();

    changed_paths
        .iter()
        .filter(|path| path_in_coverage_scope(path, loaded_coverage_configs))
        .filter(|path| !matched.contains(path.as_str()))
        .cloned()
        .collect()
}

fn path_in_coverage_scope(path: &str, loaded_coverage_configs: &[LoadedCoverageConfig]) -> bool {
    if matches_any_coverage_pattern(path, loaded_coverage_configs, CoverageSelector::Exclude) {
        return false;
    }

    let has_include = loaded_coverage_configs
        .iter()
        .any(|loaded| !loaded.coverage.include.is_empty());
    if !has_include {
        return true;
    }

    matches_any_coverage_pattern(path, loaded_coverage_configs, CoverageSelector::Include)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoverageSelector {
    Include,
    Exclude,
}

fn matches_any_coverage_pattern(
    path: &str,
    loaded_coverage_configs: &[LoadedCoverageConfig],
    selector: CoverageSelector,
) -> bool {
    loaded_coverage_configs.iter().any(|loaded| {
        let patterns = match selector {
            CoverageSelector::Include => &loaded.coverage.include,
            CoverageSelector::Exclude => &loaded.coverage.exclude,
        };

        patterns
            .iter()
            .any(|pattern| matches_pattern(path, &resolve_rule_path(&loaded.base_dir, pattern)))
    })
}

fn metadata_refresh_satisfied(root_dir: &Path, args: &LintArgs, rel_path: &str) -> Result<bool> {
    let comparison = get_file_comparison(root_dir, args, rel_path)?;

    if is_markdown_path(rel_path) {
        let current = match comparison.current.as_deref() {
            Some(current) => current,
            None => return Ok(false),
        };
        if !missing_markdown_review_metadata(current).is_empty() {
            return Ok(false);
        }

        let current_values = review_metadata_values_from_markdown(current);
        return Ok(match comparison.previous.as_deref() {
            Some(previous) => review_metadata_values_from_markdown(previous) != current_values,
            None => true,
        });
    }

    if is_yaml_path(rel_path) {
        let current = load_yaml_value(&root_dir.join(rel_path), rel_path)?;
        if !missing_yaml_review_metadata_from_value(&current).is_empty() {
            return Ok(false);
        }

        let current_values = review_metadata_values_from_yaml(&current);
        return Ok(match comparison.previous.as_deref() {
            Some(previous) => match parse_yaml_value(previous, rel_path) {
                Ok(previous) => review_metadata_values_from_yaml(&previous) != current_values,
                Err(_) => true,
            },
            None => true,
        });
    }

    Ok(true)
}

fn body_update_satisfied(root_dir: &Path, args: &LintArgs, rel_path: &str) -> Result<bool> {
    let comparison = get_file_comparison(root_dir, args, rel_path)?;

    if is_markdown_path(rel_path) {
        let current = match comparison.current.as_deref() {
            Some(current) => current,
            None => return Ok(false),
        };

        return Ok(match comparison.previous.as_deref() {
            Some(previous) => markdown_body(previous) != markdown_body(current),
            None => true,
        });
    }

    if is_yaml_path(rel_path) {
        let current = load_yaml_value(&root_dir.join(rel_path), rel_path)?;
        let current = strip_review_metadata_from_yaml(current);
        return Ok(match comparison.previous.as_deref() {
            Some(previous) => match parse_yaml_value(previous, rel_path) {
                Ok(previous) => strip_review_metadata_from_yaml(previous) != current,
                Err(_) => true,
            },
            None => true,
        });
    }

    Ok(file_contents_changed(&comparison))
}

fn file_contents_changed(comparison: &FileComparison) -> bool {
    comparison.previous != comparison.current
}

fn review_metadata_values_from_markdown(text: &str) -> BTreeMap<String, String> {
    let values = parse_frontmatter_scalar_values(text);
    values
        .into_iter()
        .filter(|(key, _)| matches!(key.as_str(), "lastReviewedAt" | "lastReviewedCommit"))
        .collect()
}

fn review_metadata_values_from_yaml(value: &Value) -> BTreeMap<String, Value> {
    let mapping = match value {
        Value::Mapping(mapping) => mapping,
        _ => return BTreeMap::new(),
    };

    let mut values = BTreeMap::new();
    for key in ["lastReviewedAt", "lastReviewedCommit"] {
        if let Some(value) = mapping.get(Value::String(key.to_string())) {
            values.insert(key.to_string(), value.clone());
        }
    }
    values
}

fn strip_review_metadata_from_yaml(value: Value) -> Value {
    let Value::Mapping(mut mapping) = value else {
        return value;
    };

    for key in ["lastReviewedAt", "lastReviewedCommit"] {
        mapping.remove(Value::String(key.to_string()));
    }

    Value::Mapping(mapping)
}

fn is_markdown_path(path: &str) -> bool {
    path.ends_with(".md")
}

fn is_yaml_path(path: &str) -> bool {
    path.ends_with(".yaml") || path.ends_with(".yml")
}

fn join_sorted(values: &BTreeSet<String>) -> String {
    values.iter().cloned().collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::cli::{LintArgs, LintMode, OutputFormat};
    use crate::config::{RequiredDoc, Rule, Trigger};
    use crate::diagnostics::read_diagnostics_artifact;
    use crate::rules::{MatchedRule, RequiredDocMode};

    use super::{build_required_doc_problems, execute, run};

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be valid")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{nanos}-{}", std::process::id()));
        fs::create_dir_all(&path).expect("temp dir should be created");
        path
    }

    fn base_args(root: PathBuf) -> LintArgs {
        LintArgs {
            root: Some(root),
            config: None,
            base: None,
            head: None,
            files: None,
            staged: false,
            worktree: false,
            merge_base: None,
            mode: LintMode::Warn,
            format: OutputFormat::Text,
            detail: crate::cli::DiagnosticDetail::Compact,
            diagnostics_page: 1,
            diagnostics_page_size: 5,
            fail_on_uncovered_change: false,
            fail_on_stale_docs: false,
            baseline: None,
            waivers: None,
            output: None,
        }
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

    fn git_stdout(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git command failed: git {}",
            args.join(" ")
        );
        String::from_utf8(output.stdout)
            .expect("git stdout should be utf-8")
            .trim()
            .to_string()
    }

    fn init_git_repo(root: &Path) {
        git(root, &["init"]);
        git(root, &["config", "user.name", "Codex"]);
        git(root, &["config", "user.email", "codex@example.com"]);
    }

    #[test]
    fn execute_reports_missing_review_and_metadata() {
        let root = temp_dir("docpact-check");
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src")).expect("src dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
lastReviewedAt: "2026-04-18"
lastReviewedCommit: "abc"
coverage:
  include:
    - src/**
repo:
  id: example
rules:
  - id: repo-rule
    scope: repo
    repo: example
    triggers:
      - path: src/**
        kind: code
    requiredDocs:
      - path: .docpact/config.yaml
        mode: review_or_update
      - path: .docpact/quality-rubric.md
        mode: review_or_update
    reason: repo
"#,
        )
        .expect("impact config");

        fs::write(root.join("src/index.ts"), "export const x = 1;\n").expect("source file");
        fs::write(
            root.join(".docpact/quality-rubric.md"),
            "# Missing frontmatter\n",
        )
        .expect("doc file");

        let mut args = base_args(root);
        args.files = Some("src/index.ts,.docpact/quality-rubric.md".into());

        let run = execute(&args).expect("lint should execute");

        assert_eq!(run.problems.len(), 2);
        assert_eq!(run.problems[0].problem_type, "missing-review");
        assert_eq!(run.problems[1].problem_type, "missing-metadata");
    }

    #[test]
    fn execute_reports_uncovered_change_with_include_and_exclude() {
        let root = temp_dir("docpact-check-coverage");
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src/api")).expect("src api dir");
        fs::create_dir_all(root.join("src/payments")).expect("src payments dir");
        fs::create_dir_all(root.join("src/generated")).expect("src generated dir");
        fs::create_dir_all(root.join("docs")).expect("docs dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
lastReviewedAt: "2026-04-18"
lastReviewedCommit: "abc"
coverage:
  include:
    - src/**
  exclude:
    - src/generated/**
repo:
  id: example
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
    reason: repo
"#,
        )
        .expect("impact config");

        fs::write(root.join("src/api/client.ts"), "export const api = 1;\n").expect("api file");
        fs::write(
            root.join("src/payments/charge.ts"),
            "export const charge = 1;\n",
        )
        .expect("payments file");
        fs::write(
            root.join("src/generated/schema.ts"),
            "export const generated = 1;\n",
        )
        .expect("generated file");
        fs::write(root.join("docs/api.md"), "# API\n").expect("doc");

        let mut args = base_args(root);
        args.files =
            Some("src/api/client.ts,src/payments/charge.ts,src/generated/schema.ts".into());

        let run = execute(&args).expect("lint should execute");
        let uncovered = run
            .problems
            .iter()
            .filter(|problem| problem.problem_type == "uncovered-change")
            .map(|problem| problem.path.clone())
            .collect::<Vec<_>>();

        assert_eq!(uncovered, vec!["src/payments/charge.ts".to_string()]);
    }

    #[test]
    fn execute_uses_workspace_relative_coverage_patterns() {
        let root = temp_dir("docpact-check-coverage-workspace");
        fs::create_dir_all(root.join(".docpact")).expect("root doc dir");
        fs::create_dir_all(root.join("subrepo/.docpact")).expect("subrepo doc dir");
        fs::create_dir_all(root.join("subrepo/src/api")).expect("subrepo api dir");
        fs::create_dir_all(root.join("subrepo/src/payments")).expect("subrepo payments dir");
        fs::create_dir_all(root.join("subrepo/docs")).expect("subrepo docs dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: workspace
lastReviewedAt: "2026-04-18"
lastReviewedCommit: "abc"
workspace:
  name: demo
rules: []
"#,
        )
        .expect("root config");

        fs::write(
            root.join("subrepo/.docpact/config.yaml"),
            r#"
version: 1
layout: repo
lastReviewedAt: "2026-04-18"
lastReviewedCommit: "abc"
coverage:
  include:
    - src/**
repo:
  id: subrepo
rules:
  - id: repo-rule
    scope: repo
    repo: subrepo
    triggers:
      - path: src/api/**
        kind: code
    requiredDocs:
      - path: docs/api.md
        mode: review_or_update
    reason: repo
"#,
        )
        .expect("subrepo config");

        fs::write(
            root.join("subrepo/src/api/client.ts"),
            "export const api = 1;\n",
        )
        .expect("api file");
        fs::write(
            root.join("subrepo/src/payments/charge.ts"),
            "export const charge = 1;\n",
        )
        .expect("payments file");
        fs::write(root.join("subrepo/docs/api.md"), "# API\n").expect("doc");

        let mut args = base_args(root);
        args.files = Some("subrepo/src/api/client.ts,subrepo/src/payments/charge.ts".into());

        let run = execute(&args).expect("lint should execute");
        let uncovered = run
            .problems
            .iter()
            .filter(|problem| problem.problem_type == "uncovered-change")
            .map(|problem| problem.path.clone())
            .collect::<Vec<_>>();

        assert_eq!(
            uncovered,
            vec!["subrepo/src/payments/charge.ts".to_string()]
        );
    }

    #[test]
    fn fail_on_uncovered_change_returns_lint_failure_in_warn_mode() {
        let root = temp_dir("docpact-check-fail-on-uncovered");
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src/payments")).expect("src dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
lastReviewedAt: "2026-04-18"
lastReviewedCommit: "abc"
coverage:
  include:
    - src/**
repo:
  id: example
rules: []
"#,
        )
        .expect("impact config");

        fs::write(
            root.join("src/payments/charge.ts"),
            "export const charge = 1;\n",
        )
        .expect("payments file");

        let mut args = base_args(root);
        args.files = Some("src/payments/charge.ts".into());
        args.fail_on_uncovered_change = true;

        let exit = run(args).expect("lint should execute");
        assert_eq!(exit, crate::AppExit::LintFailure);
    }

    #[test]
    fn fail_on_stale_docs_returns_lint_failure_and_writes_freshness_summary() {
        let root = temp_dir("docpact-check-fail-on-stale");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src/api")).expect("src dir");
        fs::create_dir_all(root.join("docs")).expect("docs dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
freshness:
  warn_after_commits: 99
  warn_after_days: 2
  critical_after_days: 3
repo:
  id: example
rules:
  - id: api-rule
    scope: repo
    repo: example
    triggers:
      - path: src/api/**
        kind: code
    requiredDocs:
      - path: docs/api.md
        mode: must_exist
    reason: api
"#,
        )
        .expect("config");
        fs::write(root.join("src/api/client.ts"), "export const api = 1;\n").expect("src");
        fs::write(
            root.join("docs/api.md"),
            "---\nlastReviewedAt: 2026-04-01\nlastReviewedCommit: pending\n---\n# API\n",
        )
        .expect("doc");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);
        let base_commit = git_stdout(&root, &["rev-parse", "HEAD"]);
        fs::write(
            root.join("docs/api.md"),
            format!(
                "---\nlastReviewedAt: 2026-04-01\nlastReviewedCommit: {base_commit}\n---\n# API\n"
            ),
        )
        .expect("doc baseline");
        git(&root, &["add", "docs/api.md"]);
        git(&root, &["commit", "-m", "record baseline"]);

        let mut args = base_args(root.clone());
        args.files = Some("src/api/client.ts".into());
        args.fail_on_stale_docs = true;
        args.output = Some(root.join(".docpact/runs/test-lint.json"));

        let exit = run(args).expect("lint should execute");
        assert_eq!(exit, crate::AppExit::LintFailure);

        let artifact = read_diagnostics_artifact(&root.join(".docpact/runs/test-lint.json"))
            .expect("artifact should be readable");
        assert_eq!(artifact.freshness_status, "has-critical-stale-doc");
        assert_eq!(artifact.freshness_summary.critical_count, 1);
        assert_eq!(artifact.stale_docs.len(), 1);
        assert_eq!(artifact.stale_docs[0].path, "docs/api.md");
    }

    #[test]
    fn must_exist_mode_allows_untouched_existing_doc() {
        let root = temp_dir("docpact-check-must-exist");
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::write(root.join("README.md"), "# Present\n").expect("readme");

        let matched = vec![MatchedRule {
            changed_path: "src/index.ts".into(),
            source: ".docpact/config.yaml".into(),
            config_source: ".docpact/config.yaml".into(),
            base_dir: String::new(),
            rule: Rule {
                id: "repo-rule".into(),
                scope: "repo".into(),
                repo: "example".into(),
                triggers: vec![Trigger {
                    path: "src/**".into(),
                    kind: Some("code".into()),
                }],
                required_docs: vec![RequiredDoc {
                    path: "README.md".into(),
                    mode: Some(RequiredDocMode::MustExist.as_str().into()),
                }],
                reason: "repo".into(),
            },
        }];

        let problems = build_required_doc_problems(
            &root,
            &base_args(root.clone()),
            &["src/index.ts".into()],
            &matched,
        )
        .expect("mode evaluation should succeed");
        assert!(problems.is_empty());
    }

    #[test]
    fn metadata_refresh_required_fails_when_review_metadata_does_not_change() {
        let root = temp_dir("docpact-check-metadata-mode");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("docs")).expect("docs dir");
        fs::create_dir_all(root.join("src")).expect("src dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
lastReviewedAt: "2026-04-21"
lastReviewedCommit: "base"
coverage:
  include:
    - src/**
repo:
  id: example
rules:
  - id: repo-rule
    scope: repo
    repo: example
    triggers:
      - path: src/**
        kind: code
    requiredDocs:
      - path: docs/api.md
        mode: metadata_refresh_required
    reason: repo
"#,
        )
        .expect("config");

        fs::write(root.join("src/index.ts"), "export const x = 1;\n").expect("src");
        fs::write(
            root.join("docs/api.md"),
            r#"---
docType: contract
scope: repo
status: draft
authoritative: true
owner: example
language: en
whenToUse:
  - api updates
whenToUpdate:
  - api changes
checkPaths:
  - src/**
lastReviewedAt: 2026-04-20
lastReviewedCommit: base
---

# API

Old body
"#,
        )
        .expect("doc");

        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);

        fs::write(root.join("src/index.ts"), "export const x = 2;\n").expect("src update");
        fs::write(
            root.join("docs/api.md"),
            r#"---
docType: contract
scope: repo
status: draft
authoritative: true
owner: example
language: en
whenToUse:
  - api updates
whenToUpdate:
  - api changes
checkPaths:
  - src/**
lastReviewedAt: 2026-04-20
lastReviewedCommit: base
---

# API

New body without metadata refresh
"#,
        )
        .expect("doc update");

        let mut args = base_args(root);
        args.worktree = true;

        let run = execute(&args).expect("lint should execute");
        assert_eq!(run.problems.len(), 1);
        assert!(
            run.problems[0]
                .message
                .contains("review metadata was not refreshed")
        );
    }

    #[test]
    fn body_update_required_fails_for_metadata_only_change() {
        let root = temp_dir("docpact-check-body-mode");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("docs")).expect("docs dir");
        fs::create_dir_all(root.join("src")).expect("src dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
lastReviewedAt: "2026-04-21"
lastReviewedCommit: "base"
coverage:
  include:
    - src/**
repo:
  id: example
rules:
  - id: repo-rule
    scope: repo
    repo: example
    triggers:
      - path: src/**
        kind: code
    requiredDocs:
      - path: docs/api.md
        mode: body_update_required
    reason: repo
"#,
        )
        .expect("config");

        fs::write(root.join("src/index.ts"), "export const x = 1;\n").expect("src");
        fs::write(
            root.join("docs/api.md"),
            r#"---
docType: contract
scope: repo
status: draft
authoritative: true
owner: example
language: en
whenToUse:
  - api updates
whenToUpdate:
  - api changes
checkPaths:
  - src/**
lastReviewedAt: 2026-04-20
lastReviewedCommit: base
---

# API

Stable body
"#,
        )
        .expect("doc");

        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);

        fs::write(root.join("src/index.ts"), "export const x = 2;\n").expect("src update");
        fs::write(
            root.join("docs/api.md"),
            r#"---
docType: contract
scope: repo
status: draft
authoritative: true
owner: example
language: en
whenToUse:
  - api updates
whenToUpdate:
  - api changes
checkPaths:
  - src/**
lastReviewedAt: 2026-04-21
lastReviewedCommit: head
---

# API

Stable body
"#,
        )
        .expect("doc update");

        let mut args = base_args(root);
        args.worktree = true;

        let run = execute(&args).expect("lint should execute");
        assert_eq!(run.problems.len(), 1);
        assert!(run.problems[0].message.contains("body was not updated"));
    }

    #[test]
    fn body_update_required_passes_when_body_changes() {
        let root = temp_dir("docpact-check-body-pass");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("docs")).expect("docs dir");
        fs::create_dir_all(root.join("src")).expect("src dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
lastReviewedAt: "2026-04-21"
lastReviewedCommit: "base"
coverage:
  include:
    - src/**
repo:
  id: example
rules:
  - id: repo-rule
    scope: repo
    repo: example
    triggers:
      - path: src/**
        kind: code
    requiredDocs:
      - path: docs/api.md
        mode: body_update_required
    reason: repo
"#,
        )
        .expect("config");

        fs::write(root.join("src/index.ts"), "export const x = 1;\n").expect("src");
        fs::write(
            root.join("docs/api.md"),
            r#"---
docType: contract
scope: repo
status: draft
authoritative: true
owner: example
language: en
whenToUse:
  - api updates
whenToUpdate:
  - api changes
checkPaths:
  - src/**
lastReviewedAt: 2026-04-20
lastReviewedCommit: base
---

# API

Stable body
"#,
        )
        .expect("doc");

        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);

        fs::write(root.join("src/index.ts"), "export const x = 2;\n").expect("src update");
        fs::write(
            root.join("docs/api.md"),
            r#"---
docType: contract
scope: repo
status: draft
authoritative: true
owner: example
language: en
whenToUse:
  - api updates
whenToUpdate:
  - api changes
checkPaths:
  - src/**
lastReviewedAt: 2026-04-21
lastReviewedCommit: head
---

# API

Updated body
"#,
        )
        .expect("doc update");

        let mut args = base_args(root);
        args.worktree = true;

        let run = execute(&args).expect("lint should execute");
        assert!(run.problems.is_empty());
    }
}
