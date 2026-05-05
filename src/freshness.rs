//! Implementation of document freshness checks.
//!
//! Freshness evaluates review metadata and git history to identify governed
//! documents that may no longer be trustworthy.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use miette::{IntoDiagnostic, Result, bail, miette};
use serde::{Deserialize, Serialize};
use yaml_serde::Value;

use crate::AppExit;
use crate::cli::{FreshnessArgs, FreshnessOutputFormat};
use crate::config::{
    FreshnessConfig, ImpactFileDescriptor, LoadedRule, list_impact_files, load_freshness_configs,
    load_impact_files, load_yaml_value, resolve_rule_path, root_dir_from_option,
};
use crate::git::{get_tracked_paths, get_unique_commits_since, is_commit_reachable_from_head};
use crate::metadata::parse_frontmatter_scalar_values;
use crate::reporters::OutputWarning;
use crate::rules::MatchedRule;
use crate::rules::matches_pattern;

pub const FRESHNESS_AUDIT_SCHEMA_VERSION: &str = "docpact.freshness.v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FreshnessReport {
    pub schema_version: String,
    pub tool_name: String,
    pub tool_version: String,
    pub command: String,
    pub warnings: Vec<OutputWarning>,
    pub generated_at: String,
    pub summary: FreshnessSummary,
    pub items: Vec<FreshnessItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct FreshnessSummary {
    pub governed_doc_count: usize,
    pub fresh_doc_count: usize,
    pub stale_doc_count: usize,
    pub warn_count: usize,
    pub critical_count: usize,
    pub invalid_review_reference_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FreshnessItem {
    pub path: String,
    pub last_reviewed_commit: Option<String>,
    pub last_reviewed_at: Option<String>,
    pub commits_since_review: Option<usize>,
    pub days_since_review: Option<i64>,
    pub associated_changed_paths: Vec<String>,
    pub associated_changed_paths_count: usize,
    pub staleness_level: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub review_reference_problems: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct LintFreshnessReport {
    pub freshness_status: String,
    pub summary: FreshnessSummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale_docs: Vec<FreshnessItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteFreshnessTarget {
    pub path: String,
    pub config_sources: Vec<String>,
    pub associated_patterns: Vec<String>,
}

#[derive(Debug, Clone)]
struct GovernedDocContext {
    thresholds: FreshnessConfig,
    associated_patterns: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReviewMetadata {
    last_reviewed_commit: Option<String>,
    last_reviewed_at: Option<String>,
    review_reference_problems: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FreshnessLevel {
    Ok,
    Warn,
    Critical,
}

impl FreshnessLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Critical => "critical",
        }
    }
}

pub fn run(args: FreshnessArgs) -> Result<AppExit> {
    let report = execute(&args)?;
    emit_report(&report, args.format);
    Ok(AppExit::Success)
}

pub fn execute(args: &FreshnessArgs) -> Result<FreshnessReport> {
    let today = today_date_string()?;
    execute_with_today(args, &today)
}

fn execute_with_today(args: &FreshnessArgs, today: &str) -> Result<FreshnessReport> {
    let root_dir = root_dir_from_option(args.root.as_deref())?;
    let impact_files = list_impact_files(&root_dir, args.config.as_deref())?;
    let tracked_paths = collect_tracked_paths(&root_dir, &impact_files)?;
    let loaded_rules = load_impact_files(&root_dir, args.config.as_deref())?;
    let freshness_configs = load_freshness_configs(&root_dir, args.config.as_deref())?;
    let freshness_by_source = freshness_configs
        .into_iter()
        .map(|loaded| (loaded.source, loaded.freshness))
        .collect::<BTreeMap<_, _>>();
    let governed_docs = build_governed_doc_contexts(&loaded_rules, &freshness_by_source);

    let items = evaluate_contexts(&root_dir, &governed_docs, &tracked_paths, today)?;
    let summary = summarize_items(&items);

    Ok(FreshnessReport {
        schema_version: FRESHNESS_AUDIT_SCHEMA_VERSION.into(),
        tool_name: env!("CARGO_PKG_NAME").into(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        command: "freshness".into(),
        warnings: Vec::new(),
        generated_at: today.to_string(),
        summary,
        items,
    })
}

pub fn execute_lint_for_matched_rules(
    root_dir: &Path,
    config_override: Option<&Path>,
    matched_rules: &[MatchedRule],
) -> Result<LintFreshnessReport> {
    let today = today_date_string()?;
    execute_lint_for_matched_rules_with_today(root_dir, config_override, matched_rules, &today)
}

fn execute_lint_for_matched_rules_with_today(
    root_dir: &Path,
    config_override: Option<&Path>,
    matched_rules: &[MatchedRule],
    today: &str,
) -> Result<LintFreshnessReport> {
    if matched_rules.is_empty() {
        return Ok(LintFreshnessReport {
            freshness_status: "ok".into(),
            summary: FreshnessSummary::default(),
            stale_docs: Vec::new(),
        });
    }

    let impact_files = list_impact_files(root_dir, config_override)?;
    let tracked_paths = collect_tracked_paths(root_dir, &impact_files)?;
    let freshness_configs = load_freshness_configs(root_dir, config_override)?;
    let freshness_by_source = freshness_configs
        .into_iter()
        .map(|loaded| (loaded.source, loaded.freshness))
        .collect::<BTreeMap<_, _>>();
    let governed_docs =
        build_governed_doc_contexts_from_matched_rules(matched_rules, &freshness_by_source);
    let items = evaluate_contexts(root_dir, &governed_docs, &tracked_paths, today)?;
    let summary = summarize_items(&items);
    let stale_docs = items
        .into_iter()
        .filter(|item| item.staleness_level != FreshnessLevel::Ok.as_str())
        .collect::<Vec<_>>();

    Ok(LintFreshnessReport {
        freshness_status: summarize_status(&summary).into(),
        summary,
        stale_docs,
    })
}

pub fn execute_route_freshness(
    root_dir: &Path,
    config_override: Option<&Path>,
    targets: &[RouteFreshnessTarget],
) -> Result<BTreeMap<String, FreshnessItem>> {
    let today = today_date_string()?;
    execute_route_freshness_with_today(root_dir, config_override, targets, &today)
}

pub(crate) fn execute_route_freshness_with_today(
    root_dir: &Path,
    config_override: Option<&Path>,
    targets: &[RouteFreshnessTarget],
    today: &str,
) -> Result<BTreeMap<String, FreshnessItem>> {
    if targets.is_empty() {
        return Ok(BTreeMap::new());
    }

    let impact_files = list_impact_files(root_dir, config_override)?;
    let tracked_paths = collect_tracked_paths(root_dir, &impact_files)?;
    let freshness_configs = load_freshness_configs(root_dir, config_override)?;
    let freshness_by_source = freshness_configs
        .into_iter()
        .map(|loaded| (loaded.source, loaded.freshness))
        .collect::<BTreeMap<_, _>>();

    let mut contexts = BTreeMap::<String, GovernedDocContext>::new();
    for target in targets {
        let mut thresholds = FreshnessConfig::default();
        for source in &target.config_sources {
            if let Some(config) = freshness_by_source.get(source) {
                thresholds = merge_thresholds(&thresholds, config);
            }
        }

        contexts.insert(
            target.path.clone(),
            GovernedDocContext {
                thresholds,
                associated_patterns: target.associated_patterns.iter().cloned().collect(),
            },
        );
    }

    let items = evaluate_contexts(root_dir, &contexts, &tracked_paths, today)?;
    Ok(items
        .into_iter()
        .map(|item| (item.path.clone(), item))
        .collect::<BTreeMap<_, _>>())
}

fn build_governed_doc_contexts(
    loaded_rules: &[LoadedRule],
    freshness_by_source: &BTreeMap<String, FreshnessConfig>,
) -> BTreeMap<String, GovernedDocContext> {
    let mut contexts = BTreeMap::new();

    for loaded in loaded_rules {
        let thresholds = freshness_by_source
            .get(&loaded.config_source)
            .cloned()
            .unwrap_or_default();
        let associated_patterns = loaded
            .rule
            .triggers
            .iter()
            .map(|trigger| resolve_rule_path(&loaded.base_dir, &trigger.path))
            .collect::<Vec<_>>();

        for required_doc in &loaded.rule.required_docs {
            let resolved_doc = resolve_rule_path(&loaded.base_dir, &required_doc.path);
            let entry = contexts
                .entry(resolved_doc)
                .or_insert_with(|| GovernedDocContext {
                    thresholds: thresholds.clone(),
                    associated_patterns: BTreeSet::new(),
                });
            entry.thresholds = merge_thresholds(&entry.thresholds, &thresholds);
            entry
                .associated_patterns
                .extend(associated_patterns.iter().cloned());
        }
    }

    contexts
}

fn build_governed_doc_contexts_from_matched_rules(
    matched_rules: &[MatchedRule],
    freshness_by_source: &BTreeMap<String, FreshnessConfig>,
) -> BTreeMap<String, GovernedDocContext> {
    let mut contexts = BTreeMap::new();

    for matched in matched_rules {
        let thresholds = freshness_by_source
            .get(&matched.config_source)
            .cloned()
            .unwrap_or_default();
        let associated_patterns = matched
            .rule
            .triggers
            .iter()
            .map(|trigger| resolve_rule_path(&matched.base_dir, &trigger.path))
            .collect::<Vec<_>>();

        for required_doc in &matched.rule.required_docs {
            let resolved_doc = resolve_rule_path(&matched.base_dir, &required_doc.path);
            let entry = contexts
                .entry(resolved_doc)
                .or_insert_with(|| GovernedDocContext {
                    thresholds: thresholds.clone(),
                    associated_patterns: BTreeSet::new(),
                });
            entry.thresholds = merge_thresholds(&entry.thresholds, &thresholds);
            entry
                .associated_patterns
                .extend(associated_patterns.iter().cloned());
        }
    }

    contexts
}

fn merge_thresholds(left: &FreshnessConfig, right: &FreshnessConfig) -> FreshnessConfig {
    FreshnessConfig {
        warn_after_commits: left.warn_after_commits.min(right.warn_after_commits),
        warn_after_days: left.warn_after_days.min(right.warn_after_days),
        critical_after_days: left.critical_after_days.min(right.critical_after_days),
    }
}

fn evaluate_doc(
    root_dir: &Path,
    rel_path: &str,
    context: &GovernedDocContext,
    tracked_paths: &[String],
    today: &str,
) -> Result<FreshnessItem> {
    let metadata = read_review_metadata(root_dir, rel_path)?;
    let associated_changed_paths = tracked_paths
        .iter()
        .filter(|path| {
            context
                .associated_patterns
                .iter()
                .any(|pattern| matches_pattern(path, pattern))
        })
        .cloned()
        .collect::<Vec<_>>();
    let associated_changed_paths_count = associated_changed_paths.len();
    let mut review_reference_problems = metadata.review_reference_problems.clone();

    let commits_since_review = match metadata.last_reviewed_commit.as_deref() {
        Some(commit) if is_commit_reachable_from_head(root_dir, commit)? => {
            Some(get_unique_commits_since(root_dir, commit, &associated_changed_paths)?.len())
        }
        Some(_) => {
            review_reference_problems.push("invalid-lastReviewedCommit".into());
            None
        }
        None => {
            review_reference_problems.push("missing-lastReviewedCommit".into());
            None
        }
    };

    let days_since_review = match metadata.last_reviewed_at.as_deref() {
        Some(reviewed_at) => match days_between(reviewed_at, today) {
            Ok(days) => Some(days),
            Err(error) => {
                let _ = error;
                review_reference_problems.push("invalid-lastReviewedAt".into());
                None
            }
        },
        None => {
            review_reference_problems.push("missing-lastReviewedAt".into());
            None
        }
    };

    let staleness_level =
        classify_staleness(&context.thresholds, commits_since_review, days_since_review);

    Ok(FreshnessItem {
        path: rel_path.to_string(),
        last_reviewed_commit: metadata.last_reviewed_commit,
        last_reviewed_at: metadata.last_reviewed_at,
        commits_since_review,
        days_since_review,
        associated_changed_paths,
        associated_changed_paths_count,
        staleness_level: staleness_level.as_str().into(),
        review_reference_problems,
    })
}

fn evaluate_contexts(
    root_dir: &Path,
    contexts: &BTreeMap<String, GovernedDocContext>,
    tracked_paths: &[String],
    today: &str,
) -> Result<Vec<FreshnessItem>> {
    let mut items = Vec::new();
    for (path, context) in contexts {
        items.push(evaluate_doc(root_dir, path, context, tracked_paths, today)?);
    }
    Ok(items)
}

fn summarize_items(items: &[FreshnessItem]) -> FreshnessSummary {
    let warn_count = items
        .iter()
        .filter(|item| item.staleness_level == FreshnessLevel::Warn.as_str())
        .count();
    let critical_count = items
        .iter()
        .filter(|item| item.staleness_level == FreshnessLevel::Critical.as_str())
        .count();
    let invalid_review_reference_count = items
        .iter()
        .filter(|item| !item.review_reference_problems.is_empty())
        .count();
    let stale_doc_count = warn_count + critical_count;
    let governed_doc_count = items.len();

    FreshnessSummary {
        governed_doc_count,
        fresh_doc_count: governed_doc_count.saturating_sub(stale_doc_count),
        stale_doc_count,
        warn_count,
        critical_count,
        invalid_review_reference_count,
    }
}

fn summarize_status(summary: &FreshnessSummary) -> &'static str {
    if summary.critical_count > 0 {
        "has-critical-stale-doc"
    } else if summary.stale_doc_count > 0 {
        "has-stale-doc"
    } else {
        "ok"
    }
}

fn classify_staleness(
    thresholds: &FreshnessConfig,
    commits_since_review: Option<usize>,
    days_since_review: Option<i64>,
) -> FreshnessLevel {
    let mut level = FreshnessLevel::Ok;

    if let Some(days) = days_since_review {
        if days >= thresholds.critical_after_days as i64 {
            level = FreshnessLevel::Critical;
        } else if days >= thresholds.warn_after_days as i64 {
            level = level.max(FreshnessLevel::Warn);
        }
    }

    if let Some(commits) = commits_since_review {
        if commits >= thresholds.warn_after_commits {
            level = level.max(FreshnessLevel::Warn);
        }
    }

    level
}

fn read_review_metadata(root_dir: &Path, rel_path: &str) -> Result<ReviewMetadata> {
    let abs_path = root_dir.join(rel_path);
    if !abs_path.exists() {
        return Ok(ReviewMetadata {
            last_reviewed_commit: None,
            last_reviewed_at: None,
            review_reference_problems: vec!["missing-document".into()],
        });
    }

    if rel_path.ends_with(".md") {
        let text = fs::read_to_string(abs_path).into_diagnostic()?;
        let values = parse_frontmatter_scalar_values(&text);
        return Ok(ReviewMetadata {
            last_reviewed_commit: values
                .get("lastReviewedCommit")
                .map(|value| normalize_scalar_value(value)),
            last_reviewed_at: values
                .get("lastReviewedAt")
                .map(|value| normalize_scalar_value(value)),
            review_reference_problems: Vec::new(),
        });
    }

    if rel_path.ends_with(".yaml") || rel_path.ends_with(".yml") {
        let yaml = load_yaml_value(&abs_path, rel_path)?;
        return Ok(read_review_metadata_from_yaml(&yaml));
    }

    Ok(ReviewMetadata {
        last_reviewed_commit: None,
        last_reviewed_at: None,
        review_reference_problems: vec!["unsupported-review-metadata-format".into()],
    })
}

fn read_review_metadata_from_yaml(value: &Value) -> ReviewMetadata {
    let Value::Mapping(mapping) = value else {
        return ReviewMetadata {
            last_reviewed_commit: None,
            last_reviewed_at: None,
            review_reference_problems: vec!["invalid-yaml-review-metadata".into()],
        };
    };

    ReviewMetadata {
        last_reviewed_commit: mapping
            .get(Value::String("lastReviewedCommit".into()))
            .and_then(value_to_string),
        last_reviewed_at: mapping
            .get(Value::String("lastReviewedAt".into()))
            .and_then(value_to_string),
        review_reference_problems: Vec::new(),
    }
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(string) => Some(normalize_scalar_value(string)),
        _ => None,
    }
}

fn normalize_scalar_value(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 {
        let bytes = trimmed.as_bytes();
        let first = bytes[0];
        let last = bytes[trimmed.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return trimmed[1..trimmed.len() - 1].to_string();
        }
    }
    trimmed.to_string()
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

fn days_between(older: &str, newer: &str) -> Result<i64> {
    let (older_year, older_month, older_day) = parse_iso_date(older)?;
    let (newer_year, newer_month, newer_day) = parse_iso_date(newer)?;
    let delta = civil_day_number(newer_year, newer_month, newer_day)
        - civil_day_number(older_year, older_month, older_day);
    Ok(delta.max(0))
}

fn parse_iso_date(value: &str) -> Result<(i32, u32, u32)> {
    if value.len() != 10 {
        bail!("invalid YYYY-MM-DD date: {value}");
    }

    let bytes = value.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        bail!("invalid YYYY-MM-DD date: {value}");
    }

    if !bytes
        .iter()
        .enumerate()
        .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
    {
        bail!("invalid YYYY-MM-DD date: {value}");
    }

    let year = value[0..4]
        .parse::<i32>()
        .map_err(|_| miette!("invalid YYYY-MM-DD date: {value}"))?;
    let month = value[5..7]
        .parse::<u32>()
        .map_err(|_| miette!("invalid YYYY-MM-DD date: {value}"))?;
    let day = value[8..10]
        .parse::<u32>()
        .map_err(|_| miette!("invalid YYYY-MM-DD date: {value}"))?;

    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        bail!("invalid YYYY-MM-DD date: {value}");
    }

    Ok((year, month, day))
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn civil_day_number(year: i32, month: u32, day: u32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i32;
    let day = day as i32;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146097 + doe - 719468) as i64
}

fn today_date_string() -> Result<String> {
    let output = Command::new("date")
        .args(["+%F"])
        .output()
        .into_diagnostic()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!("date +%F failed: {stderr}");
    }

    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_string())
        .map_err(|error| miette!("date output was not valid UTF-8: {error}"))
}

fn emit_report(report: &FreshnessReport, format: FreshnessOutputFormat) {
    match format {
        FreshnessOutputFormat::Text => emit_text_report(report),
        FreshnessOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(report).expect("freshness report should serialize")
        ),
    }
}

fn emit_text_report(report: &FreshnessReport) {
    let status = if report.summary.critical_count > 0 {
        "critical stale docs found"
    } else if report.summary.warn_count > 0 || report.summary.invalid_review_reference_count > 0 {
        "stale docs need attention"
    } else {
        "pass"
    };
    println!("Docpact freshness: {status}.");
    println!(
        "Summary: governed_docs={}, fresh_docs={}, stale_docs={}, warn={}, critical={}, invalid_review_references={}",
        report.summary.governed_doc_count,
        report.summary.fresh_doc_count,
        report.summary.stale_doc_count,
        report.summary.warn_count,
        report.summary.critical_count,
        report.summary.invalid_review_reference_count,
    );
    emit_items("Critical docs", report, FreshnessLevel::Critical);
    emit_items("Warn docs", report, FreshnessLevel::Warn);
    emit_invalid_review_references(report);
    if status == "pass" {
        println!("Next: no freshness action required.");
    } else {
        println!("Next: review stale docs and refresh lastReviewedAt / lastReviewedCommit.");
    }
}

fn emit_items(label: &str, report: &FreshnessReport, level: FreshnessLevel) {
    println!("{label}:");
    let items = report
        .items
        .iter()
        .filter(|item| item.staleness_level == level.as_str())
        .collect::<Vec<_>>();

    if items.is_empty() {
        println!("- none");
        return;
    }

    for item in items.into_iter().take(10) {
        println!(
            "- review {} (commits_since_review={}, days_since_review={}, associated_paths={})",
            item.path,
            item.commits_since_review
                .map(|value| value.to_string())
                .unwrap_or_else(|| "n/a".into()),
            item.days_since_review
                .map(|value| value.to_string())
                .unwrap_or_else(|| "n/a".into()),
            item.associated_changed_paths_count,
        );
    }
}

fn emit_invalid_review_references(report: &FreshnessReport) {
    println!("Invalid review references:");
    let items = report
        .items
        .iter()
        .filter(|item| !item.review_reference_problems.is_empty())
        .collect::<Vec<_>>();

    if items.is_empty() {
        println!("- none");
        return;
    }

    for item in items.into_iter().take(10) {
        println!(
            "- fix review metadata for {} ({})",
            item.path,
            item.review_reference_problems.join(",")
        );
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::AppExit;
    use crate::cli::FreshnessOutputFormat;

    use super::{
        FRESHNESS_AUDIT_SCHEMA_VERSION, FreshnessArgs, FreshnessLevel, execute_with_today,
        parse_iso_date, run,
    };

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be valid")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{nanos}-{}", std::process::id()));
        fs::create_dir_all(&path).expect("temp dir should be created");
        path
    }

    fn init_git_repo(root: &Path) {
        fs::create_dir_all(root).expect("repo root should exist");
        git(root, &["init"]);
        git(root, &["config", "user.email", "docpact@example.com"]);
        git(root, &["config", "user.name", "Docpact Tests"]);
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git command should run");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git stdout should be utf-8")
            .trim()
            .to_string()
    }

    fn base_args(root: PathBuf) -> FreshnessArgs {
        FreshnessArgs {
            root: Some(root),
            config: None,
            format: FreshnessOutputFormat::Json,
        }
    }

    #[test]
    fn parse_iso_date_rejects_invalid_calendar_dates() {
        assert!(parse_iso_date("2026-02-29").is_err());
        assert!(parse_iso_date("2026-13-01").is_err());
        assert!(parse_iso_date("2026-04-31").is_err());
    }

    #[test]
    fn freshness_report_surfaces_warn_critical_and_invalid_review_references() {
        let root = temp_dir("docpact-freshness-repo");
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
  warn_after_commits: 1
  warn_after_days: 30
  critical_after_days: 180
repo:
  id: example
rules:
  - id: api-guide
    scope: repo
    repo: example
    triggers:
      - path: src/api/**
        kind: code
    requiredDocs:
      - path: docs/api.md
        mode: review_or_update
      - path: docs/legacy.md
        mode: review_or_update
      - path: docs/broken.md
        mode: review_or_update
    reason: api
"#,
        )
        .expect("config");
        fs::write(root.join("src/api/client.ts"), "export const api = 1;\n").expect("src");
        fs::write(
            root.join("docs/api.md"),
            "---\nlastReviewedAt: 2026-04-20\nlastReviewedCommit: pending\n---\n# API\n",
        )
        .expect("api doc");
        fs::write(
            root.join("docs/legacy.md"),
            "---\nlastReviewedAt: 2025-01-01\nlastReviewedCommit: pending\n---\n# Legacy\n",
        )
        .expect("legacy doc");
        fs::write(
            root.join("docs/broken.md"),
            "---\nlastReviewedAt: nope\nlastReviewedCommit: deadbeef\n---\n# Broken\n",
        )
        .expect("broken doc");

        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);
        let base_commit = git(&root, &["rev-parse", "HEAD"]);

        fs::write(
            root.join("docs/api.md"),
            format!(
                "---\nlastReviewedAt: 2026-04-20\nlastReviewedCommit: {base_commit}\n---\n# API\n"
            ),
        )
        .expect("api doc updated");
        fs::write(
            root.join("docs/legacy.md"),
            format!(
                "---\nlastReviewedAt: 2025-01-01\nlastReviewedCommit: {base_commit}\n---\n# Legacy\n"
            ),
        )
        .expect("legacy doc updated");
        git(&root, &["add", "docs/api.md", "docs/legacy.md"]);
        git(&root, &["commit", "-m", "record review reference"]);

        fs::write(root.join("src/api/client.ts"), "export const api = 2;\n").expect("src update");
        git(&root, &["add", "src/api/client.ts"]);
        git(&root, &["commit", "-m", "change api"]);

        let report =
            execute_with_today(&base_args(root), "2026-04-21").expect("freshness should execute");

        assert_eq!(report.schema_version, FRESHNESS_AUDIT_SCHEMA_VERSION);
        assert_eq!(report.command, "freshness");
        assert!(report.warnings.is_empty());
        assert_eq!(report.summary.governed_doc_count, 3);
        assert_eq!(report.summary.warn_count, 1);
        assert_eq!(report.summary.critical_count, 1);
        assert_eq!(report.summary.invalid_review_reference_count, 1);

        let api = report
            .items
            .iter()
            .find(|item| item.path == "docs/api.md")
            .expect("api doc result");
        assert_eq!(api.staleness_level, FreshnessLevel::Warn.as_str());
        assert_eq!(api.commits_since_review, Some(1));
        assert_eq!(api.days_since_review, Some(1));

        let legacy = report
            .items
            .iter()
            .find(|item| item.path == "docs/legacy.md")
            .expect("legacy doc result");
        assert_eq!(legacy.staleness_level, FreshnessLevel::Critical.as_str());
        assert_eq!(legacy.days_since_review, Some(475));

        let broken = report
            .items
            .iter()
            .find(|item| item.path == "docs/broken.md")
            .expect("broken doc result");
        assert_eq!(broken.staleness_level, FreshnessLevel::Ok.as_str());
        assert!(
            broken
                .review_reference_problems
                .contains(&"invalid-lastReviewedCommit".to_string())
        );
        assert!(
            broken
                .review_reference_problems
                .contains(&"invalid-lastReviewedAt".to_string())
        );
    }

    #[test]
    fn workspace_freshness_uses_workspace_relative_required_docs() {
        let root = temp_dir("docpact-freshness-workspace");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("root doc dir");
        fs::create_dir_all(root.join("subrepo/.docpact")).expect("subrepo doc dir");
        fs::create_dir_all(root.join("subrepo/src/api")).expect("subrepo src dir");
        fs::create_dir_all(root.join("subrepo/docs")).expect("subrepo docs dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: workspace
rules: []
"#,
        )
        .expect("root config");
        fs::write(
            root.join("subrepo/.docpact/config.yaml"),
            r#"
version: 1
layout: repo
freshness:
  warn_after_commits: 1
  warn_after_days: 90
  critical_after_days: 180
repo:
  id: subrepo
rules:
  - id: api-doc
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
            root.join("subrepo/docs/api.md"),
            "---\nlastReviewedAt: 2026-04-20\nlastReviewedCommit: pending\n---\n# API\n",
        )
        .expect("doc");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);
        let base_commit = git(&root, &["rev-parse", "HEAD"]);
        fs::write(
            root.join("subrepo/docs/api.md"),
            format!(
                "---\nlastReviewedAt: 2026-04-20\nlastReviewedCommit: {base_commit}\n---\n# API\n"
            ),
        )
        .expect("doc review reference");
        git(&root, &["add", "subrepo/docs/api.md"]);
        git(&root, &["commit", "-m", "record review reference"]);
        fs::write(
            root.join("subrepo/src/api/client.ts"),
            "export const api = 2;\n",
        )
        .expect("src update");
        git(&root, &["add", "subrepo/src/api/client.ts"]);
        git(&root, &["commit", "-m", "change api"]);

        let report =
            execute_with_today(&base_args(root), "2026-04-21").expect("freshness should execute");
        let item = report
            .items
            .iter()
            .find(|item| item.path == "subrepo/docs/api.md")
            .expect("workspace doc result");

        assert_eq!(item.staleness_level, FreshnessLevel::Warn.as_str());
        assert_eq!(item.commits_since_review, Some(1));
        assert_eq!(
            item.associated_changed_paths,
            vec!["subrepo/src/api/client.ts".to_string()]
        );
    }

    #[test]
    fn freshness_command_returns_success_exit() {
        let root = temp_dir("docpact-freshness-run");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
repo:
  id: example
rules: []
"#,
        )
        .expect("config");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);

        let exit = run(base_args(root)).expect("freshness run should execute");
        assert_eq!(exit, AppExit::Success);
    }
}
