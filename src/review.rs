//! Implementation of `docpact review`.
//!
//! Review commands record explicit review evidence in supported documentation
//! metadata formats.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use miette::{IntoDiagnostic, Result, bail, miette};
use serde::Serialize;

use crate::AppExit;
use crate::cli::{ReviewArgs, ReviewCommands, ReviewMarkArgs, ReviewOutputFormat};
use crate::config::{normalize_path, root_dir_from_option};
use crate::diagnostics::read_diagnostics_artifact;
use crate::git::get_head_commit;
use crate::metadata::{apply_review_metadata_to_markdown, apply_review_metadata_to_yaml};
use crate::reporters::OutputWarning;

pub fn run(args: ReviewArgs) -> Result<AppExit> {
    match args.command {
        ReviewCommands::Mark(args) => mark(args),
    }
}

fn mark(args: ReviewMarkArgs) -> Result<AppExit> {
    let root_dir = root_dir_from_option(args.root.as_deref())?;
    let targets = resolve_targets(&root_dir, &args)?;
    let reviewed_at = resolve_reviewed_at(args.date.as_deref())?;
    let reviewed_commit = resolve_reviewed_commit(&root_dir, args.commit.as_deref())?;

    let mut items = Vec::new();
    for target in targets {
        items.push(update_review_metadata(
            &root_dir,
            &target.path,
            &reviewed_at,
            &reviewed_commit,
            target.diagnostic_id.as_deref(),
        )?);
    }

    emit_result(
        ReviewMarkResult {
            schema_version: "docpact.review-mark.v1".into(),
            tool_name: env!("CARGO_PKG_NAME").into(),
            tool_version: env!("CARGO_PKG_VERSION").into(),
            command: "review mark".into(),
            warnings: Vec::new(),
            status: "ok".into(),
            mode: if args.report.is_some() {
                "diagnostic".into()
            } else {
                "path".into()
            },
            reviewed_at,
            reviewed_commit,
            items,
        },
        args.format,
    );

    Ok(AppExit::Success)
}

#[derive(Debug, Clone)]
struct ReviewTarget {
    path: String,
    diagnostic_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct ReviewMarkResult {
    schema_version: String,
    tool_name: String,
    tool_version: String,
    command: String,
    warnings: Vec<OutputWarning>,
    status: String,
    mode: String,
    reviewed_at: String,
    reviewed_commit: String,
    items: Vec<ReviewMarkItem>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct ReviewMarkItem {
    path: String,
    changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostic_id: Option<String>,
}

fn resolve_targets(root_dir: &Path, args: &ReviewMarkArgs) -> Result<Vec<ReviewTarget>> {
    let has_paths = !args.paths.is_empty();
    let has_report = args.report.is_some() || args.id.is_some();

    if has_paths && has_report {
        bail!("pass either --path or --report with --id, but do not mix them");
    }

    if has_paths {
        let mut seen = HashSet::new();
        let mut targets = Vec::new();
        for path in &args.paths {
            let normalized = normalize_review_path(root_dir, path)?;
            if seen.insert(normalized.clone()) {
                targets.push(ReviewTarget {
                    path: normalized,
                    diagnostic_id: None,
                });
            }
        }
        return Ok(targets);
    }

    let Some(report_path) = args.report.as_deref() else {
        bail!("pass either at least one --path or both --report and --id");
    };
    let Some(diagnostic_id) = args.id.as_deref() else {
        bail!("--report requires --id");
    };

    let artifact = read_diagnostics_artifact(report_path)?;
    let diagnostic = artifact
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.diagnostic_id == diagnostic_id)
        .ok_or_else(|| {
            miette!(
                "diagnostic `{}` was not found in report {}",
                diagnostic_id,
                report_path.display()
            )
        })?;

    Ok(vec![ReviewTarget {
        path: diagnostic.path.clone(),
        diagnostic_id: Some(diagnostic_id.to_string()),
    }])
}

fn normalize_review_path(root_dir: &Path, path: &Path) -> Result<String> {
    if path.is_absolute() {
        let relative = path.strip_prefix(root_dir).map_err(|_| {
            miette!(
                "path {} is outside root {}",
                path.display(),
                root_dir.display()
            )
        })?;
        return Ok(normalize_path(&relative.to_string_lossy()));
    }

    Ok(normalize_path(&path.to_string_lossy()))
}

fn resolve_reviewed_at(override_date: Option<&str>) -> Result<String> {
    if let Some(value) = override_date {
        return Ok(value.to_string());
    }

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

fn resolve_reviewed_commit(root_dir: &Path, override_commit: Option<&str>) -> Result<String> {
    match override_commit {
        Some(value) if !value.trim().is_empty() => Ok(value.trim().to_string()),
        Some(_) => bail!("--commit cannot be empty"),
        None => get_head_commit(root_dir),
    }
}

fn update_review_metadata(
    root_dir: &Path,
    rel_path: &str,
    reviewed_at: &str,
    reviewed_commit: &str,
    diagnostic_id: Option<&str>,
) -> Result<ReviewMarkItem> {
    let abs_path = root_dir.join(rel_path);
    if !abs_path.exists() {
        bail!("path does not exist: {}", abs_path.display());
    }

    let current = fs::read_to_string(&abs_path).into_diagnostic()?;
    let updated = if is_reviewable_markdown_path(rel_path) {
        apply_review_metadata_to_markdown(&current, reviewed_at, reviewed_commit)
    } else if is_reviewable_yaml_path(rel_path) {
        apply_review_metadata_to_yaml(&current, reviewed_at, reviewed_commit)
    } else {
        bail!(
            "review mark only supports Markdown and YAML documents: {}",
            rel_path
        );
    };

    let changed = updated != current;
    if changed {
        fs::write(&abs_path, updated).into_diagnostic()?;
    }

    Ok(ReviewMarkItem {
        path: rel_path.to_string(),
        changed,
        diagnostic_id: diagnostic_id.map(str::to_string),
    })
}

fn is_reviewable_markdown_path(rel_path: &str) -> bool {
    normalize_path(rel_path).ends_with(".md")
}

fn is_reviewable_yaml_path(rel_path: &str) -> bool {
    let normalized = normalize_path(rel_path);
    normalized.ends_with(".yaml") || normalized.ends_with(".yml")
}

fn emit_result(result: ReviewMarkResult, format: ReviewOutputFormat) {
    match format {
        ReviewOutputFormat::Text => {
            println!(
                "Recorded review evidence: lastReviewedAt={}, lastReviewedCommit={}",
                result.reviewed_at, result.reviewed_commit
            );
            for item in result.items {
                if let Some(diagnostic_id) = item.diagnostic_id {
                    println!(
                        "- path={} changed={} diagnostic_id={}",
                        item.path, item.changed, diagnostic_id
                    );
                } else {
                    println!("- path={} changed={}", item.path, item.changed);
                }
            }
        }
        ReviewOutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&result).expect("review result should serialize")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::cli::{ReviewCommands, ReviewMarkArgs, ReviewOutputFormat};
    use crate::reporters::{DiagnosticsArtifact, Problem, build_diagnostics_artifact};

    use super::{
        normalize_review_path, resolve_reviewed_commit, resolve_targets, update_review_metadata,
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
        assert!(status.success(), "git {} failed", args.join(" "));
    }

    fn init_git_repo(root: &Path) {
        git(root, &["init"]);
        git(root, &["config", "user.name", "Codex"]);
        git(root, &["config", "user.email", "codex@example.com"]);
    }

    fn sample_mark_args() -> ReviewMarkArgs {
        ReviewMarkArgs {
            root: None,
            paths: Vec::new(),
            report: None,
            id: None,
            date: None,
            commit: None,
            format: ReviewOutputFormat::Text,
        }
    }

    fn sample_artifact() -> DiagnosticsArtifact {
        build_diagnostics_artifact(
            &[Problem::missing_review(
                "docs/api.md".into(),
                "repo-rule".into(),
                ".docpact/config.yaml".into(),
                "review_or_update".into(),
                "required_doc_not_touched".into(),
                "touch_required_doc".into(),
                vec!["src/index.ts".into()],
                "repo rationale".into(),
                "missing review".into(),
            )],
            &["src/index.ts".into()],
            1,
        )
    }

    #[test]
    fn reject_mixed_path_and_report_inputs() {
        let root = temp_dir("docpact-review-mixed");
        let mut args = sample_mark_args();
        args.paths.push(PathBuf::from("docs/api.md"));
        args.report = Some(root.join("report.json"));
        args.id = Some("d001".into());

        let error = resolve_targets(&root, &args).expect_err("mixed inputs should fail");
        assert!(error.to_string().contains("do not mix"));
    }

    #[test]
    fn normalize_review_path_accepts_absolute_paths_under_root() {
        let root = temp_dir("docpact-review-path");
        let nested = root.join("docs/api.md");
        fs::create_dir_all(nested.parent().expect("parent")).expect("parent dir");
        fs::write(&nested, "# API\n").expect("doc");

        let normalized = normalize_review_path(&root, &nested).expect("path should normalize");
        assert_eq!(normalized, "docs/api.md");
    }

    #[test]
    fn review_mark_updates_markdown_without_frontmatter() {
        let root = temp_dir("docpact-review-markdown");
        fs::create_dir_all(root.join("docs")).expect("docs dir");
        fs::write(root.join("docs/api.md"), "# API\n").expect("doc");

        let item = update_review_metadata(&root, "docs/api.md", "2026-04-21", "abc123", None)
            .expect("markdown update should succeed");

        assert!(item.changed);
        let updated = fs::read_to_string(root.join("docs/api.md")).expect("updated doc");
        assert!(updated.starts_with(
            "---\nlastReviewedAt: 2026-04-21\nlastReviewedCommit: abc123\n---\n\n# API\n"
        ));
    }

    #[test]
    fn review_mark_updates_existing_yaml_without_touching_other_keys() {
        let root = temp_dir("docpact-review-yaml");
        fs::create_dir_all(root.join(".docpact")).expect("doc dir");
        fs::write(
            root.join(".docpact/config.yaml"),
            "version: 1\nlayout: repo\nlastReviewedAt: \"2026-04-18\"\n",
        )
        .expect("yaml");

        let item = update_review_metadata(
            &root,
            ".docpact/config.yaml",
            "2026-04-21",
            "abc123",
            Some("d001"),
        )
        .expect("yaml update should succeed");

        assert!(item.changed);
        let updated = fs::read_to_string(root.join(".docpact/config.yaml")).expect("updated yaml");
        assert!(updated.contains("version: 1"));
        assert!(updated.contains("layout: repo"));
        assert!(updated.contains("lastReviewedAt: \"2026-04-21\""));
        assert!(updated.contains("lastReviewedCommit: \"abc123\""));
    }

    #[test]
    fn resolve_targets_uses_report_artifact_for_diagnostic_lookup() {
        let root = temp_dir("docpact-review-report");
        let report_path = root.join(".docpact/runs/latest.json");
        fs::create_dir_all(report_path.parent().expect("parent")).expect("runs dir");
        fs::write(
            &report_path,
            serde_json::to_string_pretty(&sample_artifact()).expect("artifact should serialize"),
        )
        .expect("report");

        let mut args = sample_mark_args();
        args.report = Some(report_path);
        args.id = Some("d001".into());

        let targets = resolve_targets(&root, &args).expect("artifact lookup should succeed");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].path, "docs/api.md");
        assert_eq!(targets[0].diagnostic_id.as_deref(), Some("d001"));
    }

    #[test]
    fn resolve_reviewed_commit_defaults_to_head() {
        let root = temp_dir("docpact-review-head");
        init_git_repo(&root);
        fs::write(root.join("README.md"), "# Example\n").expect("readme");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);

        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .expect("git should run");
        let expected = String::from_utf8(head.stdout)
            .expect("utf8")
            .trim()
            .to_string();

        let actual = resolve_reviewed_commit(&root, None).expect("head commit should resolve");
        assert_eq!(actual, expected);
    }

    #[test]
    fn review_command_enum_exposes_mark_subcommand() {
        let args = ReviewCommands::Mark(sample_mark_args());
        match args {
            ReviewCommands::Mark(mark) => assert_eq!(mark.format, ReviewOutputFormat::Text),
        }
    }
}
