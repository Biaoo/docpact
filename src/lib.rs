//! Deterministic documentation governance for AI-assisted software teams.
//!
//! `docpact` is primarily a CLI. Repositories describe documentation
//! obligations in `.docpact/config.yaml`; commands such as `lint`, `route`,
//! `freshness`, and `render` evaluate those rules from explicit inputs.
//!
//! The crate intentionally keeps enforcement deterministic. It does not use an
//! LLM to decide whether documentation is stale, and it does not hide state in
//! background services or opaque caches. Automation can rely on repeatable
//! reports, diagnostics artifacts, and CI exit codes.
//!
//! Most users should start with the CLI. Library APIs are currently organized
//! around the same command implementations and report structures that power the
//! command-line interface.

/// Baseline support for suppressing existing diagnostics during adoption.
pub mod baseline;
/// Diff-driven linting for required documentation review and update checks.
pub mod check;
/// Command-line argument definitions and shared CLI enums.
pub mod cli;
/// Configuration loading, inheritance, validation, and path normalization.
pub mod config;
/// Coverage audits for governed paths and reachable documentation inventory.
pub mod coverage;
/// Report-backed diagnostics drill-down.
pub mod diagnostics;
/// Repository health checks for configuration and governance setup.
pub mod doctor;
/// Rule-match explanation for individual paths.
pub mod explain;
/// Freshness audits for governed documents and review references.
pub mod freshness;
/// Git helpers used by diff, history, and tracked-path commands.
pub mod git;
/// Rule listing and inspection output.
pub mod list_rules;
/// Review metadata parsing and update helpers.
pub mod metadata;
/// Read-only derived views over catalog, ownership, routing, and navigation.
pub mod render;
/// Text, JSON, and SARIF report builders.
pub mod reporters;
/// Review evidence commands for marking documents as reviewed.
pub mod review;
/// Reading-route recommendations for paths, modules, and controlled intents.
pub mod route;
/// Trigger matching and required-document rule primitives.
pub mod rules;
/// Configuration validation command implementation.
pub mod validate_config;
/// Waiver lifecycle support for explicit temporary exceptions.
pub mod waiver;

use miette::Result;

use crate::cli::{Cli, Commands};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppExit {
    Success,
    LintFailure,
}

pub fn run(cli: Cli) -> Result<AppExit> {
    match cli.command {
        Commands::Lint(args) => check::run(args),
        Commands::Baseline(args) => baseline::run(args),
        Commands::Waiver(args) => waiver::run(args),
        Commands::Route(args) => route::run(args),
        Commands::Render(args) => render::run(args),
        Commands::ListRules(args) => list_rules::run(args),
        Commands::Doctor(args) => doctor::run(args),
        Commands::Coverage(args) => coverage::run(args),
        Commands::Freshness(args) => freshness::run(args),
        Commands::Diagnostics(args) => diagnostics::run(args),
        Commands::Review(args) => review::run(args),
        Commands::Explain(args) => explain::run(args),
        Commands::ValidateConfig(args) => validate_config::run(args),
    }
}
