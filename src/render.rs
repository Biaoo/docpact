//! Implementation of read-only derived render views.
//!
//! Render exposes compact summaries over configured catalog, ownership,
//! routing, navigation, and workspace context without creating a new source of
//! truth.

use std::collections::BTreeSet;

use miette::{Result, bail};
use serde::Serialize;

use crate::AppExit;
use crate::cli::{
    RenderArgs, RenderOutputFormat, RenderView, RouteArgs, RouteDetail, RouteOutputFormat,
};
use crate::config::{
    analyze_ownership_paths, load_catalog_configs, load_ownership_configs, load_routing_configs,
    resolve_rule_path, root_dir_from_option,
};
use crate::git::get_tracked_paths;
use crate::route;
use crate::rules::matches_pattern;

pub const RENDER_SCHEMA_VERSION: &str = "docpact.render.v2";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "view", rename_all = "kebab-case")]
pub enum RenderReport {
    CatalogSummary(CatalogSummaryReport),
    OwnershipSummary(OwnershipSummaryReport),
    NavigationSummary(NavigationSummaryReport),
    RoutingSummary(RoutingSummaryReport),
    WorkspaceSummary(WorkspaceSummaryReport),
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CatalogSummaryReport {
    pub schema_version: String,
    pub tool_name: String,
    pub tool_version: String,
    pub command: String,
    pub summary: CatalogSummary,
    pub repos: Vec<CatalogRepoSummary>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CatalogSummary {
    pub repo_count: usize,
    pub entry_doc_count: usize,
    pub branch_policy_doc_count: usize,
    pub workflow_doc_count: usize,
    pub integration_doc_count: usize,
    pub workspace_integration_required_count: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CatalogRepoSummary {
    pub id: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_doc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_policy_doc: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflow_docs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub integration_docs: Vec<String>,
    pub workspace_integration_required: bool,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OwnershipSummaryReport {
    pub schema_version: String,
    pub tool_name: String,
    pub tool_version: String,
    pub command: String,
    pub summary: OwnershipSummary,
    pub domains: Vec<OwnershipDomainSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlaps: Vec<OwnershipOverlapSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<OwnershipConflictSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analysis_warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OwnershipSummary {
    pub domain_count: usize,
    pub owner_repo_count: usize,
    pub tracked_path_count: usize,
    pub analyzed_path_count: usize,
    pub overlap_count: usize,
    pub conflict_count: usize,
    pub analysis_available: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OwnershipDomainSummary {
    pub id: String,
    pub owner_repo: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub non_owner_repos: Vec<String>,
    pub include: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OwnershipOverlapSummary {
    pub path: String,
    pub owner_repo: String,
    pub domain_ids: Vec<String>,
    pub selected_domain_id: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OwnershipConflictSummary {
    pub path: String,
    pub owner_repos: Vec<String>,
    pub domain_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NavigationSummaryReport {
    pub schema_version: String,
    pub tool_name: String,
    pub tool_version: String,
    pub command: String,
    pub summary: NavigationSummary,
    #[serde(default)]
    pub warnings: Vec<route::RouteWarning>,
    pub governed_docs: Vec<NavigationGovernedDoc>,
    pub advisory_docs: Vec<NavigationAdvisoryDoc>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NavigationSummary {
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
pub struct NavigationGovernedDoc {
    pub path: String,
    pub priority: String,
    pub rule_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owner_repos: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repo_ids: Vec<String>,
    pub freshness_level: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NavigationAdvisoryDoc {
    pub path: String,
    pub repo_id: String,
    pub pointer_types: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_repo: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RoutingSummaryReport {
    pub schema_version: String,
    pub tool_name: String,
    pub tool_version: String,
    pub command: String,
    pub summary: RoutingSummary,
    pub intents: Vec<RoutingIntentSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analysis_warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RoutingSummary {
    pub config_count: usize,
    pub intent_count: usize,
    pub duplicate_alias_count: usize,
    pub tracked_path_count: usize,
    pub analysis_available: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RoutingIntentSummary {
    pub alias: String,
    pub config_source: String,
    pub source: String,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<String>,
    pub base_dir: String,
    pub resolution: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub override_mode: Option<String>,
    pub configured_paths: Vec<String>,
    pub resolved_patterns: Vec<String>,
    pub matched_tracked_path_count: usize,
    pub duplicate: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorkspaceSummaryReport {
    pub schema_version: String,
    pub tool_name: String,
    pub tool_version: String,
    pub command: String,
    pub summary: WorkspaceSummary,
    pub repos: Vec<WorkspaceRepoSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analysis_warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorkspaceSummary {
    pub catalog_repo_count: usize,
    pub ownership_domain_count: usize,
    pub ownership_overlap_count: usize,
    pub ownership_conflict_count: usize,
    pub tracked_path_count: usize,
    pub advisory_pointer_count: usize,
    pub workspace_integration_required_count: usize,
    pub analysis_available: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorkspaceRepoSummary {
    pub id: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_repo: Option<String>,
    pub entry_doc_present: bool,
    pub branch_policy_doc_present: bool,
    pub workflow_doc_count: usize,
    pub integration_doc_count: usize,
    pub workspace_integration_required: bool,
}

pub fn run(args: RenderArgs) -> Result<AppExit> {
    let report = execute(&args)?;
    emit_report(&report, args.format, args.limit);
    Ok(AppExit::Success)
}

pub fn execute(args: &RenderArgs) -> Result<RenderReport> {
    match args.view {
        RenderView::CatalogSummary => {
            reject_navigation_inputs(args)?;
            execute_catalog_summary(args)
        }
        RenderView::OwnershipSummary => {
            reject_navigation_inputs(args)?;
            execute_ownership_summary(args)
        }
        RenderView::NavigationSummary => execute_navigation_summary(args),
        RenderView::RoutingSummary => {
            reject_navigation_inputs(args)?;
            execute_routing_summary(args)
        }
        RenderView::WorkspaceSummary => {
            reject_navigation_inputs(args)?;
            execute_workspace_summary(args)
        }
    }
}

fn reject_navigation_inputs(args: &RenderArgs) -> Result<()> {
    if args.paths.is_some() || !args.module.is_empty() || !args.intent.is_empty() {
        bail!(
            "`--paths`, `--module`, and `--intent` are only valid with `--view navigation-summary`."
        );
    }
    Ok(())
}

fn execute_catalog_summary(args: &RenderArgs) -> Result<RenderReport> {
    let root_dir = root_dir_from_option(args.root.as_deref())?;
    let loaded = load_catalog_configs(&root_dir, args.config.as_deref())?;
    let repos = loaded
        .iter()
        .flat_map(|catalog| {
            catalog.catalog.repos.iter().map(|repo| CatalogRepoSummary {
                id: repo.id.clone(),
                path: repo.path.clone(),
                canonical_repo: repo.canonical_repo.clone(),
                entry_doc: repo
                    .entry_doc
                    .as_ref()
                    .map(|path| resolve_catalog_doc_pointer(&catalog.base_dir, &repo.path, path)),
                branch_policy_doc: repo
                    .branch_policy_doc
                    .as_ref()
                    .map(|path| resolve_catalog_doc_pointer(&catalog.base_dir, &repo.path, path)),
                workflow_docs: repo
                    .workflow_docs
                    .iter()
                    .map(|path| resolve_catalog_doc_pointer(&catalog.base_dir, &repo.path, path))
                    .collect(),
                integration_docs: repo
                    .integration_docs
                    .iter()
                    .map(|path| resolve_catalog_doc_pointer(&catalog.base_dir, &repo.path, path))
                    .collect(),
                workspace_integration_required: repo.workspace_integration_required,
                source: catalog.source.clone(),
            })
        })
        .collect::<Vec<_>>();

    let summary = CatalogSummary {
        repo_count: repos.len(),
        entry_doc_count: repos.iter().filter(|repo| repo.entry_doc.is_some()).count(),
        branch_policy_doc_count: repos
            .iter()
            .filter(|repo| repo.branch_policy_doc.is_some())
            .count(),
        workflow_doc_count: repos.iter().map(|repo| repo.workflow_docs.len()).sum(),
        integration_doc_count: repos.iter().map(|repo| repo.integration_docs.len()).sum(),
        workspace_integration_required_count: repos
            .iter()
            .filter(|repo| repo.workspace_integration_required)
            .count(),
    };

    Ok(RenderReport::CatalogSummary(CatalogSummaryReport {
        schema_version: RENDER_SCHEMA_VERSION.into(),
        tool_name: env!("CARGO_PKG_NAME").into(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        command: "render".into(),
        summary,
        repos,
    }))
}

fn execute_ownership_summary(args: &RenderArgs) -> Result<RenderReport> {
    let root_dir = root_dir_from_option(args.root.as_deref())?;
    let catalog_configs = load_catalog_configs(&root_dir, args.config.as_deref())?;
    let ownership_configs = load_ownership_configs(&root_dir, args.config.as_deref())?;
    let tracked_paths = get_tracked_paths(&root_dir);
    let owner_repo_count = ownership_configs
        .iter()
        .flat_map(|config| {
            config
                .ownership
                .domains
                .iter()
                .map(|domain| domain.owner_repo.clone())
        })
        .collect::<BTreeSet<_>>()
        .len();

    let domains = ownership_configs
        .iter()
        .flat_map(|ownership| {
            ownership
                .ownership
                .domains
                .iter()
                .map(|domain| OwnershipDomainSummary {
                    id: domain.id.clone(),
                    owner_repo: domain.owner_repo.clone(),
                    non_owner_repos: domain.non_owner_repos.clone(),
                    include: domain
                        .paths
                        .include
                        .iter()
                        .map(|pattern| resolve_rule_path(&ownership.base_dir, pattern))
                        .collect(),
                    exclude: domain
                        .paths
                        .exclude
                        .iter()
                        .map(|pattern| resolve_rule_path(&ownership.base_dir, pattern))
                        .collect(),
                    source: ownership.source.clone(),
                })
        })
        .collect::<Vec<_>>();

    let (
        tracked_path_count,
        analyzed_path_count,
        overlaps,
        conflicts,
        analysis_warning,
        analysis_available,
    ) = match tracked_paths {
        Ok(paths) => {
            let analysis = analyze_ownership_paths(&paths, &ownership_configs);
            let overlaps = analysis
                .overlaps
                .into_iter()
                .map(|item| OwnershipOverlapSummary {
                    path: item.path,
                    owner_repo: item.owner_repo,
                    domain_ids: item.domain_ids,
                    selected_domain_id: item.selected_domain_id,
                })
                .collect::<Vec<_>>();
            let conflicts = analysis
                .conflicts
                .into_iter()
                .map(|item| OwnershipConflictSummary {
                    path: item.path,
                    owner_repos: item.owner_repos,
                    domain_ids: item.domain_ids,
                })
                .collect::<Vec<_>>();
            (
                paths.len(),
                analysis.paths.len(),
                overlaps,
                conflicts,
                None,
                true,
            )
        }
        Err(error) => (
            0,
            0,
            Vec::new(),
            Vec::new(),
            Some(format!(
                "Ownership analysis could not read tracked files from git: {error}"
            )),
            false,
        ),
    };

    let _ = catalog_configs; // keep symmetry with ownership strict reference model

    Ok(RenderReport::OwnershipSummary(OwnershipSummaryReport {
        schema_version: RENDER_SCHEMA_VERSION.into(),
        tool_name: env!("CARGO_PKG_NAME").into(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        command: "render".into(),
        summary: OwnershipSummary {
            domain_count: domains.len(),
            owner_repo_count,
            tracked_path_count,
            analyzed_path_count,
            overlap_count: overlaps.len(),
            conflict_count: conflicts.len(),
            analysis_available,
        },
        domains,
        overlaps,
        conflicts,
        analysis_warning,
    }))
}

fn execute_navigation_summary(args: &RenderArgs) -> Result<RenderReport> {
    let route_args = RouteArgs {
        root: args.root.clone(),
        config: args.config.clone(),
        paths: args.paths.clone(),
        module: args.module.clone(),
        intent: args.intent.clone(),
        detail: RouteDetail::Compact,
        limit: None,
        format: RouteOutputFormat::Json,
    };
    let report = route::execute(&route_args)?;
    Ok(RenderReport::NavigationSummary(NavigationSummaryReport {
        schema_version: RENDER_SCHEMA_VERSION.into(),
        tool_name: env!("CARGO_PKG_NAME").into(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        command: "render".into(),
        summary: NavigationSummary {
            input_path_count: report.summary.input_path_count,
            module_input_count: report.summary.module_input_count,
            intent_input_count: report.summary.intent_input_count,
            matched_rule_count: report.summary.matched_rule_count,
            governed_doc_count: report.summary.governed_doc_count,
            advisory_doc_count: report.summary.advisory_doc_count,
            freshness_warning_count: report.summary.freshness_warning_count,
            critical_freshness_count: report.summary.critical_freshness_count,
        },
        warnings: report.warnings,
        governed_docs: report
            .governed_docs
            .into_iter()
            .map(|doc| NavigationGovernedDoc {
                path: doc.path,
                priority: doc.priority,
                rule_ids: doc.match_reason.rule_ids,
                owner_repos: doc
                    .ownership_context
                    .map(|context| context.owner_repos)
                    .unwrap_or_default(),
                repo_ids: doc
                    .repo_context
                    .map(|context| context.repo_ids)
                    .unwrap_or_default(),
                freshness_level: doc.freshness_level,
            })
            .collect(),
        advisory_docs: report
            .advisory_docs
            .into_iter()
            .map(|doc| NavigationAdvisoryDoc {
                path: doc.path,
                repo_id: doc.repo_id,
                pointer_types: doc.pointer_types,
                canonical_repo: doc.canonical_repo,
            })
            .collect(),
    }))
}

fn execute_routing_summary(args: &RenderArgs) -> Result<RenderReport> {
    let root_dir = root_dir_from_option(args.root.as_deref())?;
    let routing_configs = load_routing_configs(&root_dir, args.config.as_deref())?;
    let tracked_paths = get_tracked_paths(&root_dir);
    let alias_counts = routing_configs
        .iter()
        .flat_map(|config| config.routing.intents.keys().cloned())
        .fold(
            std::collections::BTreeMap::<String, usize>::new(),
            |mut counts, alias| {
                *counts.entry(alias).or_default() += 1;
                counts
            },
        );

    let (tracked_path_count, analysis_warning, analysis_available, tracked_paths) =
        match tracked_paths {
            Ok(paths) => (paths.len(), None, true, paths),
            Err(error) => (
                0,
                Some(format!(
                    "Routing summary could not read tracked files from git: {error}"
                )),
                false,
                Vec::new(),
            ),
        };

    let intents = routing_configs
        .iter()
        .flat_map(|config| {
            config.routing.intents.iter().map(|(alias, intent)| {
                let resolved_patterns = intent
                    .paths
                    .iter()
                    .map(|pattern| resolve_rule_path(&config.base_dir, pattern))
                    .collect::<Vec<_>>();
                let matched_tracked_path_count = if analysis_available {
                    tracked_paths
                        .iter()
                        .filter(|tracked| {
                            resolved_patterns
                                .iter()
                                .any(|pattern| matches_pattern(tracked, pattern))
                        })
                        .count()
                } else {
                    0
                };

                RoutingIntentSummary {
                    alias: alias.clone(),
                    config_source: config.source.clone(),
                    source: config.source.clone(),
                    scope: config.scope_kind.as_str().into(),
                    repo_id: config.repo_id.clone(),
                    base_dir: if config.base_dir.is_empty() {
                        ".".into()
                    } else {
                        config.base_dir.clone()
                    },
                    resolution: config.resolution.origin_kind.as_str().into(),
                    workspace_profile: config.resolution.workspace_profile.clone(),
                    override_mode: config.resolution.mode.clone(),
                    configured_paths: intent.paths.clone(),
                    resolved_patterns,
                    matched_tracked_path_count,
                    duplicate: alias_counts.get(alias).copied().unwrap_or_default() > 1,
                }
            })
        })
        .collect::<Vec<_>>();

    Ok(RenderReport::RoutingSummary(RoutingSummaryReport {
        schema_version: RENDER_SCHEMA_VERSION.into(),
        tool_name: env!("CARGO_PKG_NAME").into(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        command: "render".into(),
        summary: RoutingSummary {
            config_count: routing_configs.len(),
            intent_count: intents.len(),
            duplicate_alias_count: alias_counts.values().filter(|count| **count > 1).count(),
            tracked_path_count,
            analysis_available,
        },
        intents,
        analysis_warning,
    }))
}

fn execute_workspace_summary(args: &RenderArgs) -> Result<RenderReport> {
    let root_dir = root_dir_from_option(args.root.as_deref())?;
    let catalog_configs = load_catalog_configs(&root_dir, args.config.as_deref())?;
    let ownership_configs = load_ownership_configs(&root_dir, args.config.as_deref())?;
    let tracked_paths = get_tracked_paths(&root_dir);

    let repos = catalog_configs
        .iter()
        .flat_map(|catalog| {
            catalog
                .catalog
                .repos
                .iter()
                .map(|repo| WorkspaceRepoSummary {
                    id: repo.id.clone(),
                    path: repo.path.clone(),
                    canonical_repo: repo.canonical_repo.clone(),
                    entry_doc_present: repo.entry_doc.is_some(),
                    branch_policy_doc_present: repo.branch_policy_doc.is_some(),
                    workflow_doc_count: repo.workflow_docs.len(),
                    integration_doc_count: repo.integration_docs.len(),
                    workspace_integration_required: repo.workspace_integration_required,
                })
        })
        .collect::<Vec<_>>();

    let advisory_pointer_count = catalog_configs
        .iter()
        .flat_map(|catalog| catalog.catalog.repos.iter())
        .map(|repo| {
            usize::from(repo.entry_doc.is_some())
                + usize::from(repo.branch_policy_doc.is_some())
                + repo.workflow_docs.len()
                + repo.integration_docs.len()
        })
        .sum();

    let (tracked_path_count, overlap_count, conflict_count, analysis_warning, analysis_available) =
        match tracked_paths {
            Ok(paths) => {
                let analysis = analyze_ownership_paths(&paths, &ownership_configs);
                (
                    paths.len(),
                    analysis.overlaps.len(),
                    analysis.conflicts.len(),
                    None,
                    true,
                )
            }
            Err(error) => (
                0,
                0,
                0,
                Some(format!(
                    "Workspace ownership analysis could not read tracked files from git: {error}"
                )),
                false,
            ),
        };

    Ok(RenderReport::WorkspaceSummary(WorkspaceSummaryReport {
        schema_version: RENDER_SCHEMA_VERSION.into(),
        tool_name: env!("CARGO_PKG_NAME").into(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        command: "render".into(),
        summary: WorkspaceSummary {
            catalog_repo_count: repos.len(),
            ownership_domain_count: ownership_configs
                .iter()
                .map(|config| config.ownership.domains.len())
                .sum(),
            ownership_overlap_count: overlap_count,
            ownership_conflict_count: conflict_count,
            tracked_path_count,
            advisory_pointer_count,
            workspace_integration_required_count: repos
                .iter()
                .filter(|repo| repo.workspace_integration_required)
                .count(),
            analysis_available,
        },
        repos,
        analysis_warning,
    }))
}

fn resolve_catalog_doc_pointer(base_dir: &str, repo_path: &str, pointer: &str) -> String {
    let repo_root = if repo_path == "." {
        base_dir.to_string()
    } else {
        resolve_rule_path(base_dir, repo_path)
    };

    if repo_root.is_empty() {
        pointer.to_string()
    } else {
        format!("{repo_root}/{pointer}")
    }
}

fn emit_report(report: &RenderReport, format: RenderOutputFormat, limit: Option<usize>) {
    match format {
        RenderOutputFormat::Text => print!("{}", render_text_report(report, limit)),
        RenderOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(report).expect("render report should serialize")
        ),
    }
}

fn render_text_report(report: &RenderReport, limit: Option<usize>) -> String {
    match report {
        RenderReport::CatalogSummary(report) => render_catalog_summary_text(report, limit),
        RenderReport::OwnershipSummary(report) => render_ownership_summary_text(report, limit),
        RenderReport::NavigationSummary(report) => render_navigation_summary_text(report, limit),
        RenderReport::RoutingSummary(report) => render_routing_summary_text(report, limit),
        RenderReport::WorkspaceSummary(report) => render_workspace_summary_text(report, limit),
    }
}

fn render_catalog_summary_text(report: &CatalogSummaryReport, limit: Option<usize>) -> String {
    let mut output = String::new();
    output.push_str("Docpact render catalog-summary:\n");
    output.push_str(&format!(
        "Summary: repos={} entry_docs={} branch_policy_docs={} workflow_docs={} integration_docs={} workspace_integration_required={}\n",
        report.summary.repo_count,
        report.summary.entry_doc_count,
        report.summary.branch_policy_doc_count,
        report.summary.workflow_doc_count,
        report.summary.integration_doc_count,
        report.summary.workspace_integration_required_count,
    ));
    render_limited_section(
        &mut output,
        "Repos",
        report.repos.len(),
        limit,
        report.repos.iter().map(|repo| {
            format!(
                "- id={} path={} canonical_repo={} entry_doc={} branch_policy_doc={} workflow_docs={} integration_docs={} workspace_integration_required={} source={}\n",
                repo.id,
                repo.path,
                repo.canonical_repo.as_deref().unwrap_or("-"),
                repo.entry_doc.as_deref().unwrap_or("-"),
                repo.branch_policy_doc.as_deref().unwrap_or("-"),
                repo.workflow_docs.len(),
                repo.integration_docs.len(),
                repo.workspace_integration_required,
                repo.source,
            )
        }),
    );
    output
}

fn render_ownership_summary_text(report: &OwnershipSummaryReport, limit: Option<usize>) -> String {
    let mut output = String::new();
    output.push_str("Docpact render ownership-summary:\n");
    output.push_str(&format!(
        "Summary: domains={} owner_repos={} tracked_paths={} analyzed_paths={} overlaps={} conflicts={} analysis_available={}\n",
        report.summary.domain_count,
        report.summary.owner_repo_count,
        report.summary.tracked_path_count,
        report.summary.analyzed_path_count,
        report.summary.overlap_count,
        report.summary.conflict_count,
        report.summary.analysis_available,
    ));
    if let Some(warning) = &report.analysis_warning {
        output.push_str(&format!("Analysis warning: {warning}\n"));
    }
    render_limited_section(
        &mut output,
        "Domains",
        report.domains.len(),
        limit,
        report.domains.iter().map(|domain| {
            format!(
                "- id={} owner={} non_owners={} include={} exclude={} source={}\n",
                domain.id,
                domain.owner_repo,
                join_or_dash(&domain.non_owner_repos),
                join_or_dash(&domain.include),
                join_or_dash(&domain.exclude),
                domain.source,
            )
        }),
    );
    if !report.overlaps.is_empty() {
        render_limited_section(
            &mut output,
            "Overlaps",
            report.overlaps.len(),
            limit,
            report.overlaps.iter().map(|overlap| {
                format!(
                    "- path={} owner={} domains={} selected={}\n",
                    overlap.path,
                    overlap.owner_repo,
                    overlap.domain_ids.join(","),
                    overlap.selected_domain_id,
                )
            }),
        );
    }
    if !report.conflicts.is_empty() {
        render_limited_section(
            &mut output,
            "Conflicts",
            report.conflicts.len(),
            limit,
            report.conflicts.iter().map(|conflict| {
                format!(
                    "- path={} owners={} domains={}\n",
                    conflict.path,
                    conflict.owner_repos.join(","),
                    conflict.domain_ids.join(","),
                )
            }),
        );
    }
    output
}

fn render_navigation_summary_text(
    report: &NavigationSummaryReport,
    limit: Option<usize>,
) -> String {
    let mut output = String::new();
    output.push_str("Docpact render navigation-summary:\n");
    output.push_str(&format!(
        "Summary: input_paths={} modules={} intents={} matched_rules={} governed_docs={} advisory_docs={} freshness_warnings={} critical_freshness={}\n",
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
    render_limited_section(
        &mut output,
        "Governed docs",
        report.governed_docs.len(),
        limit,
        report.governed_docs.iter().map(|doc| {
            format!(
                "- path={} priority={} rules={} owners={} repos={} freshness={}\n",
                doc.path,
                doc.priority,
                doc.rule_ids.join(","),
                join_or_dash(&doc.owner_repos),
                join_or_dash(&doc.repo_ids),
                doc.freshness_level,
            )
        }),
    );
    render_limited_section(
        &mut output,
        "Advisory docs",
        report.advisory_docs.len(),
        limit,
        report.advisory_docs.iter().map(|doc| {
            format!(
                "- path={} repo={} pointers={} canonical_repo={}\n",
                doc.path,
                doc.repo_id,
                doc.pointer_types.join(","),
                doc.canonical_repo.as_deref().unwrap_or("-"),
            )
        }),
    );
    output
}

fn render_routing_summary_text(report: &RoutingSummaryReport, limit: Option<usize>) -> String {
    let mut output = String::new();
    output.push_str("Docpact render routing-summary:\n");
    output.push_str(&format!(
        "Summary: configs={} intents={} duplicate_aliases={} tracked_paths={} analysis_available={}\n",
        report.summary.config_count,
        report.summary.intent_count,
        report.summary.duplicate_alias_count,
        report.summary.tracked_path_count,
        report.summary.analysis_available,
    ));
    if let Some(warning) = &report.analysis_warning {
        output.push_str(&format!("Analysis warning: {warning}\n"));
    }
    render_limited_section(
        &mut output,
        "Routing intents",
        report.intents.len(),
        limit,
        report.intents.iter().map(|intent| {
            format!(
                "- alias={} source={} scope={} repo_id={} base_dir={} resolution={} profile={} mode={} paths={} matched_tracked_paths={} duplicate={}\n",
                intent.alias,
                intent.source,
                intent.scope,
                intent.repo_id.as_deref().unwrap_or("-"),
                intent.base_dir,
                intent.resolution,
                intent.workspace_profile.as_deref().unwrap_or("-"),
                intent.override_mode.as_deref().unwrap_or("-"),
                join_or_dash(&intent.resolved_patterns),
                intent.matched_tracked_path_count,
                intent.duplicate,
            )
        }),
    );
    if report.intents.is_empty() {
        output.push_str("Next: define aliases under `routing.intents`, or use `docpact route --paths <csv>` / `--module <prefix>`.\n");
    } else {
        output.push_str(
            "Next: use `docpact route --intent <alias>` with one of the aliases above.\n",
        );
    }
    output
}

fn render_workspace_summary_text(report: &WorkspaceSummaryReport, limit: Option<usize>) -> String {
    let mut output = String::new();
    output.push_str("Docpact render workspace-summary:\n");
    output.push_str(&format!(
        "Summary: catalog_repos={} ownership_domains={} ownership_overlaps={} ownership_conflicts={} tracked_paths={} advisory_pointers={} workspace_integration_required={} analysis_available={}\n",
        report.summary.catalog_repo_count,
        report.summary.ownership_domain_count,
        report.summary.ownership_overlap_count,
        report.summary.ownership_conflict_count,
        report.summary.tracked_path_count,
        report.summary.advisory_pointer_count,
        report.summary.workspace_integration_required_count,
        report.summary.analysis_available,
    ));
    if let Some(warning) = &report.analysis_warning {
        output.push_str(&format!("Analysis warning: {warning}\n"));
    }
    render_limited_section(
        &mut output,
        "Repos",
        report.repos.len(),
        limit,
        report.repos.iter().map(|repo| {
            format!(
                "- id={} path={} canonical_repo={} entry_doc_present={} branch_policy_doc_present={} workflow_docs={} integration_docs={} workspace_integration_required={}\n",
                repo.id,
                repo.path,
                repo.canonical_repo.as_deref().unwrap_or("-"),
                repo.entry_doc_present,
                repo.branch_policy_doc_present,
                repo.workflow_doc_count,
                repo.integration_doc_count,
                repo.workspace_integration_required,
            )
        }),
    );
    output
}

fn render_limited_section<I>(
    output: &mut String,
    title: &str,
    total: usize,
    limit: Option<usize>,
    lines: I,
) where
    I: IntoIterator<Item = String>,
{
    let displayed = limit.map(|value| value.min(total)).unwrap_or(total);
    if displayed < total {
        output.push_str(&format!("{title} (showing {displayed} of {total}):\n"));
    } else {
        output.push_str(&format!("{title}:\n"));
    }

    if total == 0 {
        output.push_str("- none\n");
        return;
    }

    for line in lines.into_iter().take(displayed) {
        output.push_str(&line);
    }
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

    use crate::cli::{RenderOutputFormat, RenderView};

    use super::{
        CatalogRepoSummary, CatalogSummary, CatalogSummaryReport, RENDER_SCHEMA_VERSION,
        RenderArgs, RenderReport, execute, render_text_report,
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

    fn base_args(root: &Path, view: RenderView) -> RenderArgs {
        RenderArgs {
            root: Some(root.to_path_buf()),
            config: None,
            view,
            paths: None,
            module: Vec::new(),
            intent: Vec::new(),
            limit: None,
            format: RenderOutputFormat::Json,
        }
    }

    #[test]
    fn render_catalog_summary_reports_repo_doc_pointers() {
        let root = temp_dir("docpact-render-catalog");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");

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
      workspaceIntegrationRequired: true
repo:
  id: demo
rules: []
"#,
        )
        .expect("config");
        git(&root, &["add", "."]);

        let report = execute(&base_args(&root, RenderView::CatalogSummary)).expect("render report");
        let RenderReport::CatalogSummary(report) = report else {
            panic!("expected catalog summary");
        };
        assert_eq!(report.schema_version, RENDER_SCHEMA_VERSION);
        assert_eq!(report.summary.repo_count, 1);
        assert_eq!(report.summary.entry_doc_count, 1);
        assert_eq!(report.summary.branch_policy_doc_count, 1);
        assert_eq!(report.summary.workflow_doc_count, 1);
        assert_eq!(report.summary.integration_doc_count, 1);
        assert_eq!(report.summary.workspace_integration_required_count, 1);
        assert_eq!(
            report.repos[0].canonical_repo.as_deref(),
            Some("Biaoo/docpack")
        );
        assert_eq!(report.repos[0].entry_doc.as_deref(), Some("AGENTS.md"));
    }

    #[test]
    fn render_ownership_summary_reports_domains_and_analysis() {
        let root = temp_dir("docpact-render-ownership");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src/payments")).expect("payments dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
catalog:
  repos:
    - id: demo
      path: .
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
rules: []
"#,
        )
        .expect("config");
        fs::write(
            root.join("src/payments/charge.ts"),
            "export const charge = 1;\n",
        )
        .expect("src");
        git(&root, &["add", "."]);

        let report =
            execute(&base_args(&root, RenderView::OwnershipSummary)).expect("render report");
        let RenderReport::OwnershipSummary(report) = report else {
            panic!("expected ownership summary");
        };
        assert_eq!(report.summary.domain_count, 1);
        assert_eq!(report.summary.owner_repo_count, 1);
        assert!(report.summary.analysis_available);
        assert_eq!(report.summary.tracked_path_count, 2);
        assert_eq!(report.domains[0].id, "payments");
        assert_eq!(report.domains[0].non_owner_repos, vec!["app-shell"]);
    }

    #[test]
    fn render_navigation_summary_derives_from_route() {
        let root = temp_dir("docpact-render-navigation");
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
        .expect("config");
        fs::write(root.join("src/api/client.ts"), "export const client = 1;\n").expect("src");
        git(&root, &["add", "."]);

        let mut args = base_args(&root, RenderView::NavigationSummary);
        args.paths = Some("src/api/client.ts".into());
        let report = execute(&args).expect("render report");
        let RenderReport::NavigationSummary(report) = report else {
            panic!("expected navigation summary");
        };
        assert_eq!(report.summary.governed_doc_count, 1);
        assert_eq!(report.summary.advisory_doc_count, 1);
        assert_eq!(report.governed_docs[0].path, "docs/api.md");
        assert_eq!(report.advisory_docs[0].path, "AGENTS.md");
    }

    #[test]
    fn render_navigation_summary_preserves_route_warnings() {
        let root = temp_dir("docpact-render-navigation-warnings");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::write(
            root.join(".docpact/config.yaml"),
            "version: 1\nlayout: repo\nrepo:\n  id: demo\nrules: []\n",
        )
        .expect("config");
        git(&root, &["add", "."]);

        let mut args = base_args(&root, RenderView::NavigationSummary);
        args.module = vec!["src/missing".into()];
        let report = execute(&args).expect("render report");
        let RenderReport::NavigationSummary(report) = report else {
            panic!("expected navigation summary");
        };
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.warnings[0].code, "no-tracked-path-matches");

        let rendered = render_text_report(&RenderReport::NavigationSummary(report), None);
        assert!(rendered.contains("Warnings:"));
        assert!(rendered.contains("no-tracked-path-matches for"));
    }

    #[test]
    fn render_routing_summary_lists_effective_intents() {
        let root = temp_dir("docpact-render-routing");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("src/api")).expect("api dir");
        fs::create_dir_all(root.join("src/commands")).expect("commands dir");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
catalog:
  repos:
    - id: demo
      path: .
routing:
  intents:
    api:
      paths:
        - src/api/**
        - src/commands/**
repo:
  id: demo
rules: []
"#,
        )
        .expect("config");
        fs::write(root.join("src/api/client.ts"), "export const client = 1;\n").expect("src");
        fs::write(
            root.join("src/commands/sync.ts"),
            "export const sync = 1;\n",
        )
        .expect("src");
        git(&root, &["add", "."]);

        let report = execute(&base_args(&root, RenderView::RoutingSummary)).expect("render report");
        let RenderReport::RoutingSummary(report) = report else {
            panic!("expected routing summary");
        };
        assert_eq!(report.schema_version, RENDER_SCHEMA_VERSION);
        assert_eq!(report.summary.config_count, 1);
        assert_eq!(report.summary.intent_count, 1);
        assert_eq!(report.summary.duplicate_alias_count, 0);
        assert_eq!(report.summary.tracked_path_count, 3);
        assert!(report.summary.analysis_available);
        assert_eq!(report.intents[0].alias, "api");
        assert_eq!(report.intents[0].config_source, ".docpact/config.yaml");
        assert_eq!(report.intents[0].source, ".docpact/config.yaml");
        assert_eq!(report.intents[0].scope, "repo-local");
        assert_eq!(report.intents[0].repo_id.as_deref(), Some("demo"));
        assert_eq!(report.intents[0].base_dir, ".");
        assert_eq!(report.intents[0].resolution, "local");
        assert_eq!(
            report.intents[0].resolved_patterns,
            vec!["src/api/**".to_string(), "src/commands/**".to_string()]
        );
        assert_eq!(report.intents[0].matched_tracked_path_count, 2);
        assert!(!report.intents[0].duplicate);

        let rendered = render_text_report(&RenderReport::RoutingSummary(report), None);
        assert!(rendered.contains("Docpact render routing-summary:"));
        assert!(rendered.contains("alias=api"));
        assert!(rendered.contains("scope=repo-local"));
        assert!(rendered.contains("repo_id=demo"));
        assert!(rendered.contains("Next: use `docpact route --intent <alias>`"));
    }

    #[test]
    fn render_workspace_summary_reports_repo_and_analysis_counts() {
        let root = temp_dir("docpact-render-workspace");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::create_dir_all(root.join("repo-a/src")).expect("repo a");
        fs::create_dir_all(root.join("repo-b/src")).expect("repo b");

        fs::write(
            root.join(".docpact/config.yaml"),
            r#"
version: 1
layout: repo
catalog:
  repos:
    - id: repo-a
      path: repo-a
      entryDoc: AGENTS.md
    - id: repo-b
      path: repo-b
      workflowDocs:
        - docs/workflow.md
ownership:
  domains:
    - id: repo-a-domain
      paths:
        include:
          - repo-a/src/**
      ownerRepo: repo-a
    - id: repo-b-domain
      paths:
        include:
          - repo-b/src/**
      ownerRepo: repo-b
repo:
  id: workspace
rules: []
"#,
        )
        .expect("config");
        fs::write(root.join("repo-a/src/a.ts"), "export const a = 1;\n").expect("src");
        fs::write(root.join("repo-b/src/b.ts"), "export const b = 1;\n").expect("src");
        git(&root, &["add", "."]);

        let report =
            execute(&base_args(&root, RenderView::WorkspaceSummary)).expect("render report");
        let RenderReport::WorkspaceSummary(report) = report else {
            panic!("expected workspace summary");
        };
        assert_eq!(report.summary.catalog_repo_count, 2);
        assert_eq!(report.summary.ownership_domain_count, 2);
        assert_eq!(report.summary.advisory_pointer_count, 2);
        assert!(report.summary.analysis_available);
        assert_eq!(report.repos.len(), 2);
    }

    #[test]
    fn render_navigation_summary_rejects_missing_inputs_for_navigation_view() {
        let root = temp_dir("docpact-render-navigation-empty");
        init_git_repo(&root);
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::write(
            root.join(".docpact/config.yaml"),
            "version: 1\nlayout: repo\nrepo:\n  id: demo\nrules: []\n",
        )
        .expect("config");
        git(&root, &["add", "."]);

        let error =
            execute(&base_args(&root, RenderView::NavigationSummary)).expect_err("should fail");
        assert!(
            error
                .to_string()
                .contains("Pass at least one non-empty route input")
        );
    }

    #[test]
    fn render_non_navigation_views_reject_navigation_inputs() {
        let root = temp_dir("docpact-render-invalid-inputs");
        let mut args = base_args(&root, RenderView::CatalogSummary);
        args.paths = Some("src/api/client.ts".into());

        let error = execute(&args).expect_err("should fail");
        assert!(
            error
                .to_string()
                .contains("only valid with `--view navigation-summary`")
        );
    }

    #[test]
    fn render_text_output_is_short_by_default() {
        let report = RenderReport::CatalogSummary(CatalogSummaryReport {
            schema_version: RENDER_SCHEMA_VERSION.into(),
            tool_name: env!("CARGO_PKG_NAME").into(),
            tool_version: env!("CARGO_PKG_VERSION").into(),
            command: "render".into(),
            summary: CatalogSummary {
                repo_count: 2,
                entry_doc_count: 1,
                branch_policy_doc_count: 1,
                workflow_doc_count: 2,
                integration_doc_count: 1,
                workspace_integration_required_count: 0,
            },
            repos: vec![
                CatalogRepoSummary {
                    id: "a".into(),
                    path: ".".into(),
                    canonical_repo: None,
                    entry_doc: Some("AGENTS.md".into()),
                    branch_policy_doc: None,
                    workflow_docs: vec![],
                    integration_docs: vec![],
                    workspace_integration_required: false,
                    source: ".docpact/config.yaml".into(),
                },
                CatalogRepoSummary {
                    id: "b".into(),
                    path: "subrepo".into(),
                    canonical_repo: None,
                    entry_doc: None,
                    branch_policy_doc: Some("docs/branch.md".into()),
                    workflow_docs: vec!["docs/workflow.md".into()],
                    integration_docs: vec![],
                    workspace_integration_required: false,
                    source: ".docpact/config.yaml".into(),
                },
            ],
        });

        let rendered = render_text_report(&report, Some(1));
        assert!(rendered.contains("Repos (showing 1 of 2):"));
    }
}
