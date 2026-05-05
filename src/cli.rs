//! CLI argument definitions for the `docpact` binary.
//!
//! This module maps command-line flags into typed command structs. Command
//! implementations live in their corresponding modules.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "docpact",
    version,
    about = "Diff-driven documentation drift gate for AI-assisted teams."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    Lint(LintArgs),
    Baseline(BaselineArgs),
    Waiver(WaiverArgs),
    Route(RouteArgs),
    Render(RenderArgs),
    ListRules(ListRulesArgs),
    Doctor(DoctorArgs),
    Coverage(CoverageArgs),
    Freshness(FreshnessArgs),
    Diagnostics(DiagnosticsArgs),
    Review(ReviewArgs),
    Explain(ExplainArgs),
    ValidateConfig(ValidateConfigArgs),
}

#[derive(Debug, Clone, Args)]
#[command(after_help = "Examples:
  docpact lint --root . --files src/api/client.ts --format json
  docpact lint --root . --staged --mode enforce
  docpact lint --root . --merge-base main --output .docpact/runs/latest.json

Choose exactly one diff source: --files, --staged, --worktree, --merge-base, or --base with --head.")]
pub struct LintArgs {
    /// Repository root. Defaults to the current working directory.
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Explicit config file. Defaults to .docpact/config.yaml under --root.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Base commit for explicit base/head diff mode.
    #[arg(long)]
    pub base: Option<String>,
    /// Head commit for explicit base/head diff mode.
    #[arg(long)]
    pub head: Option<String>,
    /// Comma-separated changed paths to inspect.
    #[arg(long)]
    pub files: Option<String>,
    /// Inspect staged git changes.
    #[arg(long, default_value_t = false)]
    pub staged: bool,
    /// Inspect unstaged worktree changes.
    #[arg(long, default_value_t = false)]
    pub worktree: bool,
    /// Diff from merge-base between HEAD and the given ref.
    #[arg(long = "merge-base")]
    pub merge_base: Option<String>,
    /// warn reports findings with exit 0; enforce returns a failing exit for active findings.
    #[arg(long, value_enum, default_value_t = LintMode::Warn)]
    pub mode: LintMode,
    /// Output format. JSON stdout is intended for automation.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
    /// Amount of diagnostic detail to render in the paged stdout view.
    #[arg(long, value_enum, default_value_t = DiagnosticDetail::Compact)]
    pub detail: DiagnosticDetail,
    /// 1-based diagnostics page for stdout report rendering.
    #[arg(long, default_value_t = 1, value_parser = parse_positive_usize)]
    pub diagnostics_page: usize,
    /// Number of diagnostics to render per stdout page.
    #[arg(long, default_value_t = 5, value_parser = parse_positive_usize)]
    pub diagnostics_page_size: usize,
    /// Return a failing exit when changed paths are outside configured coverage.
    #[arg(long, default_value_t = false)]
    pub fail_on_uncovered_change: bool,
    /// Return a failing exit when matched governed docs are critically stale.
    #[arg(long, default_value_t = false)]
    pub fail_on_stale_docs: bool,
    /// Baseline file used to suppress already-accepted diagnostics.
    #[arg(long)]
    pub baseline: Option<PathBuf>,
    /// Waiver file used to suppress approved diagnostics.
    #[arg(long)]
    pub waivers: Option<PathBuf>,
    /// Full diagnostics artifact path. The stdout report remains paged.
    #[arg(long)]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct BaselineArgs {
    #[command(subcommand)]
    pub command: BaselineCommands,
}

#[derive(Debug, Clone, Subcommand)]
pub enum BaselineCommands {
    Create(BaselineCreateArgs),
}

#[derive(Debug, Clone, Args)]
#[command(after_help = "Example:
  docpact baseline create --report .docpact/runs/latest.json --output .docpact/baseline.json

Creates a baseline from a full diagnostics artifact, not from the paged lint stdout report.")]
pub struct BaselineCreateArgs {
    /// Full diagnostics artifact created by `docpact lint --output`.
    #[arg(long)]
    pub report: PathBuf,
    /// Baseline JSON file to write.
    #[arg(long)]
    pub output: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct WaiverArgs {
    #[command(subcommand)]
    pub command: WaiverCommands,
}

#[derive(Debug, Clone, Subcommand)]
pub enum WaiverCommands {
    Add(WaiverAddArgs),
}

#[derive(Debug, Clone, Args)]
#[command(after_help = "Example:
  docpact waiver add --report .docpact/runs/latest.json --id d001 --reason \"legacy migration\" --owner docs-team --expires-at 2026-05-01 --waivers .docpact/waivers.yaml

Use diagnostics ids from `docpact lint --output` and optionally narrow the waiver with --scope-rule-id or --scope-path.")]
pub struct WaiverAddArgs {
    /// Repository root. Defaults to the current working directory.
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Full diagnostics artifact containing the diagnostic id.
    #[arg(long)]
    pub report: PathBuf,
    /// Diagnostic id to waive, for example d001.
    #[arg(long)]
    pub id: String,
    /// Human-readable waiver reason.
    #[arg(long)]
    pub reason: String,
    /// Owner responsible for the waiver.
    #[arg(long)]
    pub owner: String,
    /// Expiration date in YYYY-MM-DD format.
    #[arg(long = "expires-at", value_parser = parse_iso_date)]
    pub expires_at: String,
    /// Optional rule id scope. May be repeated.
    #[arg(long = "scope-rule-id")]
    pub scope_rule_ids: Vec<String>,
    /// Optional path scope. May be repeated.
    #[arg(long = "scope-path")]
    pub scope_paths: Vec<String>,
    /// Waiver YAML file to update.
    #[arg(long)]
    pub waivers: PathBuf,
    /// Output format.
    #[arg(long, value_enum, default_value_t = WaiverOutputFormat::Text)]
    pub format: WaiverOutputFormat,
}

#[derive(Debug, Clone, Args)]
#[command(after_help = "Example:
  docpact coverage --root . --format json

Reports effective coverage include/exclude patterns and uncovered tracked-path candidates.")]
pub struct CoverageArgs {
    /// Repository root. Defaults to the current working directory.
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Explicit config file. Defaults to .docpact/config.yaml under --root.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = CoverageOutputFormat::Text)]
    pub format: CoverageOutputFormat,
}

#[derive(Debug, Clone, Args)]
#[command(after_help = "Examples:
  docpact route --root . --paths src/api/client.ts --format json
  docpact route --root . --module src/payments --format text
  docpact route --root . --intent payments --detail full

Use --paths when target files are known, --module for a repo-relative prefix, and --intent only for aliases listed by `docpact render --view routing-summary`.")]
pub struct RouteArgs {
    /// Repository root. Defaults to the current working directory.
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Explicit config file. Defaults to .docpact/config.yaml under --root.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Comma-separated repo-relative paths or globs.
    #[arg(long)]
    pub paths: Option<String>,
    /// Comma-separated repo-relative path prefixes. Glob syntax is not accepted.
    #[arg(long, value_delimiter = ',')]
    pub module: Vec<String>,
    /// Comma-separated controlled aliases from effective routing.intents.
    #[arg(long, value_delimiter = ',')]
    pub intent: Vec<String>,
    /// compact is short by default; full includes provenance and scoring detail.
    #[arg(long, value_enum, default_value_t = RouteDetail::Compact)]
    pub detail: RouteDetail,
    /// Limit text rows only. JSON still returns the full recommendation set.
    #[arg(long, value_parser = parse_positive_usize)]
    pub limit: Option<usize>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = RouteOutputFormat::Text)]
    pub format: RouteOutputFormat,
}

#[derive(Debug, Clone, Args)]
#[command(after_help = "Examples:
  docpact render --root . --view routing-summary --format text
  docpact render --root . --view navigation-summary --paths src/api/client.ts --format json
  docpact render --root . --view catalog-summary --format text

Only navigation-summary accepts --paths, --module, and --intent. Use routing-summary to discover effective route intents.")]
pub struct RenderArgs {
    /// Repository root. Defaults to the current working directory.
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Explicit config file. Defaults to .docpact/config.yaml under --root.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Read-only derived view to render.
    #[arg(long, value_enum)]
    pub view: RenderView,
    /// Navigation-summary only: comma-separated repo-relative paths or globs.
    #[arg(long)]
    pub paths: Option<String>,
    /// Navigation-summary only: comma-separated repo-relative path prefixes.
    #[arg(long, value_delimiter = ',')]
    pub module: Vec<String>,
    /// Navigation-summary only: comma-separated controlled intent aliases.
    #[arg(long, value_delimiter = ',')]
    pub intent: Vec<String>,
    /// Limit text rows only. JSON still returns the full derived structure.
    #[arg(long, value_parser = parse_positive_usize)]
    pub limit: Option<usize>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = RenderOutputFormat::Text)]
    pub format: RenderOutputFormat,
}

#[derive(Debug, Clone, Args)]
#[command(after_help = "Example:
  docpact list-rules --root . --format json

Lists effective rules after workspace inheritance and overrides.")]
pub struct ListRulesArgs {
    /// Repository root. Defaults to the current working directory.
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Explicit config file. Defaults to .docpact/config.yaml under --root.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = ListRulesOutputFormat::Text)]
    pub format: ListRulesOutputFormat,
}

#[derive(Debug, Clone, Args)]
#[command(after_help = "Example:
  docpact doctor --root . --format json

Checks higher-level workspace/catalog/ownership consistency and emits machine-readable findings in JSON.")]
pub struct DoctorArgs {
    /// Repository root. Defaults to the current working directory.
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Explicit config file. Defaults to .docpact/config.yaml under --root.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = DoctorOutputFormat::Text)]
    pub format: DoctorOutputFormat,
}

#[derive(Debug, Clone, Args)]
#[command(after_help = "Example:
  docpact freshness --root . --format json

Audits lastReviewedAt / lastReviewedCommit metadata against current repo history.")]
pub struct FreshnessArgs {
    /// Repository root. Defaults to the current working directory.
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Explicit config file. Defaults to .docpact/config.yaml under --root.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = FreshnessOutputFormat::Text)]
    pub format: FreshnessOutputFormat,
}

#[derive(Debug, Clone, Args)]
pub struct DiagnosticsArgs {
    #[command(subcommand)]
    pub command: DiagnosticsCommands,
}

#[derive(Debug, Clone, Subcommand)]
pub enum DiagnosticsCommands {
    Show(DiagnosticsShowArgs),
}

#[derive(Debug, Clone, Args)]
#[command(after_help = "Example:
  docpact diagnostics show --report .docpact/runs/latest.json --id d001
  docpact diagnostics show --report .docpact/runs/latest.json --id d001 --format json

Reads the full diagnostics artifact produced by `docpact lint --output` or the default run path.")]
pub struct DiagnosticsShowArgs {
    /// Full diagnostics artifact path.
    #[arg(long)]
    pub report: PathBuf,
    /// Diagnostic id to inspect, for example d001.
    #[arg(long)]
    pub id: String,
    /// Output format.
    #[arg(long, value_enum, default_value_t = DiagnosticsOutputFormat::Text)]
    pub format: DiagnosticsOutputFormat,
}

#[derive(Debug, Clone, Args)]
pub struct ReviewArgs {
    #[command(subcommand)]
    pub command: ReviewCommands,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ReviewCommands {
    Mark(ReviewMarkArgs),
}

#[derive(Debug, Clone, Args)]
#[command(after_help = "Examples:
  docpact review mark --root . --path docs/api.md
  docpact review mark --root . --report .docpact/runs/latest.json --id d001

Use either one or more --path values, or --report with --id. Do not mix the two target modes.")]
pub struct ReviewMarkArgs {
    /// Repository root. Defaults to the current working directory.
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Document path to mark as reviewed. May be repeated.
    #[arg(long = "path")]
    pub paths: Vec<PathBuf>,
    /// Diagnostics report containing the finding to mark reviewed.
    #[arg(long)]
    pub report: Option<PathBuf>,
    /// Diagnostic id from --report.
    #[arg(long)]
    pub id: Option<String>,
    /// Review date in YYYY-MM-DD format. Defaults to today.
    #[arg(long, value_parser = parse_iso_date)]
    pub date: Option<String>,
    /// Reviewed commit. Defaults to current HEAD.
    #[arg(long)]
    pub commit: Option<String>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = ReviewOutputFormat::Text)]
    pub format: ReviewOutputFormat,
}

#[derive(Debug, Clone, Args)]
#[command(after_help = "Example:
  docpact explain src/api/client.ts --root .
  docpact explain src/api/client.ts --root . --format json

Use route for read-first navigation; explain is a quick rule-match inspection helper.")]
pub struct ExplainArgs {
    /// Repo-relative path to explain.
    pub path: PathBuf,
    /// Repository root. Defaults to the current working directory.
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Explicit config file. Defaults to .docpact/config.yaml under --root.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = ExplainOutputFormat::Text)]
    pub format: ExplainOutputFormat,
}

#[derive(Debug, Clone, Args)]
#[command(after_help = "Examples:
  docpact validate-config --root .
  docpact validate-config --root . --strict
  docpact validate-config --root . --strict --format json

Default mode checks that config loads. --strict also validates graph consistency, routing aliases, ownership, coverage, freshness, and doc inventory.")]
pub struct ValidateConfigArgs {
    /// Repository root. Defaults to the current working directory.
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Explicit config file. Defaults to .docpact/config.yaml under --root.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Run full structural and graph validation.
    #[arg(long, default_value_t = false)]
    pub strict: bool,
    /// Output format.
    #[arg(long, value_enum, default_value_t = ValidateConfigOutputFormat::Text)]
    pub format: ValidateConfigOutputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LintMode {
    Warn,
    Enforce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
    Sarif,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ListRulesOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DoctorOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CoverageOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RouteOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RenderOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RenderView {
    CatalogSummary,
    OwnershipSummary,
    NavigationSummary,
    RoutingSummary,
    WorkspaceSummary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RouteDetail {
    Compact,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FreshnessOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DiagnosticsOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReviewOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ExplainOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ValidateConfigOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum WaiverOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DiagnosticDetail {
    Summary,
    Compact,
    Full,
}

impl DiagnosticDetail {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Compact => "compact",
            Self::Full => "full",
        }
    }
}

fn parse_positive_usize(value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(0) => Err("value must be greater than 0".into()),
        Ok(parsed) => Ok(parsed),
        Err(_) => Err(format!("invalid positive integer: {value}")),
    }
}

fn parse_iso_date(value: &str) -> Result<String, String> {
    if value.len() != 10 {
        return Err(format!("invalid YYYY-MM-DD date: {value}"));
    }

    let bytes = value.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(format!("invalid YYYY-MM-DD date: {value}"));
    }

    if !bytes
        .iter()
        .enumerate()
        .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
    {
        return Err(format!("invalid YYYY-MM-DD date: {value}"));
    }

    let month = value[5..7]
        .parse::<u8>()
        .map_err(|_| format!("invalid YYYY-MM-DD date: {value}"))?;
    let day = value[8..10]
        .parse::<u8>()
        .map_err(|_| format!("invalid YYYY-MM-DD date: {value}"))?;

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(format!("invalid YYYY-MM-DD date: {value}"));
    }

    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{
        BaselineCommands, Cli, Commands, CoverageOutputFormat, DiagnosticDetail,
        DiagnosticsCommands, DiagnosticsOutputFormat, DoctorOutputFormat, ExplainOutputFormat,
        FreshnessOutputFormat, LintMode, ListRulesOutputFormat, OutputFormat, ReviewCommands,
        ReviewOutputFormat, RouteDetail, RouteOutputFormat, ValidateConfigOutputFormat,
        WaiverCommands, WaiverOutputFormat,
    };

    #[test]
    fn parses_lint_command() {
        let cli = Cli::try_parse_from([
            "docpact",
            "lint",
            "--base",
            "abc123",
            "--head",
            "def456",
            "--mode",
            "enforce",
            "--format",
            "json",
            "--detail",
            "full",
            "--diagnostics-page",
            "2",
            "--diagnostics-page-size",
            "9",
            "--fail-on-uncovered-change",
            "--fail-on-stale-docs",
            "--baseline",
            ".docpact/baseline.json",
            "--waivers",
            ".docpact/waivers.yaml",
            "--output",
            ".docpact/runs/latest.json",
        ])
        .expect("cli should parse");

        match cli.command {
            Commands::Lint(args) => {
                assert_eq!(args.base.as_deref(), Some("abc123"));
                assert_eq!(args.head.as_deref(), Some("def456"));
                assert_eq!(args.mode, LintMode::Enforce);
                assert_eq!(args.format, OutputFormat::Json);
                assert_eq!(args.detail, DiagnosticDetail::Full);
                assert_eq!(args.diagnostics_page, 2);
                assert_eq!(args.diagnostics_page_size, 9);
                assert!(args.fail_on_uncovered_change);
                assert!(args.fail_on_stale_docs);
                assert_eq!(
                    args.baseline.as_deref(),
                    Some(std::path::Path::new(".docpact/baseline.json"))
                );
                assert_eq!(
                    args.waivers.as_deref(),
                    Some(std::path::Path::new(".docpact/waivers.yaml"))
                );
                assert_eq!(
                    args.output.as_deref(),
                    Some(std::path::Path::new(".docpact/runs/latest.json"))
                );
            }
            _ => panic!("expected lint command"),
        }
    }

    #[test]
    fn parses_diagnostics_show_command() {
        let cli = Cli::try_parse_from([
            "docpact",
            "diagnostics",
            "show",
            "--report",
            ".docpact/runs/latest.json",
            "--id",
            "d003",
            "--format",
            "json",
        ])
        .expect("cli should parse");

        match cli.command {
            Commands::Diagnostics(args) => match args.command {
                DiagnosticsCommands::Show(show) => {
                    assert_eq!(
                        show.report,
                        std::path::PathBuf::from(".docpact/runs/latest.json")
                    );
                    assert_eq!(show.id, "d003");
                    assert_eq!(show.format, DiagnosticsOutputFormat::Json);
                }
            },
            _ => panic!("expected diagnostics command"),
        }
    }

    #[test]
    fn parses_route_command() {
        let cli = Cli::try_parse_from([
            "docpact",
            "route",
            "--root",
            ".",
            "--config",
            ".docpact/config.yaml",
            "--paths",
            "src/payments/charge.ts,src/payments/refund.ts",
            "--module",
            "src/payments",
            "--intent",
            "payments,auth",
            "--detail",
            "full",
            "--limit",
            "7",
            "--format",
            "json",
        ])
        .expect("cli should parse");

        match cli.command {
            Commands::Route(args) => {
                assert_eq!(args.root.as_deref(), Some(std::path::Path::new(".")));
                assert_eq!(
                    args.config.as_deref(),
                    Some(std::path::Path::new(".docpact/config.yaml"))
                );
                assert_eq!(
                    args.paths.as_deref(),
                    Some("src/payments/charge.ts,src/payments/refund.ts")
                );
                assert_eq!(args.module, vec!["src/payments"]);
                assert_eq!(args.intent, vec!["payments", "auth"]);
                assert_eq!(args.detail, RouteDetail::Full);
                assert_eq!(args.limit, Some(7));
                assert_eq!(args.format, RouteOutputFormat::Json);
            }
            _ => panic!("expected route command"),
        }
    }

    #[test]
    fn parses_render_command() {
        let cli = Cli::try_parse_from([
            "docpact",
            "render",
            "--root",
            ".",
            "--view",
            "navigation-summary",
            "--paths",
            "src/payments/charge.ts",
            "--module",
            "src/payments",
            "--intent",
            "payments",
            "--limit",
            "5",
            "--format",
            "json",
        ])
        .expect("cli should parse");

        match cli.command {
            Commands::Render(args) => {
                assert_eq!(args.root.as_deref(), Some(std::path::Path::new(".")));
                assert_eq!(args.view, super::RenderView::NavigationSummary);
                assert_eq!(args.paths.as_deref(), Some("src/payments/charge.ts"));
                assert_eq!(args.module, vec!["src/payments"]);
                assert_eq!(args.intent, vec!["payments"]);
                assert_eq!(args.limit, Some(5));
                assert_eq!(args.format, super::RenderOutputFormat::Json);
            }
            _ => panic!("expected render command"),
        }
    }

    #[test]
    fn parses_baseline_create_command() {
        let cli = Cli::try_parse_from([
            "docpact",
            "baseline",
            "create",
            "--report",
            ".docpact/runs/latest.json",
            "--output",
            ".docpact/baseline.json",
        ])
        .expect("cli should parse");

        match cli.command {
            Commands::Baseline(args) => match args.command {
                BaselineCommands::Create(create) => {
                    assert_eq!(
                        create.report,
                        std::path::PathBuf::from(".docpact/runs/latest.json")
                    );
                    assert_eq!(
                        create.output,
                        std::path::PathBuf::from(".docpact/baseline.json")
                    );
                }
            },
            _ => panic!("expected baseline command"),
        }
    }

    #[test]
    fn parses_waiver_add_command() {
        let cli = Cli::try_parse_from([
            "docpact",
            "waiver",
            "add",
            "--root",
            ".",
            "--report",
            ".docpact/runs/latest.json",
            "--id",
            "d001",
            "--reason",
            "legacy migration in progress",
            "--owner",
            "docs-team",
            "--expires-at",
            "2026-05-01",
            "--scope-rule-id",
            "api-docs",
            "--scope-path",
            "README.md",
            "--waivers",
            ".docpact/waivers.yaml",
            "--format",
            "json",
        ])
        .expect("cli should parse");

        match cli.command {
            Commands::Waiver(args) => match args.command {
                WaiverCommands::Add(add) => {
                    assert_eq!(add.root.as_deref(), Some(std::path::Path::new(".")));
                    assert_eq!(
                        add.report,
                        std::path::PathBuf::from(".docpact/runs/latest.json")
                    );
                    assert_eq!(add.id, "d001");
                    assert_eq!(add.reason, "legacy migration in progress");
                    assert_eq!(add.owner, "docs-team");
                    assert_eq!(add.expires_at, "2026-05-01");
                    assert_eq!(add.scope_rule_ids, vec!["api-docs"]);
                    assert_eq!(add.scope_paths, vec!["README.md"]);
                    assert_eq!(
                        add.waivers,
                        std::path::PathBuf::from(".docpact/waivers.yaml")
                    );
                    assert_eq!(add.format, WaiverOutputFormat::Json);
                }
            },
            _ => panic!("expected waiver command"),
        }
    }

    #[test]
    fn parses_coverage_command() {
        let cli = Cli::try_parse_from(["docpact", "coverage", "--root", ".", "--format", "json"])
            .expect("cli should parse");

        match cli.command {
            Commands::Coverage(args) => {
                assert_eq!(args.root.as_deref(), Some(std::path::Path::new(".")));
                assert_eq!(args.format, CoverageOutputFormat::Json);
            }
            _ => panic!("expected coverage command"),
        }
    }

    #[test]
    fn parses_list_rules_command() {
        let cli = Cli::try_parse_from(["docpact", "list-rules", "--root", ".", "--format", "json"])
            .expect("cli should parse");

        match cli.command {
            Commands::ListRules(args) => {
                assert_eq!(args.root.as_deref(), Some(std::path::Path::new(".")));
                assert_eq!(args.format, ListRulesOutputFormat::Json);
            }
            _ => panic!("expected list-rules command"),
        }
    }

    #[test]
    fn parses_doctor_command() {
        let cli = Cli::try_parse_from(["docpact", "doctor", "--root", ".", "--format", "json"])
            .expect("cli should parse");

        match cli.command {
            Commands::Doctor(args) => {
                assert_eq!(args.root.as_deref(), Some(std::path::Path::new(".")));
                assert_eq!(args.format, DoctorOutputFormat::Json);
            }
            _ => panic!("expected doctor command"),
        }
    }

    #[test]
    fn parses_freshness_command() {
        let cli = Cli::try_parse_from(["docpact", "freshness", "--root", ".", "--format", "json"])
            .expect("cli should parse");

        match cli.command {
            Commands::Freshness(args) => {
                assert_eq!(args.root.as_deref(), Some(std::path::Path::new(".")));
                assert_eq!(args.format, FreshnessOutputFormat::Json);
            }
            _ => panic!("expected freshness command"),
        }
    }

    #[test]
    fn parses_review_mark_path_command() {
        let cli = Cli::try_parse_from([
            "docpact",
            "review",
            "mark",
            "--root",
            ".",
            "--path",
            "docs/api.md",
            "--path",
            "AGENTS.md",
            "--date",
            "2026-04-21",
            "--commit",
            "abc123",
            "--format",
            "json",
        ])
        .expect("cli should parse");

        match cli.command {
            Commands::Review(args) => match args.command {
                ReviewCommands::Mark(mark) => {
                    assert_eq!(mark.paths.len(), 2);
                    assert_eq!(mark.date.as_deref(), Some("2026-04-21"));
                    assert_eq!(mark.commit.as_deref(), Some("abc123"));
                    assert_eq!(mark.format, ReviewOutputFormat::Json);
                }
            },
            _ => panic!("expected review command"),
        }
    }

    #[test]
    fn parses_review_mark_report_command() {
        let cli = Cli::try_parse_from([
            "docpact",
            "review",
            "mark",
            "--report",
            ".docpact/runs/latest.json",
            "--id",
            "d001",
        ])
        .expect("cli should parse");

        match cli.command {
            Commands::Review(args) => match args.command {
                ReviewCommands::Mark(mark) => {
                    assert_eq!(
                        mark.report,
                        Some(std::path::PathBuf::from(".docpact/runs/latest.json"))
                    );
                    assert_eq!(mark.id.as_deref(), Some("d001"));
                }
            },
            _ => panic!("expected review command"),
        }
    }

    #[test]
    fn parses_validate_config_strict_flag() {
        let cli =
            Cli::try_parse_from(["docpact", "validate-config", "--strict", "--format", "json"])
                .expect("cli should parse");

        match cli.command {
            Commands::ValidateConfig(args) => {
                assert!(args.strict);
                assert_eq!(args.format, ValidateConfigOutputFormat::Json);
            }
            _ => panic!("expected validate-config command"),
        }
    }

    #[test]
    fn parses_explain_json_format() {
        let cli = Cli::try_parse_from([
            "docpact",
            "explain",
            "src/api/client.ts",
            "--root",
            ".",
            "--format",
            "json",
        ])
        .expect("cli should parse");

        match cli.command {
            Commands::Explain(args) => {
                assert_eq!(args.path, std::path::PathBuf::from("src/api/client.ts"));
                assert_eq!(args.format, ExplainOutputFormat::Json);
            }
            _ => panic!("expected explain command"),
        }
    }

    #[test]
    fn core_help_mentions_examples_and_json_contracts() {
        let mut command = Cli::command();
        let help = command.render_long_help().to_string();

        assert!(help.contains("Diff-driven documentation drift gate"));
        assert!(help.contains("lint"));

        let mut route = Cli::command()
            .find_subcommand_mut("route")
            .expect("route command exists")
            .clone();
        let route_help = route.render_long_help().to_string();
        assert!(route_help.contains("Examples:"));
        assert!(route_help.contains("routing-summary"));

        let mut diagnostics = Cli::command()
            .find_subcommand_mut("diagnostics")
            .expect("diagnostics command exists")
            .clone();
        let diagnostics_help = diagnostics.render_long_help().to_string();
        assert!(diagnostics_help.contains("show"));

        let mut validate = Cli::command()
            .find_subcommand_mut("validate-config")
            .expect("validate-config command exists")
            .clone();
        let validate_help = validate.render_long_help().to_string();
        assert!(validate_help.contains("--format"));
        assert!(validate_help.contains("--strict --format json"));
    }
}
