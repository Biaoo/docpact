//! Implementation of `docpact route`.
//!
//! Route recommends governed and advisory documents to read before coding,
//! using paths, module scopes, or controlled intents as deterministic inputs.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use miette::{IntoDiagnostic, Result, bail};
use serde::Serialize;

use crate::AppExit;
use crate::cli::{RouteArgs, RouteDetail, RouteOutputFormat};
use crate::config::{
    CatalogRepo, ConfigScopeKind, LoadedCatalogConfig, LoadedRoutingConfig, OwnershipPathAnalysis,
    analyze_ownership_paths, load_catalog_configs, load_impact_files, load_ownership_configs,
    load_routing_configs, normalize_path, resolve_rule_path, root_dir_from_option,
};
use crate::freshness::{FreshnessItem, RouteFreshnessTarget, execute_route_freshness_with_today};
use crate::git::get_tracked_paths;
use crate::rules::{RequiredDocMode, matches_pattern};

pub const ROUTE_SCHEMA_VERSION: &str = "docpact.route.v4";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RouteReport {
    pub schema_version: String,
    pub tool_name: String,
    pub tool_version: String,
    pub command: String,
    pub summary: RouteSummary,
    #[serde(default)]
    pub warnings: Vec<RouteWarning>,
    pub governed_docs: Vec<RouteRecommendation>,
    pub advisory_docs: Vec<RouteAdvisoryDoc>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RouteSummary {
    pub input_path_count: usize,
    pub module_input_count: usize,
    pub intent_input_count: usize,
    pub matched_rule_count: usize,
    pub governed_doc_count: usize,
    pub advisory_doc_count: usize,
    pub freshness_warning_count: usize,
    pub critical_freshness_count: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RouteWarning {
    pub code: String,
    pub input: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RouteRecommendation {
    pub path: String,
    pub priority: String,
    pub match_reason: RouteMatchReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ownership_context: Option<RouteOwnershipContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_context: Option<RouteRepoContext>,
    pub tie_break_context: RouteGovernedTieBreak,
    pub score_breakdown: RouteScoreBreakdown,
    pub freshness_level: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness_warning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub review_reference_problems: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_sources: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rule_sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RouteAdvisoryDoc {
    pub path: String,
    pub repo_id: String,
    pub pointer_types: Vec<String>,
    pub matched_input_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_repo: Option<String>,
    pub tie_break_context: RouteAdvisoryTieBreak,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_sources: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pointer_sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RouteMatchReason {
    pub rule_ids: Vec<String>,
    pub matched_input_paths: Vec<String>,
    pub matched_trigger_paths: Vec<String>,
    pub modes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RouteScoreBreakdown {
    pub mode_score: usize,
    pub specificity_score: usize,
    pub matched_input_count: usize,
    pub matched_rule_count: usize,
    pub freshness_penalty: usize,
    pub total_score: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RouteOwnershipContext {
    pub owner_repos: Vec<String>,
    pub non_owner_repos: Vec<String>,
    pub domain_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_patterns: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domain_sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RouteRepoContext {
    pub repo_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub canonical_repos: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RouteGovernedTieBreak {
    pub ownership_context_present: bool,
    pub repo_context_present: bool,
    pub owner_repo_count: usize,
    pub repo_context_count: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RouteAdvisoryTieBreak {
    pub pointer_priority: usize,
    pub matched_input_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedInput {
    original: String,
    candidates: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedInputs {
    explicit_path_count: usize,
    module_count: usize,
    intent_count: usize,
    resolved_inputs: Vec<ResolvedInput>,
    warnings: Vec<RouteWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecommendationBuilder {
    path: String,
    rule_ids: BTreeSet<String>,
    matched_input_paths: BTreeSet<String>,
    matched_candidate_paths: BTreeSet<String>,
    matched_trigger_paths: BTreeSet<String>,
    modes: BTreeSet<RequiredDocMode>,
    config_sources: BTreeSet<String>,
    rule_sources: BTreeSet<String>,
    best_specificity_score: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AdvisoryDocBuilder {
    path: String,
    repo_id: String,
    canonical_repo: Option<String>,
    pointer_types: BTreeSet<String>,
    matched_input_paths: BTreeSet<String>,
    config_sources: BTreeSet<String>,
    pointer_sources: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CatalogPathContext {
    repo_id: String,
    canonical_repo: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IntentEntry {
    source: String,
    base_dir: String,
    scope_kind: ConfigScopeKind,
    repo_id: Option<String>,
    patterns: Vec<String>,
}

pub fn run(args: RouteArgs) -> Result<AppExit> {
    let report = execute(&args)?;
    emit_report(&report, args.format, args.detail, args.limit);
    Ok(AppExit::Success)
}

pub fn execute(args: &RouteArgs) -> Result<RouteReport> {
    let today = today_date_string()?;
    execute_with_today(args, &today)
}

fn execute_with_today(args: &RouteArgs, today: &str) -> Result<RouteReport> {
    let root_dir = root_dir_from_option(args.root.as_deref())?;
    let loaded_rules = load_impact_files(&root_dir, args.config.as_deref())?;
    let routing_configs = load_routing_configs(&root_dir, args.config.as_deref())?;
    let catalog_configs = load_catalog_configs(&root_dir, args.config.as_deref())?;
    let ownership_configs = load_ownership_configs(&root_dir, args.config.as_deref())?;
    let prepared_inputs = prepare_inputs(&root_dir, args, &routing_configs)?;
    let analyzed_paths = prepared_inputs
        .resolved_inputs
        .iter()
        .flat_map(|input| input.candidates.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let ownership_analysis = analyze_ownership_paths(&analyzed_paths, &ownership_configs);
    let ownership_index = ownership_index(&ownership_analysis);
    let catalog_path_index = build_catalog_path_index(&analyzed_paths, &catalog_configs);

    let mut matched_rule_keys = BTreeSet::new();
    let mut recommendations = BTreeMap::<String, RecommendationBuilder>::new();

    for input in &prepared_inputs.resolved_inputs {
        for candidate_path in &input.candidates {
            for loaded in &loaded_rules {
                let matched_triggers = loaded
                    .rule
                    .triggers
                    .iter()
                    .map(|trigger| resolve_rule_path(&loaded.base_dir, &trigger.path))
                    .filter(|trigger_path| matches_pattern(candidate_path, trigger_path))
                    .collect::<Vec<_>>();

                if matched_triggers.is_empty() {
                    continue;
                }

                matched_rule_keys.insert(format!("{}::{}", loaded.config_source, loaded.rule.id));
                let specificity_score = matched_triggers
                    .iter()
                    .map(|trigger| trigger_specificity_score(trigger))
                    .max()
                    .unwrap_or_default();

                for required_doc in &loaded.rule.required_docs {
                    let path = resolve_rule_path(&loaded.base_dir, &required_doc.path);
                    let entry = recommendations.entry(path.clone()).or_insert_with(|| {
                        RecommendationBuilder {
                            path,
                            rule_ids: BTreeSet::new(),
                            matched_input_paths: BTreeSet::new(),
                            matched_candidate_paths: BTreeSet::new(),
                            matched_trigger_paths: BTreeSet::new(),
                            modes: BTreeSet::new(),
                            config_sources: BTreeSet::new(),
                            rule_sources: BTreeSet::new(),
                            best_specificity_score: 0,
                        }
                    });
                    entry.rule_ids.insert(loaded.rule.id.clone());
                    entry.matched_input_paths.insert(input.original.clone());
                    entry.matched_candidate_paths.insert(candidate_path.clone());
                    entry
                        .matched_trigger_paths
                        .extend(matched_triggers.iter().cloned());
                    entry
                        .modes
                        .insert(RequiredDocMode::from_option(required_doc.mode.as_deref()));
                    entry.config_sources.insert(loaded.config_source.clone());
                    entry.rule_sources.insert(loaded.source.clone());
                    entry.best_specificity_score =
                        entry.best_specificity_score.max(specificity_score);
                }
            }
        }
    }

    let freshness_targets = recommendations
        .values()
        .map(|entry| RouteFreshnessTarget {
            path: entry.path.clone(),
            config_sources: entry.config_sources.iter().cloned().collect(),
            associated_patterns: entry.matched_trigger_paths.iter().cloned().collect(),
        })
        .collect::<Vec<_>>();
    let freshness_by_path = execute_route_freshness_with_today(
        &root_dir,
        args.config.as_deref(),
        &freshness_targets,
        today,
    )?;

    let include_sources = args.detail == RouteDetail::Full;
    let mut governed_docs = recommendations
        .into_values()
        .map(|entry| {
            let freshness = freshness_by_path.get(&entry.path);
            build_recommendation(
                entry,
                freshness,
                include_sources,
                &ownership_index,
                &catalog_path_index,
            )
        })
        .collect::<Vec<_>>();
    let mut advisory_docs =
        build_advisory_docs(&prepared_inputs, &catalog_configs, include_sources);

    governed_docs.sort_by(compare_recommendations);
    advisory_docs.sort_by(compare_advisory_docs);

    let freshness_warning_count = governed_docs
        .iter()
        .filter(|item| item.freshness_level != "ok" || !item.review_reference_problems.is_empty())
        .count();
    let critical_freshness_count = governed_docs
        .iter()
        .filter(|item| item.freshness_level == "critical")
        .count();
    let mut warnings = prepared_inputs.warnings;
    if !analyzed_paths.is_empty() && matched_rule_keys.is_empty() {
        warnings.push(RouteWarning {
            code: "no-rule-matches".into(),
            input: analyzed_paths.join(","),
            message: "Route inputs resolved to tracked paths, but no rule triggers matched them."
                .into(),
        });
    }
    if !analyzed_paths.is_empty() && governed_docs.is_empty() && advisory_docs.is_empty() {
        warnings.push(RouteWarning {
            code: "no-route-recommendations".into(),
            input: analyzed_paths.join(","),
            message:
                "Route inputs resolved to paths, but produced no governed docs or advisory docs."
                    .into(),
        });
    }

    Ok(RouteReport {
        schema_version: ROUTE_SCHEMA_VERSION.into(),
        tool_name: env!("CARGO_PKG_NAME").into(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        command: "route".into(),
        summary: RouteSummary {
            input_path_count: prepared_inputs.explicit_path_count,
            module_input_count: prepared_inputs.module_count,
            intent_input_count: prepared_inputs.intent_count,
            matched_rule_count: matched_rule_keys.len(),
            governed_doc_count: governed_docs.len(),
            advisory_doc_count: advisory_docs.len(),
            freshness_warning_count,
            critical_freshness_count,
        },
        warnings,
        governed_docs,
        advisory_docs,
    })
}

fn build_recommendation(
    entry: RecommendationBuilder,
    freshness: Option<&FreshnessItem>,
    include_sources: bool,
    ownership_index: &BTreeMap<String, OwnershipPathAnalysis>,
    catalog_path_index: &BTreeMap<String, CatalogPathContext>,
) -> RouteRecommendation {
    let mode_score = entry
        .modes
        .iter()
        .map(|mode| mode_score(*mode))
        .max()
        .unwrap_or_default();
    let matched_input_count = entry.matched_input_paths.len();
    let matched_rule_count = entry.rule_ids.len();
    let base_score = mode_score
        + entry.best_specificity_score
        + matched_input_count * 3
        + matched_rule_count * 2;
    let freshness_penalty = freshness.map(freshness_penalty).unwrap_or_default();
    let total_score = base_score.saturating_sub(freshness_penalty);
    let priority = priority_from_score(total_score);
    let freshness_level = freshness
        .map(|item| item.staleness_level.clone())
        .unwrap_or_else(|| "ok".into());
    let review_reference_problems = freshness
        .map(|item| item.review_reference_problems.clone())
        .unwrap_or_default();
    let freshness_warning = build_freshness_warning(freshness);
    let ownership_context = build_governed_ownership_context(
        &entry.matched_candidate_paths,
        ownership_index,
        include_sources,
    );
    let repo_context =
        build_governed_repo_context(&entry.matched_candidate_paths, catalog_path_index);
    let tie_break_context = RouteGovernedTieBreak {
        ownership_context_present: ownership_context.is_some(),
        repo_context_present: repo_context.is_some(),
        owner_repo_count: ownership_context
            .as_ref()
            .map(|context| context.owner_repos.len())
            .unwrap_or(0),
        repo_context_count: repo_context
            .as_ref()
            .map(|context| context.repo_ids.len())
            .unwrap_or(0),
    };

    RouteRecommendation {
        path: entry.path,
        priority: priority.into(),
        match_reason: RouteMatchReason {
            rule_ids: entry.rule_ids.into_iter().collect(),
            matched_input_paths: entry.matched_input_paths.into_iter().collect(),
            matched_trigger_paths: entry.matched_trigger_paths.into_iter().collect(),
            modes: entry
                .modes
                .into_iter()
                .map(|mode| mode.as_str().to_string())
                .collect(),
        },
        ownership_context,
        repo_context,
        tie_break_context,
        score_breakdown: RouteScoreBreakdown {
            mode_score,
            specificity_score: entry.best_specificity_score,
            matched_input_count,
            matched_rule_count,
            freshness_penalty,
            total_score,
        },
        freshness_level,
        freshness_warning,
        review_reference_problems,
        config_sources: if include_sources {
            entry.config_sources.into_iter().collect()
        } else {
            Vec::new()
        },
        rule_sources: if include_sources {
            entry.rule_sources.into_iter().collect()
        } else {
            Vec::new()
        },
    }
}

fn compare_recommendations(left: &RouteRecommendation, right: &RouteRecommendation) -> Ordering {
    right
        .score_breakdown
        .total_score
        .cmp(&left.score_breakdown.total_score)
        .then_with(|| {
            right
                .score_breakdown
                .mode_score
                .cmp(&left.score_breakdown.mode_score)
        })
        .then_with(|| {
            right
                .score_breakdown
                .specificity_score
                .cmp(&left.score_breakdown.specificity_score)
        })
        .then_with(|| {
            right
                .score_breakdown
                .matched_input_count
                .cmp(&left.score_breakdown.matched_input_count)
        })
        .then_with(|| {
            right
                .score_breakdown
                .matched_rule_count
                .cmp(&left.score_breakdown.matched_rule_count)
        })
        .then_with(|| {
            right
                .tie_break_context
                .ownership_context_present
                .cmp(&left.tie_break_context.ownership_context_present)
        })
        .then_with(|| {
            right
                .tie_break_context
                .repo_context_present
                .cmp(&left.tie_break_context.repo_context_present)
        })
        .then_with(|| {
            left.tie_break_context
                .owner_repo_count
                .cmp(&right.tie_break_context.owner_repo_count)
        })
        .then_with(|| {
            left.tie_break_context
                .repo_context_count
                .cmp(&right.tie_break_context.repo_context_count)
        })
        .then_with(|| left.path.cmp(&right.path))
}

fn advisory_pointer_priority(pointer_type: &str) -> usize {
    match pointer_type {
        "entryDoc" => 40,
        "branchPolicyDoc" => 30,
        "workflowDocs" => 20,
        "integrationDocs" => 10,
        _ => 0,
    }
}

fn compare_advisory_docs(left: &RouteAdvisoryDoc, right: &RouteAdvisoryDoc) -> Ordering {
    right
        .tie_break_context
        .pointer_priority
        .cmp(&left.tie_break_context.pointer_priority)
        .then_with(|| {
            right
                .tie_break_context
                .matched_input_count
                .cmp(&left.tie_break_context.matched_input_count)
        })
        .then_with(|| left.repo_id.cmp(&right.repo_id))
        .then_with(|| left.path.cmp(&right.path))
}

fn mode_score(mode: RequiredDocMode) -> usize {
    match mode {
        RequiredDocMode::BodyUpdateRequired => 40,
        RequiredDocMode::MetadataRefreshRequired => 30,
        RequiredDocMode::ReviewOrUpdate => 20,
        RequiredDocMode::MustExist => 10,
    }
}

fn priority_from_score(total_score: usize) -> &'static str {
    if total_score >= 50 {
        "high"
    } else if total_score >= 30 {
        "medium"
    } else {
        "low"
    }
}

fn trigger_specificity_score(pattern: &str) -> usize {
    let segments = pattern
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let segment_count = segments.len();
    let wildcard_segments = segments
        .iter()
        .filter(|segment| segment.contains('*') || segment.contains('?'))
        .count();
    let recursive_segments = segments.iter().filter(|segment| **segment == "**").count();
    let literal_segments = segment_count.saturating_sub(wildcard_segments);
    let literal_chars = pattern
        .chars()
        .filter(|ch| *ch != '*' && *ch != '?' && *ch != '/')
        .count();

    let raw_score = literal_segments * 4 + (literal_chars.min(12) / 2) + segment_count.min(4);
    let penalty = wildcard_segments * 3 + recursive_segments * 4;

    raw_score.saturating_sub(penalty).min(20)
}

fn prepare_inputs(
    root_dir: &Path,
    args: &RouteArgs,
    routing_configs: &[LoadedRoutingConfig],
) -> Result<PreparedInputs> {
    let explicit_paths = parse_optional_csv_inputs(args.paths.as_deref())?;
    let modules = parse_named_inputs(&args.module, "module")?;
    let intents = parse_named_inputs(&args.intent, "intent")?;

    if explicit_paths.is_empty() && modules.is_empty() && intents.is_empty() {
        bail!("Pass at least one non-empty route input through --paths, --module, or --intent.");
    }

    let tracked_paths = get_tracked_paths(root_dir)?;
    let mut resolved_inputs = Vec::new();
    let mut warnings = Vec::new();

    for input in &explicit_paths {
        if has_glob_syntax(input) {
            let candidates = tracked_paths
                .iter()
                .filter(|tracked| matches_pattern(tracked, input))
                .cloned()
                .collect::<Vec<_>>();
            if candidates.is_empty() {
                warnings.push(no_tracked_path_matches_warning(input));
            }
            resolved_inputs.push(ResolvedInput {
                original: input.clone(),
                candidates,
            });
        } else {
            resolved_inputs.push(ResolvedInput {
                original: input.clone(),
                candidates: vec![input.clone()],
            });
        }
    }

    for module in &modules {
        if has_glob_syntax(module) {
            bail!(
                "`--module` does not accept glob syntax; pass a repo-relative path prefix instead."
            );
        }

        let prefix = module.trim_end_matches('/').to_string();
        let candidates = tracked_paths
            .iter()
            .filter(|tracked| matches_module_scope(tracked, &prefix))
            .cloned()
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            warnings.push(no_tracked_path_matches_warning(&format!("module:{prefix}")));
        }
        resolved_inputs.push(ResolvedInput {
            original: format!("module:{prefix}"),
            candidates,
        });
    }

    let scope_hints = infer_scope_hints(&resolved_inputs, routing_configs);
    let intent_index = build_intent_table(routing_configs);
    for intent in &intents {
        let Some(entries) = intent_index.get(intent) else {
            let available = intent_index.keys().cloned().collect::<Vec<_>>();
            if available.is_empty() {
                bail!(
                    "Unknown routing intent alias `{intent}`. No routing intent aliases are configured. Try `docpact render --view routing-summary --format text` to inspect effective routing configuration."
                );
            }
            bail!(
                "Unknown routing intent alias `{intent}`. Available aliases: {}. Try `docpact render --view routing-summary --format text` to inspect effective routing configuration.",
                available.join(", ")
            );
        };
        let patterns = resolve_intent_patterns(intent, entries, &scope_hints)?;

        let candidates = tracked_paths
            .iter()
            .filter(|tracked| {
                patterns
                    .iter()
                    .any(|pattern| matches_pattern(tracked, pattern))
            })
            .cloned()
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            warnings.push(no_tracked_path_matches_warning(&format!("intent:{intent}")));
        }
        resolved_inputs.push(ResolvedInput {
            original: format!("intent:{intent}"),
            candidates,
        });
    }

    Ok(PreparedInputs {
        explicit_path_count: explicit_paths.len(),
        module_count: modules.len(),
        intent_count: intents.len(),
        resolved_inputs,
        warnings,
    })
}

fn no_tracked_path_matches_warning(input: &str) -> RouteWarning {
    RouteWarning {
        code: "no-tracked-path-matches".into(),
        input: input.into(),
        message: "Route input did not match any git tracked paths before rule matching.".into(),
    }
}

fn parse_optional_csv_inputs(values: Option<&str>) -> Result<Vec<String>> {
    Ok(values
        .unwrap_or_default()
        .split(',')
        .map(|value| normalize_path(value.trim()))
        .filter(|value| !value.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>())
}

fn parse_named_inputs(values: &[String], flag_name: &str) -> Result<Vec<String>> {
    let parsed = values
        .iter()
        .map(|value| normalize_path(value.trim()))
        .filter(|value| !value.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    if values.iter().any(|value| value.trim().is_empty()) {
        bail!("`--{flag_name}` must not include empty values.");
    }

    Ok(parsed)
}

fn build_intent_table(
    routing_configs: &[LoadedRoutingConfig],
) -> BTreeMap<String, Vec<IntentEntry>> {
    let mut index = BTreeMap::<String, Vec<IntentEntry>>::new();

    for loaded in routing_configs {
        for (alias, intent) in &loaded.routing.intents {
            let resolved_patterns = intent
                .paths
                .iter()
                .map(|pattern| resolve_rule_path(&loaded.base_dir, pattern))
                .collect::<Vec<_>>();

            index.entry(alias.clone()).or_default().push(IntentEntry {
                source: loaded.source.clone(),
                base_dir: loaded.base_dir.clone(),
                scope_kind: loaded.scope_kind,
                repo_id: loaded.repo_id.clone(),
                patterns: resolved_patterns,
            });
        }
    }

    index
}

fn resolve_intent_patterns(
    alias: &str,
    entries: &[IntentEntry],
    scope_hints: &BTreeSet<String>,
) -> Result<Vec<String>> {
    if entries.len() == 1 {
        return Ok(entries[0].patterns.clone());
    }

    let root_entries = entries
        .iter()
        .filter(|entry| entry.scope_kind == ConfigScopeKind::WorkspaceRoot)
        .collect::<Vec<_>>();
    if root_entries.len() == 1 {
        return Ok(root_entries[0].patterns.clone());
    }

    if !scope_hints.is_empty() {
        let scoped_matches = entries
            .iter()
            .filter(|entry| !entry.base_dir.is_empty() && scope_hints.contains(&entry.base_dir))
            .collect::<Vec<_>>();
        if scoped_matches.len() == 1 {
            return Ok(scoped_matches[0].patterns.clone());
        }
    }

    let scopes = entries
        .iter()
        .map(|entry| {
            format!(
                "{} scope={} repo_id={} base_dir={}",
                entry.source,
                entry.scope_kind.as_str(),
                entry.repo_id.as_deref().unwrap_or("-"),
                if entry.base_dir.is_empty() {
                    "."
                } else {
                    entry.base_dir.as_str()
                }
            )
        })
        .collect::<Vec<_>>()
        .join("; ");

    bail!(
        "routing intent alias `{alias}` exists in multiple repo scopes. Matched scopes: {scopes}. Try adding --paths <repo-path/...> or --module <repo-path> to disambiguate, or inspect aliases with `docpact render --view routing-summary --format text`."
    );
}

fn infer_scope_hints(
    resolved_inputs: &[ResolvedInput],
    routing_configs: &[LoadedRoutingConfig],
) -> BTreeSet<String> {
    let mut hints = BTreeSet::new();

    for input in resolved_inputs {
        for candidate in &input.candidates {
            for config in routing_configs {
                if config.base_dir.is_empty() {
                    continue;
                }
                if matches_module_scope(candidate, &config.base_dir) {
                    hints.insert(config.base_dir.clone());
                }
            }
        }
    }

    hints
}

fn matches_module_scope(tracked_path: &str, module: &str) -> bool {
    tracked_path == module
        || tracked_path
            .strip_prefix(module)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn has_glob_syntax(value: &str) -> bool {
    value.contains('*') || value.contains('?')
}

fn freshness_penalty(item: &FreshnessItem) -> usize {
    match item.staleness_level.as_str() {
        "critical" => 20,
        "warn" => 10,
        _ => 0,
    }
}

fn build_freshness_warning(item: Option<&FreshnessItem>) -> Option<String> {
    let item = item?;
    let mut parts = Vec::new();

    match item.staleness_level.as_str() {
        "critical" => parts.push("potentially stale (critical)".to_string()),
        "warn" => parts.push("potentially stale (warn)".to_string()),
        _ => {}
    }

    if !item.review_reference_problems.is_empty() {
        parts.push(format!(
            "review references: {}",
            item.review_reference_problems.join(",")
        ));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

fn build_advisory_docs(
    prepared_inputs: &PreparedInputs,
    catalog_configs: &[LoadedCatalogConfig],
    include_sources: bool,
) -> Vec<RouteAdvisoryDoc> {
    let mut builders = BTreeMap::<String, AdvisoryDocBuilder>::new();

    for input in &prepared_inputs.resolved_inputs {
        for candidate_path in &input.candidates {
            for loaded in catalog_configs {
                for repo in &loaded.catalog.repos {
                    let repo_root = resolve_catalog_repo_root(&loaded.base_dir, &repo.path);
                    if !matches_catalog_repo_scope(candidate_path, &repo_root) {
                        continue;
                    }

                    add_catalog_pointer(
                        &mut builders,
                        loaded,
                        repo,
                        &repo_root,
                        repo.entry_doc.as_deref(),
                        "entryDoc",
                        &input.original,
                    );
                    add_catalog_pointer(
                        &mut builders,
                        loaded,
                        repo,
                        &repo_root,
                        repo.branch_policy_doc.as_deref(),
                        "branchPolicyDoc",
                        &input.original,
                    );
                    for path in &repo.workflow_docs {
                        add_catalog_pointer(
                            &mut builders,
                            loaded,
                            repo,
                            &repo_root,
                            Some(path.as_str()),
                            "workflowDocs",
                            &input.original,
                        );
                    }
                    for path in &repo.integration_docs {
                        add_catalog_pointer(
                            &mut builders,
                            loaded,
                            repo,
                            &repo_root,
                            Some(path.as_str()),
                            "integrationDocs",
                            &input.original,
                        );
                    }
                }
            }
        }
    }

    builders
        .into_values()
        .map(|builder| {
            let pointer_priority = builder
                .pointer_types
                .iter()
                .map(|pointer| advisory_pointer_priority(pointer))
                .max()
                .unwrap_or_default();
            let matched_input_count = builder.matched_input_paths.len();

            RouteAdvisoryDoc {
                path: builder.path,
                repo_id: builder.repo_id,
                pointer_types: builder.pointer_types.into_iter().collect(),
                matched_input_paths: builder.matched_input_paths.into_iter().collect(),
                canonical_repo: builder.canonical_repo,
                tie_break_context: RouteAdvisoryTieBreak {
                    pointer_priority,
                    matched_input_count,
                },
                config_sources: if include_sources {
                    builder.config_sources.into_iter().collect()
                } else {
                    Vec::new()
                },
                pointer_sources: if include_sources {
                    builder.pointer_sources.into_iter().collect()
                } else {
                    Vec::new()
                },
            }
        })
        .collect()
}

fn add_catalog_pointer(
    builders: &mut BTreeMap<String, AdvisoryDocBuilder>,
    loaded: &LoadedCatalogConfig,
    repo: &CatalogRepo,
    repo_root: &str,
    pointer: Option<&str>,
    pointer_type: &str,
    matched_input: &str,
) {
    let Some(pointer) = pointer else {
        return;
    };

    let path = resolve_catalog_doc_pointer(repo_root, pointer);
    let entry = builders
        .entry(path.clone())
        .or_insert_with(|| AdvisoryDocBuilder {
            path,
            repo_id: repo.id.clone(),
            canonical_repo: repo.canonical_repo.clone(),
            pointer_types: BTreeSet::new(),
            matched_input_paths: BTreeSet::new(),
            config_sources: BTreeSet::new(),
            pointer_sources: BTreeSet::new(),
        });
    entry.pointer_types.insert(pointer_type.to_string());
    entry.matched_input_paths.insert(matched_input.to_string());
    entry.config_sources.insert(loaded.source.clone());
    entry.pointer_sources.insert(format!(
        "{}#catalog.repos.{}.{}",
        loaded.source, repo.id, pointer_type
    ));
}

fn resolve_catalog_repo_root(base_dir: &str, repo_path: &str) -> String {
    if repo_path == "." {
        normalize_path(base_dir)
    } else {
        resolve_rule_path(base_dir, repo_path)
    }
}

fn resolve_catalog_doc_pointer(repo_root: &str, pointer: &str) -> String {
    if repo_root.is_empty() {
        normalize_path(pointer)
    } else {
        normalize_path(&format!("{repo_root}/{pointer}"))
    }
}

fn matches_catalog_repo_scope(candidate_path: &str, repo_root: &str) -> bool {
    repo_root.is_empty()
        || candidate_path == repo_root
        || candidate_path
            .strip_prefix(repo_root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn ownership_index(
    analysis: &crate::config::OwnershipAnalysis,
) -> BTreeMap<String, OwnershipPathAnalysis> {
    analysis
        .paths
        .iter()
        .cloned()
        .map(|item| (item.path.clone(), item))
        .collect()
}

fn build_catalog_path_index(
    candidate_paths: &[String],
    catalog_configs: &[LoadedCatalogConfig],
) -> BTreeMap<String, CatalogPathContext> {
    let mut index = BTreeMap::new();

    for candidate_path in candidate_paths {
        for loaded in catalog_configs {
            for repo in &loaded.catalog.repos {
                let repo_root = resolve_catalog_repo_root(&loaded.base_dir, &repo.path);
                if matches_catalog_repo_scope(candidate_path, &repo_root) {
                    index
                        .entry(candidate_path.clone())
                        .or_insert_with(|| CatalogPathContext {
                            repo_id: repo.id.clone(),
                            canonical_repo: repo.canonical_repo.clone(),
                        });
                }
            }
        }
    }

    index
}

fn build_governed_ownership_context(
    candidate_paths: &BTreeSet<String>,
    ownership_index: &BTreeMap<String, OwnershipPathAnalysis>,
    include_sources: bool,
) -> Option<RouteOwnershipContext> {
    let mut owner_repos = BTreeSet::new();
    let mut non_owner_repos = BTreeSet::new();
    let mut domain_ids = BTreeSet::new();
    let mut matched_patterns = BTreeSet::new();
    let mut domain_sources = BTreeSet::new();

    for path in candidate_paths {
        let Some(analysis) = ownership_index.get(path) else {
            continue;
        };
        owner_repos.insert(analysis.selected.owner_repo.clone());
        domain_ids.insert(analysis.selected.domain_id.clone());
        matched_patterns.insert(analysis.selected.matched_include.clone());
        non_owner_repos.extend(analysis.selected.non_owner_repos.iter().cloned());
        if include_sources {
            domain_sources.insert(analysis.selected.source.clone());
        }
    }

    if owner_repos.is_empty() && domain_ids.is_empty() {
        return None;
    }

    Some(RouteOwnershipContext {
        owner_repos: owner_repos.into_iter().collect(),
        non_owner_repos: non_owner_repos.into_iter().collect(),
        domain_ids: domain_ids.into_iter().collect(),
        matched_patterns: matched_patterns.into_iter().collect(),
        domain_sources: domain_sources.into_iter().collect(),
    })
}

fn build_governed_repo_context(
    candidate_paths: &BTreeSet<String>,
    catalog_path_index: &BTreeMap<String, CatalogPathContext>,
) -> Option<RouteRepoContext> {
    let mut repo_ids = BTreeSet::new();
    let mut canonical_repos = BTreeSet::new();

    for path in candidate_paths {
        let Some(context) = catalog_path_index.get(path) else {
            continue;
        };
        repo_ids.insert(context.repo_id.clone());
        if let Some(canonical_repo) = &context.canonical_repo {
            canonical_repos.insert(canonical_repo.clone());
        }
    }

    if repo_ids.is_empty() {
        return None;
    }

    Some(RouteRepoContext {
        repo_ids: repo_ids.into_iter().collect(),
        canonical_repos: canonical_repos.into_iter().collect(),
    })
}

fn today_date_string() -> Result<String> {
    let output = std::process::Command::new("date")
        .args(["+%F"])
        .output()
        .into_diagnostic()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!("date +%F failed: {stderr}");
    }

    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_string())
        .map_err(|error| miette::miette!("date output was not valid UTF-8: {error}"))
}

fn emit_report(
    report: &RouteReport,
    format: RouteOutputFormat,
    detail: RouteDetail,
    limit: Option<usize>,
) {
    match format {
        RouteOutputFormat::Text => print!("{}", render_text_report(report, detail, limit)),
        RouteOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(report).expect("route report should serialize")
        ),
    }
}

fn render_text_report(report: &RouteReport, detail: RouteDetail, limit: Option<usize>) -> String {
    let mut output = String::new();
    let status = if report.governed_docs.is_empty() && report.advisory_docs.is_empty() {
        "no document recommendations"
    } else if report.summary.critical_freshness_count > 0 {
        "recommendations include critically stale docs"
    } else {
        "recommendations ready"
    };
    output.push_str(&format!("Docpact route: {status}.\n"));
    output.push_str(&format!(
        "Summary: inputs={}, modules={}, intents={}, matched_rules={}, governed_docs={}, advisory_docs={}, freshness_warnings={}, critical_freshness={}\n",
        report.summary.input_path_count,
        report.summary.module_input_count,
        report.summary.intent_input_count,
        report.summary.matched_rule_count,
        report.summary.governed_doc_count,
        report.summary.advisory_doc_count,
        report.summary.freshness_warning_count,
        report.summary.critical_freshness_count,
    ));
    if !report.warnings.is_empty() {
        output.push_str("Warnings:\n");
        for warning in &report.warnings {
            output.push_str(&format!(
                "- {} for {}: {}\n",
                warning.code, warning.input, warning.message
            ));
        }
    }

    let governed_displayed = limit
        .map(|value| value.min(report.governed_docs.len()))
        .unwrap_or(report.governed_docs.len());
    if governed_displayed < report.governed_docs.len() {
        output.push_str(&format!(
            "Governed docs (showing {} of {}):\n",
            governed_displayed,
            report.governed_docs.len()
        ));
    } else {
        output.push_str("Governed docs:\n");
    }

    if report.governed_docs.is_empty() {
        output.push_str("- none\n");
    } else {
        for recommendation in report.governed_docs.iter().take(governed_displayed) {
            output.push_str(&format!(
                "- review or update {} (priority: {}, freshness: {}, rules: {}, inputs: {})\n",
                recommendation.path,
                recommendation.priority,
                recommendation.freshness_level,
                recommendation.match_reason.rule_ids.join(","),
                recommendation.match_reason.matched_input_paths.join(","),
            ));

            if detail == RouteDetail::Compact {
                if let Some(context) = &recommendation.ownership_context {
                    output.push_str(&format!(
                        "  ownership: owners={}, non_owners={}, domains={}\n",
                        join_or_dash(&context.owner_repos),
                        join_or_dash(&context.non_owner_repos),
                        join_or_dash(&context.domain_ids),
                    ));
                }
                if let Some(context) = &recommendation.repo_context {
                    output.push_str(&format!(
                        "  repo context: repos={}, canonical_repos={}\n",
                        join_or_dash(&context.repo_ids),
                        join_or_dash(&context.canonical_repos),
                    ));
                }
                if let Some(warning) = &recommendation.freshness_warning {
                    output.push_str(&format!("  freshness warning: {warning}\n"));
                }
                continue;
            }

            output.push_str(&format!(
                "  triggers: {}\n",
                recommendation.match_reason.matched_trigger_paths.join(",")
            ));
            output.push_str(&format!(
                "  modes: {}\n",
                recommendation.match_reason.modes.join(",")
            ));
            output.push_str(&format!(
                "  score: mode={}, specificity={}, matched_inputs={}, matched_rules={}, freshness_penalty={}, total={}\n",
                recommendation.score_breakdown.mode_score,
                recommendation.score_breakdown.specificity_score,
                recommendation.score_breakdown.matched_input_count,
                recommendation.score_breakdown.matched_rule_count,
                recommendation.score_breakdown.freshness_penalty,
                recommendation.score_breakdown.total_score,
            ));
            if let Some(warning) = &recommendation.freshness_warning {
                output.push_str(&format!("  freshness warning: {warning}\n"));
            }
            if !recommendation.review_reference_problems.is_empty() {
                output.push_str(&format!(
                    "  review reference problems: {}\n",
                    recommendation.review_reference_problems.join(",")
                ));
            }
            if let Some(context) = &recommendation.ownership_context {
                output.push_str(&format!(
                    "  ownership: owners={}, non_owners={}, domains={}\n",
                    join_or_dash(&context.owner_repos),
                    join_or_dash(&context.non_owner_repos),
                    join_or_dash(&context.domain_ids),
                ));
                if !context.matched_patterns.is_empty() {
                    output.push_str(&format!(
                        "  ownership patterns: {}\n",
                        context.matched_patterns.join(",")
                    ));
                }
                if !context.domain_sources.is_empty() {
                    output.push_str(&format!(
                        "  ownership sources: {}\n",
                        context.domain_sources.join(",")
                    ));
                }
            }
            if let Some(context) = &recommendation.repo_context {
                output.push_str(&format!(
                    "  repo context: repos={}, canonical_repos={}\n",
                    join_or_dash(&context.repo_ids),
                    join_or_dash(&context.canonical_repos),
                ));
            }
            output.push_str(&format!(
                "  tie break: ownership_context={}, repo_context={}, owner_repo_count={}, repo_context_count={}\n",
                recommendation.tie_break_context.ownership_context_present,
                recommendation.tie_break_context.repo_context_present,
                recommendation.tie_break_context.owner_repo_count,
                recommendation.tie_break_context.repo_context_count,
            ));
            if !recommendation.config_sources.is_empty() {
                output.push_str(&format!(
                    "  config sources: {}\n",
                    recommendation.config_sources.join(",")
                ));
            }
            if !recommendation.rule_sources.is_empty() {
                output.push_str(&format!(
                    "  rule sources: {}\n",
                    recommendation.rule_sources.join(",")
                ));
            }
        }
    }

    let advisory_displayed = limit
        .map(|value| value.min(report.advisory_docs.len()))
        .unwrap_or(report.advisory_docs.len());
    if advisory_displayed < report.advisory_docs.len() {
        output.push_str(&format!(
            "Advisory docs (showing {} of {}):\n",
            advisory_displayed,
            report.advisory_docs.len()
        ));
    } else {
        output.push_str("Advisory docs:\n");
    }

    if report.advisory_docs.is_empty() {
        output.push_str("- none\n");
    } else {
        for advisory in report.advisory_docs.iter().take(advisory_displayed) {
            output.push_str(&format!(
                "- read first {} (repo: {}, pointers: {}, inputs: {})\n",
                advisory.path,
                advisory.repo_id,
                advisory.pointer_types.join(","),
                advisory.matched_input_paths.join(","),
            ));

            if detail == RouteDetail::Compact {
                output.push_str(&format!(
                    "  why read first: {}; canonical_repo={}\n",
                    advisory.pointer_types.join(","),
                    advisory.canonical_repo.as_deref().unwrap_or("-"),
                ));
                continue;
            }

            output.push_str(&format!(
                "  why read first: {}; canonical_repo={}\n",
                advisory.pointer_types.join(","),
                advisory.canonical_repo.as_deref().unwrap_or("-"),
            ));
            output.push_str(&format!(
                "  tie break: pointer_priority={}, matched_inputs={}\n",
                advisory.tie_break_context.pointer_priority,
                advisory.tie_break_context.matched_input_count,
            ));
            if !advisory.config_sources.is_empty() {
                output.push_str(&format!(
                    "  config sources: {}\n",
                    advisory.config_sources.join(",")
                ));
            }
            if !advisory.pointer_sources.is_empty() {
                output.push_str(&format!(
                    "  pointer sources: {}\n",
                    advisory.pointer_sources.join(",")
                ));
            }
        }
    }

    if report.governed_docs.is_empty() && report.advisory_docs.is_empty() {
        output.push_str("Next: inspect warnings above, or run `docpact render --view routing-summary --format text` to discover intent aliases.\n");
    } else {
        output.push_str(
            "Next: read governed docs first, then advisory docs for workspace context.\n",
        );
    }

    output
}

fn join_or_dash(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_string()
    } else {
        values.join(",")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{ROUTE_SCHEMA_VERSION, execute_with_today, render_text_report};
    use crate::cli::{RouteArgs, RouteDetail, RouteOutputFormat};

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

    fn base_args(root: PathBuf, paths: &str) -> RouteArgs {
        RouteArgs {
            root: Some(root),
            config: None,
            paths: Some(paths.into()),
            module: Vec::new(),
            intent: Vec::new(),
            detail: RouteDetail::Compact,
            limit: None,
            format: RouteOutputFormat::Json,
        }
    }

    #[test]
    fn route_reports_required_docs_for_direct_paths() {
        let root = temp_dir("docpact-route-direct");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src/payments")).expect("payments dir");
        fs::create_dir_all(root.join("docs")).expect("docs dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
repo:
  id: demo
rules:
  - id: payments-docs
    scope: repo
    repo: demo
    triggers:
      - path: src/payments/**
        kind: code
    requiredDocs:
      - path: docs/payments.md
        mode: body_update_required
    reason: Keep payments docs aligned.
"#,
        )
        .expect("config should be written");
        fs::write(
            root.join("src/payments/charge.ts"),
            "export const charge = 1;\n",
        )
        .expect("source file should be written");
        fs::write(root.join("docs/payments.md"), "# Payments\n")
            .expect("doc file should be written");
        git(&root, &["add", "."]);

        let report = execute_with_today(
            &base_args(root.clone(), "src/payments/charge.ts"),
            "2026-04-22",
        )
        .expect("route report");

        assert_eq!(report.schema_version, ROUTE_SCHEMA_VERSION);
        assert_eq!(report.summary.input_path_count, 1);
        assert_eq!(report.summary.matched_rule_count, 1);
        assert_eq!(report.summary.governed_doc_count, 1);
        assert_eq!(report.summary.advisory_doc_count, 0);
        assert_eq!(report.summary.freshness_warning_count, 1);
        assert_eq!(report.summary.critical_freshness_count, 0);
        let recommendation = &report.governed_docs[0];
        assert_eq!(recommendation.path, "docs/payments.md");
        assert_eq!(recommendation.priority, "high");
        assert_eq!(recommendation.match_reason.rule_ids, vec!["payments-docs"]);
        assert_eq!(
            recommendation.match_reason.matched_input_paths,
            vec!["src/payments/charge.ts"]
        );
        assert_eq!(
            recommendation.match_reason.matched_trigger_paths,
            vec!["src/payments/**"]
        );
        assert_eq!(
            recommendation.match_reason.modes,
            vec!["body_update_required"]
        );
        assert_eq!(recommendation.score_breakdown.mode_score, 40);
        assert!(recommendation.score_breakdown.total_score >= 50);
        assert_eq!(recommendation.freshness_level, "ok");
        assert!(recommendation.freshness_warning.is_some());
        assert!(
            recommendation
                .review_reference_problems
                .contains(&"missing-lastReviewedCommit".to_string())
        );
        assert!(recommendation.config_sources.is_empty());
        assert!(recommendation.rule_sources.is_empty());
    }

    #[test]
    fn route_surfaces_ownership_and_repo_context_for_governed_docs() {
        let root = temp_dir("docpact-route-ownership-context");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src/payments")).expect("payments dir");
        fs::create_dir_all(root.join("docs")).expect("docs dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
catalog:
  repos:
    - id: demo
      path: .
      canonicalRepo: Biaoo/docpack
      entryDoc: AGENTS.md
ownership:
  domains:
    - id: payments
      paths:
        include:
          - src/payments/**
      ownerRepo: demo
      nonOwnerRepos:
        - app-shell
repo:
  id: demo
rules:
  - id: payments-docs
    scope: repo
    repo: demo
    triggers:
      - path: src/payments/**
        kind: code
    requiredDocs:
      - path: docs/payments.md
        mode: review_or_update
    reason: Keep payments docs aligned.
"#,
        )
        .expect("config should be written");
        fs::write(
            root.join("src/payments/charge.ts"),
            "export const charge = 1;\n",
        )
        .expect("source file should be written");
        fs::write(root.join("docs/payments.md"), "# Payments\n").expect("doc file");
        git(&root, &["add", "."]);

        let mut args = base_args(root.clone(), "src/payments/charge.ts");
        args.detail = RouteDetail::Full;
        let report = execute_with_today(&args, "2026-04-22").expect("route report");
        let recommendation = &report.governed_docs[0];

        assert_eq!(
            recommendation
                .ownership_context
                .as_ref()
                .map(|context| &context.owner_repos),
            Some(&vec!["demo".to_string()])
        );
        assert_eq!(
            recommendation
                .ownership_context
                .as_ref()
                .map(|context| &context.non_owner_repos),
            Some(&vec!["app-shell".to_string()])
        );
        assert_eq!(
            recommendation
                .ownership_context
                .as_ref()
                .map(|context| &context.domain_ids),
            Some(&vec!["payments".to_string()])
        );
        assert_eq!(
            recommendation
                .repo_context
                .as_ref()
                .map(|context| &context.repo_ids),
            Some(&vec!["demo".to_string()])
        );
        assert_eq!(
            recommendation
                .repo_context
                .as_ref()
                .map(|context| &context.canonical_repos),
            Some(&vec!["Biaoo/docpack".to_string()])
        );

        let compact = render_text_report(&report, RouteDetail::Compact, None);
        assert!(compact.contains("ownership: owners=demo, non_owners=app-shell, domains=payments"));
        assert!(compact.contains("repo context: repos=demo, canonical_repos=Biaoo/docpack"));

        let full = render_text_report(&report, RouteDetail::Full, None);
        assert!(full.contains("ownership patterns: src/payments/**"));
        assert!(full.contains("ownership sources: .docpact/config.yaml"));
        assert!(full.contains("tie break: ownership_context=true, repo_context=true"));
    }

    #[test]
    fn route_reports_advisory_docs_from_catalog_pointers() {
        let root = temp_dir("docpact-route-advisory");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src/payments")).expect("payments dir");
        fs::create_dir_all(root.join("docs")).expect("docs dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
catalog:
  repos:
    - id: demo
      path: .
      canonicalRepo: Biaoo/docpack
      entryDoc: AGENTS.md
      branchPolicyDoc: docs/branch-policy.md
      workflowDocs:
        - docs/workflow.md
      integrationDocs:
        - docs/integration.md
repo:
  id: demo
rules: []
"#,
        )
        .expect("config should be written");
        fs::write(
            root.join("src/payments/charge.ts"),
            "export const charge = 1;\n",
        )
        .expect("source file should be written");
        git(&root, &["add", "."]);

        let mut args = base_args(root.clone(), "src/payments/charge.ts");
        args.detail = RouteDetail::Full;
        let report = execute_with_today(&args, "2026-04-22").expect("route report");

        assert_eq!(report.summary.governed_doc_count, 0);
        assert_eq!(report.summary.advisory_doc_count, 4);
        assert!(report.governed_docs.is_empty());
        let advisory_paths = report
            .advisory_docs
            .iter()
            .map(|item| item.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            advisory_paths,
            vec![
                "AGENTS.md",
                "docs/branch-policy.md",
                "docs/workflow.md",
                "docs/integration.md",
            ]
        );
        assert_eq!(report.advisory_docs[0].pointer_types, vec!["entryDoc"]);
        assert_eq!(
            report.advisory_docs[0].matched_input_paths,
            vec!["src/payments/charge.ts"]
        );
        assert_eq!(
            report.advisory_docs[0].canonical_repo.as_deref(),
            Some("Biaoo/docpack")
        );
        assert_eq!(
            report.advisory_docs[0].tie_break_context.pointer_priority,
            40
        );
        assert_eq!(
            report.advisory_docs[0]
                .tie_break_context
                .matched_input_count,
            1
        );
        assert_eq!(
            report.advisory_docs[0].pointer_sources,
            vec![".docpact/config.yaml#catalog.repos.demo.entryDoc".to_string()]
        );

        let rendered = render_text_report(&report, RouteDetail::Full, Some(1));
        assert!(rendered.contains("why read first: entryDoc; canonical_repo=Biaoo/docpack"));
        assert!(
            rendered.contains("pointer sources: .docpact/config.yaml#catalog.repos.demo.entryDoc")
        );
    }

    #[test]
    fn route_expands_glob_inputs_against_tracked_paths() {
        let root = temp_dir("docpact-route-glob");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src/auth")).expect("auth dir");
        fs::create_dir_all(root.join("docs")).expect("docs dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
repo:
  id: demo
rules:
  - id: auth-docs
    scope: repo
    repo: demo
    triggers:
      - path: src/auth/**
        kind: code
    requiredDocs:
      - path: docs/auth.md
        mode: review_or_update
    reason: Keep auth docs aligned.
"#,
        )
        .expect("config should be written");
        fs::write(root.join("src/auth/login.ts"), "export const login = 1;\n")
            .expect("auth file should be written");
        fs::write(
            root.join("src/auth/session.ts"),
            "export const session = 1;\n",
        )
        .expect("auth session file should be written");
        fs::write(root.join("docs/auth.md"), "# Auth\n").expect("doc file should be written");
        git(&root, &["add", "."]);

        let report = execute_with_today(&base_args(root.clone(), "src/auth/**"), "2026-04-22")
            .expect("route report");

        assert_eq!(report.summary.input_path_count, 1);
        assert_eq!(report.summary.matched_rule_count, 1);
        assert_eq!(report.summary.governed_doc_count, 1);
        assert_eq!(report.summary.advisory_doc_count, 0);
        assert_eq!(report.governed_docs[0].path, "docs/auth.md");
        assert_eq!(report.governed_docs[0].priority, "medium");
        assert_eq!(
            report.governed_docs[0].match_reason.matched_input_paths,
            vec!["src/auth/**"]
        );
    }

    #[test]
    fn route_expands_module_inputs_against_tracked_paths() {
        let root = temp_dir("docpact-route-module");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src/payments")).expect("payments dir");
        fs::create_dir_all(root.join("docs")).expect("docs dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
repo:
  id: demo
rules:
  - id: payments-docs
    scope: repo
    repo: demo
    triggers:
      - path: src/payments/**
        kind: code
    requiredDocs:
      - path: docs/payments.md
        mode: review_or_update
    reason: Keep payments docs aligned.
"#,
        )
        .expect("config should be written");
        fs::write(
            root.join("src/payments/charge.ts"),
            "export const charge = 1;\n",
        )
        .expect("source file should be written");
        fs::write(root.join("docs/payments.md"), "# Payments\n")
            .expect("doc file should be written");
        git(&root, &["add", "."]);

        let mut args = base_args(root.clone(), "");
        args.paths = None;
        args.module = vec!["src/payments".into()];
        let report = execute_with_today(&args, "2026-04-22").expect("route report");

        assert_eq!(report.summary.input_path_count, 0);
        assert_eq!(report.summary.module_input_count, 1);
        assert_eq!(report.summary.intent_input_count, 0);
        assert_eq!(report.summary.governed_doc_count, 1);
        assert_eq!(report.summary.advisory_doc_count, 0);
        assert_eq!(
            report.governed_docs[0].match_reason.matched_input_paths,
            vec!["module:src/payments"]
        );
    }

    #[test]
    fn route_resolves_controlled_intent_aliases() {
        let root = temp_dir("docpact-route-intent");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src/auth")).expect("auth dir");
        fs::create_dir_all(root.join("docs")).expect("docs dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
routing:
  intents:
    auth:
      paths:
        - src/auth/**
repo:
  id: demo
rules:
  - id: auth-docs
    scope: repo
    repo: demo
    triggers:
      - path: src/auth/**
        kind: code
    requiredDocs:
      - path: docs/auth.md
        mode: body_update_required
    reason: Keep auth docs aligned.
"#,
        )
        .expect("config should be written");
        fs::write(root.join("src/auth/login.ts"), "export const login = 1;\n")
            .expect("source file should be written");
        fs::write(root.join("docs/auth.md"), "# Auth\n").expect("doc file should be written");
        git(&root, &["add", "."]);

        let mut args = base_args(root.clone(), "");
        args.paths = None;
        args.intent = vec!["auth".into()];
        let report = execute_with_today(&args, "2026-04-22").expect("route report");

        assert_eq!(report.summary.input_path_count, 0);
        assert_eq!(report.summary.module_input_count, 0);
        assert_eq!(report.summary.intent_input_count, 1);
        assert_eq!(report.summary.governed_doc_count, 1);
        assert_eq!(report.summary.advisory_doc_count, 0);
        assert_eq!(
            report.governed_docs[0].match_reason.matched_input_paths,
            vec!["intent:auth"]
        );
    }

    #[test]
    fn route_root_intent_ignores_unrelated_child_duplicate_aliases() {
        let root = temp_dir("docpact-route-workspace-root-intent");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("repo-a/.docpact")).expect("repo a config dir");
        fs::create_dir_all(root.join("repo-b/.docpact")).expect("repo b config dir");
        fs::create_dir_all(root.join("repo-a/src")).expect("repo a src");
        fs::create_dir_all(root.join("repo-b/src")).expect("repo b src");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: workspace
routing:
  intents:
    workspace-integration:
      paths:
        - repo-a/src/**
rules: []
"#,
        )
        .expect("root config");
        for repo in ["repo-a", "repo-b"] {
            fs::write(
                root.join(format!("{repo}/.docpact/config.yaml")),
                r#"
version: 1
layout: repo
routing:
  intents:
    proof:
      paths:
        - docs/proof/**
rules: []
"#,
            )
            .expect("child config");
        }
        fs::write(root.join("repo-a/src/index.ts"), "export const a = 1;\n").expect("repo a file");
        fs::write(root.join("repo-b/src/index.ts"), "export const b = 1;\n").expect("repo b file");
        git(&root, &["add", "."]);

        let mut args = base_args(root.clone(), "");
        args.paths = None;
        args.intent = vec!["workspace-integration".into()];
        let report = execute_with_today(&args, "2026-04-22").expect("route should not fail");

        assert_eq!(report.summary.intent_input_count, 1);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.code == "no-rule-matches")
        );
    }

    #[test]
    fn route_child_duplicate_intent_requires_scope_unless_path_disambiguates() {
        let root = temp_dir("docpact-route-workspace-child-intent");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("repo-a/.docpact")).expect("repo a config dir");
        fs::create_dir_all(root.join("repo-b/.docpact")).expect("repo b config dir");
        fs::create_dir_all(root.join("repo-a/src")).expect("repo a src");
        fs::create_dir_all(root.join("repo-b/src")).expect("repo b src");

        fs::write(
            root.join(".docpact/config.yaml"),
            "version: 1\nlayout: workspace\nrules: []\n",
        )
        .expect("root config");
        for repo in ["repo-a", "repo-b"] {
            fs::write(
                root.join(format!("{repo}/.docpact/config.yaml")),
                r#"
version: 1
layout: repo
routing:
  intents:
    repo-docs:
      paths:
        - src/**
rules: []
"#,
            )
            .expect("child config");
            fs::write(
                root.join(format!("{repo}/src/index.ts")),
                "export const x = 1;\n",
            )
            .expect("tracked file");
        }
        git(&root, &["add", "."]);

        let mut ambiguous_args = base_args(root.clone(), "");
        ambiguous_args.paths = None;
        ambiguous_args.intent = vec!["repo-docs".into()];
        let error = execute_with_today(&ambiguous_args, "2026-04-22")
            .expect_err("duplicate child alias should be ambiguous without scope");
        assert!(
            error
                .to_string()
                .contains("routing intent alias `repo-docs` exists in multiple repo scopes")
        );
        assert!(error.to_string().contains("repo-a/.docpact/config.yaml"));
        assert!(error.to_string().contains("repo-b/.docpact/config.yaml"));

        let mut scoped_args = base_args(root.clone(), "repo-a/src/index.ts");
        scoped_args.intent = vec!["repo-docs".into()];
        let report =
            execute_with_today(&scoped_args, "2026-04-22").expect("path should disambiguate");
        assert_eq!(report.summary.input_path_count, 1);
        assert_eq!(report.summary.intent_input_count, 1);
    }

    #[test]
    fn route_rejects_unknown_intent_aliases() {
        let root = temp_dir("docpact-route-unknown-intent");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
repo:
  id: demo
rules: []
"#,
        )
        .expect("config should be written");
        git(&root, &["add", "."]);

        let mut args = base_args(root.clone(), "");
        args.paths = None;
        args.intent = vec!["missing".into()];
        let error = execute_with_today(&args, "2026-04-22").expect_err("route should fail");
        assert!(
            error
                .to_string()
                .contains("Unknown routing intent alias `missing`")
        );
        assert!(
            error
                .to_string()
                .contains("No routing intent aliases are configured")
        );
        assert!(
            error
                .to_string()
                .contains("docpact render --view routing-summary")
        );
    }

    #[test]
    fn route_unknown_intent_lists_available_aliases() {
        let root = temp_dir("docpact-route-unknown-intent-available");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
routing:
  intents:
    api:
      paths:
        - src/api/**
    auth:
      paths:
        - src/auth/**
repo:
  id: demo
rules: []
"#,
        )
        .expect("config should be written");
        git(&root, &["add", "."]);

        let mut args = base_args(root.clone(), "");
        args.paths = None;
        args.intent = vec!["missing".into()];
        let error = execute_with_today(&args, "2026-04-22").expect_err("route should fail");
        assert!(
            error
                .to_string()
                .contains("Unknown routing intent alias `missing`")
        );
        assert!(error.to_string().contains("Available aliases: api, auth"));
        assert!(
            error
                .to_string()
                .contains("docpact render --view routing-summary")
        );
    }

    #[test]
    fn route_returns_empty_recommendations_when_no_rules_match() {
        let root = temp_dir("docpact-route-empty");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src/auth")).expect("auth dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
repo:
  id: demo
rules:
  - id: auth-docs
    scope: repo
    repo: demo
    triggers:
      - path: src/auth/**
        kind: code
    requiredDocs:
      - path: docs/auth.md
        mode: review_or_update
    reason: Keep auth docs aligned.
"#,
        )
        .expect("config should be written");
        git(&root, &["add", "."]);

        let report = execute_with_today(
            &base_args(root.clone(), "src/payments/charge.ts"),
            "2026-04-22",
        )
        .expect("route report should execute");

        assert_eq!(report.summary.input_path_count, 1);
        assert_eq!(report.summary.matched_rule_count, 0);
        assert_eq!(report.summary.governed_doc_count, 0);
        assert_eq!(report.summary.advisory_doc_count, 0);
        assert!(report.governed_docs.is_empty());
        assert!(report.advisory_docs.is_empty());
        assert_eq!(report.warnings.len(), 2);
        assert_eq!(report.warnings[0].code, "no-rule-matches");
        assert_eq!(report.warnings[1].code, "no-route-recommendations");
        let rendered = render_text_report(&report, RouteDetail::Compact, None);
        assert!(rendered.contains("Warnings:"));
        assert!(rendered.contains("no-rule-matches for"));
    }

    #[test]
    fn route_warns_when_glob_or_module_matches_no_tracked_paths() {
        let root = temp_dir("docpact-route-empty-candidates");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src/auth")).expect("auth dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
repo:
  id: demo
rules: []
"#,
        )
        .expect("config should be written");
        fs::write(root.join("src/auth/login.ts"), "export const login = 1;\n")
            .expect("source file should be written");
        git(&root, &["add", "."]);

        let mut args = base_args(root.clone(), "src/payments/**");
        args.module = vec!["src/missing".into()];
        let report = execute_with_today(&args, "2026-04-22").expect("route report");

        assert_eq!(report.summary.input_path_count, 1);
        assert_eq!(report.summary.module_input_count, 1);
        assert_eq!(report.warnings.len(), 2);
        assert_eq!(report.warnings[0].code, "no-tracked-path-matches");
        assert_eq!(report.warnings[0].input, "src/payments/**");
        assert_eq!(report.warnings[1].code, "no-tracked-path-matches");
        assert_eq!(report.warnings[1].input, "module:src/missing");
    }

    #[test]
    fn route_ranking_prefers_stronger_modes_then_specificity() {
        let root = temp_dir("docpact-route-ranking");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src/payments/admin")).expect("payments dir");
        fs::create_dir_all(root.join("docs")).expect("docs dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
repo:
  id: demo
rules:
  - id: broad-review
    scope: repo
    repo: demo
    triggers:
      - path: src/payments/**
        kind: code
    requiredDocs:
      - path: docs/broad.md
        mode: review_or_update
    reason: broad
  - id: exact-body
    scope: repo
    repo: demo
    triggers:
      - path: src/payments/admin/panel.ts
        kind: code
    requiredDocs:
      - path: docs/exact.md
        mode: body_update_required
    reason: exact
  - id: exact-metadata
    scope: repo
    repo: demo
    triggers:
      - path: src/payments/admin/panel.ts
        kind: code
    requiredDocs:
      - path: docs/meta.md
        mode: metadata_refresh_required
    reason: metadata
"#,
        )
        .expect("config should be written");
        fs::write(
            root.join("src/payments/admin/panel.ts"),
            "export const panel = 1;\n",
        )
        .expect("source file should be written");
        fs::write(root.join("docs/broad.md"), "# Broad\n").expect("doc file");
        fs::write(root.join("docs/exact.md"), "# Exact\n").expect("doc file");
        fs::write(root.join("docs/meta.md"), "# Meta\n").expect("doc file");
        git(&root, &["add", "."]);

        let report = execute_with_today(
            &base_args(root.clone(), "src/payments/admin/panel.ts"),
            "2026-04-22",
        )
        .expect("route report should execute");

        let ordered_paths = report
            .governed_docs
            .iter()
            .map(|item| item.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ordered_paths,
            vec!["docs/exact.md", "docs/meta.md", "docs/broad.md"]
        );
        assert_eq!(report.governed_docs[0].priority, "high");
        assert_eq!(report.governed_docs[1].priority, "high");
        assert_eq!(report.governed_docs[2].priority, "medium");
    }

    #[test]
    fn route_ranking_prefers_ownership_context_before_path_tie_break() {
        let root = temp_dir("docpact-route-ownership-ranking");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("repo-a/src")).expect("repo-a dir");
        fs::create_dir_all(root.join("repo-b/src")).expect("repo-b dir");
        fs::create_dir_all(root.join("docs")).expect("docs dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
catalog:
  repos:
    - id: repo-a
      path: repo-a
ownership:
  domains:
    - id: owned-domain
      paths:
        include:
          - repo-a/src/**
      ownerRepo: repo-a
repo:
  id: workspace
rules:
  - id: owned-doc
    scope: repo
    repo: workspace
    triggers:
      - path: repo-a/src/**
        kind: code
    requiredDocs:
      - path: docs/z-owned.md
        mode: review_or_update
    reason: owned
  - id: unowned-doc
    scope: repo
    repo: workspace
    triggers:
      - path: repo-b/src/**
        kind: code
    requiredDocs:
      - path: docs/a-unowned.md
        mode: review_or_update
    reason: unowned
"#,
        )
        .expect("config should be written");
        fs::write(root.join("repo-a/src/one.ts"), "export const one = 1;\n").expect("repo-a file");
        fs::write(root.join("repo-b/src/two.ts"), "export const two = 1;\n").expect("repo-b file");
        fs::write(root.join("docs/z-owned.md"), "# Owned\n").expect("owned doc");
        fs::write(root.join("docs/a-unowned.md"), "# Unowned\n").expect("unowned doc");
        git(&root, &["add", "."]);

        let report = execute_with_today(
            &base_args(root.clone(), "repo-a/src/**,repo-b/src/**"),
            "2026-04-22",
        )
        .expect("route report should execute");

        let ordered_paths = report
            .governed_docs
            .iter()
            .map(|item| item.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ordered_paths, vec!["docs/z-owned.md", "docs/a-unowned.md"]);
        assert!(report.governed_docs[0].ownership_context.is_some());
        assert!(report.governed_docs[1].ownership_context.is_none());
    }

    #[test]
    fn route_full_detail_exposes_sources_and_full_text_explanations() {
        let root = temp_dir("docpact-route-full");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src/api")).expect("api dir");
        fs::create_dir_all(root.join("docs")).expect("docs dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
repo:
  id: demo
rules:
  - id: api-docs
    scope: repo
    repo: demo
    triggers:
      - path: src/api/**
        kind: code
    requiredDocs:
      - path: docs/api.md
        mode: review_or_update
    reason: api
"#,
        )
        .expect("config should be written");
        fs::write(root.join("src/api/client.ts"), "export const client = 1;\n")
            .expect("source file should be written");
        fs::write(root.join("docs/api.md"), "# API\n").expect("doc file");
        git(&root, &["add", "."]);

        let mut args = base_args(root.clone(), "src/api/client.ts");
        args.detail = RouteDetail::Full;
        let report = execute_with_today(&args, "2026-04-22").expect("route report should execute");
        let recommendation = &report.governed_docs[0];
        assert_eq!(recommendation.config_sources, vec![".docpact/config.yaml"]);
        assert_eq!(recommendation.rule_sources, vec![".docpact/config.yaml"]);

        let rendered = render_text_report(&report, RouteDetail::Full, Some(1));
        assert!(rendered.contains("priority:"));
        assert!(rendered.contains("freshness:"));
        assert!(rendered.contains("Governed docs:"));
        assert!(rendered.contains("Advisory docs:"));
        assert!(rendered.contains("triggers: src/api/**"));
        assert!(rendered.contains("score: mode="));
        assert!(rendered.contains("freshness_penalty="));
        assert!(rendered.contains("config sources: .docpact/config.yaml"));
        assert!(rendered.contains("rule sources: .docpact/config.yaml"));
    }

    #[test]
    fn route_text_limit_only_affects_rendered_rows() {
        let root = temp_dir("docpact-route-limit");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src")).expect("src dir");
        fs::create_dir_all(root.join("docs")).expect("docs dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
repo:
  id: demo
rules:
  - id: one
    scope: repo
    repo: demo
    triggers:
      - path: src/file-a.ts
        kind: code
    requiredDocs:
      - path: docs/a.md
        mode: review_or_update
    reason: a
  - id: two
    scope: repo
    repo: demo
    triggers:
      - path: src/file-b.ts
        kind: code
    requiredDocs:
      - path: docs/b.md
        mode: review_or_update
    reason: b
"#,
        )
        .expect("config should be written");
        fs::write(root.join("src/file-a.ts"), "export const a = 1;\n").expect("source file");
        fs::write(root.join("src/file-b.ts"), "export const b = 1;\n").expect("source file");
        fs::write(root.join("docs/a.md"), "# A\n").expect("doc file");
        fs::write(root.join("docs/b.md"), "# B\n").expect("doc file");
        git(&root, &["add", "."]);

        let report = execute_with_today(
            &base_args(root.clone(), "src/file-a.ts,src/file-b.ts"),
            "2026-04-22",
        )
        .expect("route report should execute");
        let rendered = render_text_report(&report, RouteDetail::Compact, Some(1));
        assert!(rendered.contains("Governed docs (showing 1 of 2)"));
        assert!(
            rendered.contains("review or update docs/a.md")
                || rendered.contains("review or update docs/b.md")
        );
    }

    #[test]
    fn route_text_output_splits_governed_and_advisory_sections() {
        let root = temp_dir("docpact-route-split-text");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src/api")).expect("api dir");
        fs::create_dir_all(root.join("docs")).expect("docs dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
catalog:
  repos:
    - id: demo
      path: .
      entryDoc: AGENTS.md
repo:
  id: demo
rules:
  - id: api-docs
    scope: repo
    repo: demo
    triggers:
      - path: src/api/**
        kind: code
    requiredDocs:
      - path: docs/api.md
        mode: review_or_update
    reason: api
"#,
        )
        .expect("config should be written");
        fs::write(root.join("src/api/client.ts"), "export const client = 1;\n")
            .expect("source file should be written");
        git(&root, &["add", "."]);

        let report =
            execute_with_today(&base_args(root.clone(), "src/api/client.ts"), "2026-04-22")
                .expect("route report should execute");
        let rendered = render_text_report(&report, RouteDetail::Compact, None);

        assert!(rendered.contains("governed_docs=1"));
        assert!(rendered.contains("advisory_docs=1"));
        assert!(rendered.contains("Governed docs:"));
        assert!(rendered.contains("Advisory docs:"));
        assert!(rendered.contains("review or update docs/api.md"));
        assert!(rendered.contains("read first AGENTS.md"));
    }

    #[test]
    fn route_demotes_stale_docs_and_surfaces_invalid_review_references() {
        let root = temp_dir("docpact-route-freshness");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src/api")).expect("api dir");
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
  id: demo
rules:
  - id: api-stale
    scope: repo
    repo: demo
    triggers:
      - path: src/api/client.ts
        kind: code
    requiredDocs:
      - path: docs/stale.md
        mode: body_update_required
      - path: docs/broken.md
        mode: body_update_required
    reason: stale
  - id: api-fresh
    scope: repo
    repo: demo
    triggers:
      - path: src/api/client.ts
        kind: code
    requiredDocs:
      - path: docs/fresh.md
        mode: body_update_required
    reason: fresh
"#,
        )
        .expect("config");
        fs::write(root.join("src/api/client.ts"), "export const client = 1;\n").expect("src");
        fs::write(
            root.join("docs/stale.md"),
            "---\nlastReviewedAt: 2025-01-01\nlastReviewedCommit: deadbeef\n---\n# Stale\n",
        )
        .expect("stale doc");
        fs::write(root.join("docs/fresh.md"), "# Fresh\n").expect("fresh doc");
        fs::write(root.join("docs/broken.md"), "# Broken\n").expect("broken doc");
        git(&root, &["add", "."]);
        let base = git_commit_all(&root, "base");

        fs::write(root.join("src/api/client.ts"), "export const client = 2;\n").expect("src");
        git(&root, &["add", "src/api/client.ts"]);
        let _head = git_commit_all(&root, "change");

        fs::write(
            root.join("docs/fresh.md"),
            format!("---\nlastReviewedAt: 2026-04-20\nlastReviewedCommit: {base}\n---\n# Fresh\n"),
        )
        .expect("fresh doc update");

        let report =
            execute_with_today(&base_args(root.clone(), "src/api/client.ts"), "2026-04-22")
                .expect("route report should execute");
        let ordered_paths = report
            .governed_docs
            .iter()
            .map(|item| item.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ordered_paths,
            vec!["docs/broken.md", "docs/fresh.md", "docs/stale.md"]
        );
        assert_eq!(report.summary.freshness_warning_count, 3);
        assert_eq!(report.summary.critical_freshness_count, 1);

        let broken = report
            .governed_docs
            .iter()
            .find(|item| item.path == "docs/broken.md")
            .expect("broken recommendation");
        assert_eq!(broken.freshness_level, "ok");
        assert!(
            broken
                .review_reference_problems
                .contains(&"missing-lastReviewedCommit".to_string())
        );
        assert!(broken.freshness_warning.is_some());

        let stale = report
            .governed_docs
            .iter()
            .find(|item| item.path == "docs/stale.md")
            .expect("stale recommendation");
        assert_eq!(stale.freshness_level, "critical");
        assert!(
            stale
                .review_reference_problems
                .contains(&"invalid-lastReviewedCommit".to_string())
        );
        assert!(stale.score_breakdown.freshness_penalty > 0);
        assert!(stale.freshness_warning.is_some());

        let fresh = report
            .governed_docs
            .iter()
            .find(|item| item.path == "docs/fresh.md")
            .expect("fresh recommendation");
        assert_eq!(fresh.freshness_level, "warn");
        assert_eq!(fresh.score_breakdown.freshness_penalty, 10);
        assert!(fresh.freshness_warning.is_some());
    }

    fn git_commit_all(root: &Path, message: &str) -> String {
        git(root, &["commit", "-m", message]);
        git_stdout(root, &["rev-parse", "HEAD"])
    }
}
