use std::fs;
use std::path::{Path, PathBuf};

const STARTER_CLAW_JSON: &str = concat!(
    "{\n",
    "  \"permissions\": {\n",
    "    \"defaultMode\": \"dontAsk\"\n",
    "  }\n",
    "}\n",
);
const GITIGNORE_COMMENT: &str = "# Claw Code local artifacts";
const GITIGNORE_ENTRIES: [&str; 3] = [".claw/settings.local.json", ".claw/sessions/", ".clawhip/"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitStatus {
    Created,
    Updated,
    Skipped,
    /// 仅在 `--force` 模式下出现：文件已存在但被预置模板覆盖。
    Overwritten,
}

impl InitStatus {
    #[must_use]
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Skipped => "skipped (already exists)",
            Self::Overwritten => "overwritten (forced)",
        }
    }

    /// Machine-stable identifier for structured output (#142).
    /// Unlike `label()`, this never changes wording: claws can switch on
    /// these values without brittle substring matching.
    #[must_use]
    pub(crate) fn json_tag(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Skipped => "skipped",
            Self::Overwritten => "overwritten",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InitArtifact {
    pub(crate) name: &'static str,
    pub(crate) status: InitStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InitReport {
    pub(crate) project_root: PathBuf,
    pub(crate) artifacts: Vec<InitArtifact>,
}

impl InitReport {
    #[must_use]
    pub(crate) fn render(&self) -> String {
        let mut lines = vec![
            "Init".to_string(),
            format!("  Project          {}", self.project_root.display()),
        ];
        for artifact in &self.artifacts {
            lines.push(format!(
                "  {:<16} {}",
                artifact.name,
                artifact.status.label()
            ));
        }
        lines.push("  Next step        Review and tailor the generated guidance".to_string());
        lines.join("\n")
    }

    /// Summary constant that claws can embed in JSON output without having
    /// to read it out of the human-formatted `message` string (#142).
    pub(crate) const NEXT_STEP: &'static str = "Review and tailor the generated guidance";

    /// Artifact names that ended in the given status. Used to build the
    /// structured `created[]`/`updated[]`/`skipped[]` arrays for #142.
    #[must_use]
    pub(crate) fn artifacts_with_status(&self, status: InitStatus) -> Vec<String> {
        self.artifacts
            .iter()
            .filter(|artifact| artifact.status == status)
            .map(|artifact| artifact.name.to_string())
            .collect()
    }

    /// Structured artifact list for JSON output (#142). Each entry carries
    /// `name` and machine-stable `status` tag.
    #[must_use]
    pub(crate) fn artifact_json_entries(&self) -> Vec<serde_json::Value> {
        self.artifacts
            .iter()
            .map(|artifact| {
                serde_json::json!({
                    "name": artifact.name,
                    "status": artifact.status.json_tag(),
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
struct RepoDetection {
    rust_workspace: bool,
    rust_root: bool,
    python: bool,
    package_json: bool,
    typescript: bool,
    nextjs: bool,
    react: bool,
    vite: bool,
    nest: bool,
    src_dir: bool,
    tests_dir: bool,
    rust_dir: bool,
}

pub(crate) fn initialize_repo(
    cwd: &Path,
    force: bool,
) -> Result<InitReport, Box<dyn std::error::Error>> {
    let mut artifacts = Vec::new();

    let claw_dir = cwd.join(".claw");
    artifacts.push(InitArtifact {
        name: ".claw/",
        status: ensure_dir(&claw_dir)?,
    });

    let claw_json = cwd.join(".claw.json");
    artifacts.push(InitArtifact {
        name: ".claw.json",
        status: write_file_if_missing(&claw_json, STARTER_CLAW_JSON, force)?,
    });

    let gitignore = cwd.join(".gitignore");
    artifacts.push(InitArtifact {
        name: ".gitignore",
        status: ensure_gitignore_entries(&gitignore)?,
    });

    let claude_md = cwd.join("CLAUDE.md");
    let content = render_init_claude_md(cwd);
    artifacts.push(InitArtifact {
        name: "CLAUDE.md",
        status: write_file_if_missing(&claude_md, &content, force)?,
    });

    Ok(InitReport {
        project_root: cwd.to_path_buf(),
        artifacts,
    })
}

fn ensure_dir(path: &Path) -> Result<InitStatus, std::io::Error> {
    if path.is_dir() {
        return Ok(InitStatus::Skipped);
    }
    fs::create_dir_all(path)?;
    Ok(InitStatus::Created)
}

fn write_file_if_missing(
    path: &Path,
    content: &str,
    force: bool,
) -> Result<InitStatus, std::io::Error> {
    if path.exists() {
        if !force {
            return Ok(InitStatus::Skipped);
        }
        fs::write(path, content)?;
        return Ok(InitStatus::Overwritten);
    }
    fs::write(path, content)?;
    Ok(InitStatus::Created)
}

fn ensure_gitignore_entries(path: &Path) -> Result<InitStatus, std::io::Error> {
    if !path.exists() {
        let mut lines = vec![GITIGNORE_COMMENT.to_string()];
        lines.extend(GITIGNORE_ENTRIES.iter().map(|entry| (*entry).to_string()));
        fs::write(path, format!("{}\n", lines.join("\n")))?;
        return Ok(InitStatus::Created);
    }

    let existing = fs::read_to_string(path)?;
    let mut lines = existing.lines().map(ToOwned::to_owned).collect::<Vec<_>>();
    let mut changed = false;

    if !lines.iter().any(|line| line == GITIGNORE_COMMENT) {
        lines.push(GITIGNORE_COMMENT.to_string());
        changed = true;
    }

    for entry in GITIGNORE_ENTRIES {
        if !lines.iter().any(|line| line == entry) {
            lines.push(entry.to_string());
            changed = true;
        }
    }

    if !changed {
        return Ok(InitStatus::Skipped);
    }

    fs::write(path, format!("{}\n", lines.join("\n")))?;
    Ok(InitStatus::Updated)
}

pub(crate) fn render_init_claude_md(cwd: &Path) -> String {
    let detection = detect_repo(cwd);
    let mut lines = vec![
        "# CLAUDE.md".to_string(),
        String::new(),
        "This file provides guidance to Claw Code (clawcode.dev) when working with code in this repository.".to_string(),
        String::new(),
    ];

    let detected_languages = detected_languages(&detection);
    let detected_frameworks = detected_frameworks(&detection);
    lines.push("## Detected stack".to_string());
    if detected_languages.is_empty() {
        lines.push("- No specific language markers were detected yet; document the primary language and verification commands once the project structure settles.".to_string());
    } else {
        lines.push(format!("- Languages: {}.", detected_languages.join(", ")));
    }
    if detected_frameworks.is_empty() {
        lines.push("- Frameworks: none detected from the supported starter markers.".to_string());
    } else {
        lines.push(format!(
            "- Frameworks/tooling markers: {}.",
            detected_frameworks.join(", ")
        ));
    }
    lines.push(String::new());

    let verification_lines = verification_lines(cwd, &detection);
    if !verification_lines.is_empty() {
        lines.push("## Verification".to_string());
        lines.extend(verification_lines);
        lines.push(String::new());
    }

    let structure_lines = repository_shape_lines(&detection);
    if !structure_lines.is_empty() {
        lines.push("## Repository shape".to_string());
        lines.extend(structure_lines);
        lines.push(String::new());
    }

    let framework_lines = framework_notes(&detection);
    if !framework_lines.is_empty() {
        lines.push("## Framework notes".to_string());
        lines.extend(framework_lines);
        lines.push(String::new());
    }

    lines.push("## Working agreement".to_string());
    // 注意：通用工程原则（小改动、不创建不必要文件等）已由系统内置提示词的
    // `# Doing tasks` 段覆盖，此处只保留项目特定约定，避免重复。
    lines.push("- Keep generated bootstrap files aligned with actual repo workflows.".to_string());
    lines.push("- Keep shared defaults in `.claw.json`; reserve `.claw/settings.local.json` for machine-local overrides.".to_string());
    lines.push("- Do not overwrite existing `CLAUDE.md` content automatically; update it intentionally when repo workflows change.".to_string());
    lines.push(String::new());

    lines.join("\n")
}

fn detect_repo(cwd: &Path) -> RepoDetection {
    let package_json_contents = fs::read_to_string(cwd.join("package.json"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    RepoDetection {
        rust_workspace: cwd.join("rust").join("Cargo.toml").is_file(),
        rust_root: cwd.join("Cargo.toml").is_file(),
        python: cwd.join("pyproject.toml").is_file()
            || cwd.join("requirements.txt").is_file()
            || cwd.join("setup.py").is_file(),
        package_json: cwd.join("package.json").is_file(),
        typescript: cwd.join("tsconfig.json").is_file()
            || package_json_contents.contains("typescript"),
        nextjs: package_json_contents.contains("\"next\""),
        react: package_json_contents.contains("\"react\""),
        vite: package_json_contents.contains("\"vite\""),
        nest: package_json_contents.contains("@nestjs"),
        src_dir: cwd.join("src").is_dir(),
        tests_dir: cwd.join("tests").is_dir(),
        rust_dir: cwd.join("rust").is_dir(),
    }
}

fn detected_languages(detection: &RepoDetection) -> Vec<&'static str> {
    let mut languages = Vec::new();
    if detection.rust_workspace || detection.rust_root {
        languages.push("Rust");
    }
    if detection.python {
        languages.push("Python");
    }
    if detection.typescript {
        languages.push("TypeScript");
    } else if detection.package_json {
        languages.push("JavaScript/Node.js");
    }
    languages
}

fn detected_frameworks(detection: &RepoDetection) -> Vec<&'static str> {
    let mut frameworks = Vec::new();
    if detection.nextjs {
        frameworks.push("Next.js");
    }
    if detection.react {
        frameworks.push("React");
    }
    if detection.vite {
        frameworks.push("Vite");
    }
    if detection.nest {
        frameworks.push("NestJS");
    }
    frameworks
}

fn verification_lines(cwd: &Path, detection: &RepoDetection) -> Vec<String> {
    let mut lines = Vec::new();
    if detection.rust_workspace {
        lines.push("- Run Rust verification from `rust/`: `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`".to_string());
    } else if detection.rust_root {
        lines.push("- Run Rust verification from the repo root: `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`".to_string());
    }
    if detection.python {
        if cwd.join("pyproject.toml").is_file() {
            lines.push("- Run the Python project checks declared in `pyproject.toml` (for example: `pytest`, `ruff check`, and `mypy` when configured).".to_string());
        } else {
            lines.push(
                "- Run the repo's Python test/lint commands before shipping changes.".to_string(),
            );
        }
    }
    if detection.package_json {
        lines.push("- Run the JavaScript/TypeScript checks from `package.json` before shipping changes (`npm test`, `npm run lint`, `npm run build`, or the repo equivalent).".to_string());
    }
    if detection.tests_dir && detection.src_dir {
        lines.push("- `src/` and `tests/` are both present; update both surfaces together when behavior changes.".to_string());
    }
    lines
}

fn repository_shape_lines(detection: &RepoDetection) -> Vec<String> {
    let mut lines = Vec::new();
    if detection.rust_dir {
        lines.push(
            "- `rust/` contains the Rust workspace and active CLI/runtime implementation."
                .to_string(),
        );
    }
    if detection.src_dir {
        lines.push("- `src/` contains source files that should stay consistent with generated guidance and tests.".to_string());
    }
    if detection.tests_dir {
        lines.push("- `tests/` contains validation surfaces that should be reviewed alongside code changes.".to_string());
    }
    lines
}

fn framework_notes(detection: &RepoDetection) -> Vec<String> {
    let mut lines = Vec::new();
    if detection.nextjs {
        lines.push("- Next.js detected: preserve routing/data-fetching conventions and verify production builds after changing app structure.".to_string());
    }
    if detection.react && !detection.nextjs {
        lines.push("- React detected: keep component behavior covered with focused tests and avoid unnecessary prop/API churn.".to_string());
    }
    if detection.vite {
        lines.push("- Vite detected: validate the production bundle after changing build-sensitive configuration or imports.".to_string());
    }
    if detection.nest {
        lines.push("- NestJS detected: keep module/provider boundaries explicit and verify controller/service wiring after refactors.".to_string());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::{initialize_repo, render_init_claude_md, InitStatus};
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("rusty-claude-init-{nanos}"))
    }

    #[test]
    fn initialize_repo_creates_expected_files_and_gitignore_entries() {
        let root = temp_dir();
        fs::create_dir_all(root.join("rust")).expect("create rust dir");
        fs::write(root.join("rust").join("Cargo.toml"), "[workspace]\n").expect("write cargo");

        let report = initialize_repo(&root, false).expect("init should succeed");
        let rendered = report.render();
        assert!(rendered.contains(".claw/"));
        assert!(rendered.contains(".claw.json"));
        assert!(rendered.contains("created"));
        assert!(rendered.contains(".gitignore       created"));
        assert!(rendered.contains("CLAUDE.md        created"));
        assert!(root.join(".claw").is_dir());
        assert!(root.join(".claw.json").is_file());
        assert!(root.join("CLAUDE.md").is_file());
        assert_eq!(
            fs::read_to_string(root.join(".claw.json")).expect("read claw json"),
            concat!(
                "{\n",
                "  \"permissions\": {\n",
                "    \"defaultMode\": \"dontAsk\"\n",
                "  }\n",
                "}\n",
            )
        );
        let gitignore = fs::read_to_string(root.join(".gitignore")).expect("read gitignore");
        assert!(gitignore.contains(".claw/settings.local.json"));
        assert!(gitignore.contains(".claw/sessions/"));
        assert!(gitignore.contains(".clawhip/"));
        let claude_md = fs::read_to_string(root.join("CLAUDE.md")).expect("read claude md");
        assert!(claude_md.contains("Languages: Rust."));
        assert!(claude_md.contains("cargo clippy --workspace --all-targets -- -D warnings"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn initialize_repo_is_idempotent_and_preserves_existing_files() {
        let root = temp_dir();
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join("CLAUDE.md"), "custom guidance\n").expect("write existing claude md");
        fs::write(root.join(".gitignore"), ".claw/settings.local.json\n").expect("write gitignore");

        let first = initialize_repo(&root, false).expect("first init should succeed");
        assert!(first
            .render()
            .contains("CLAUDE.md        skipped (already exists)"));
        let second = initialize_repo(&root, false).expect("second init should succeed");
        let second_rendered = second.render();
        assert!(second_rendered.contains(".claw/"));
        assert!(second_rendered.contains(".claw.json"));
        assert!(second_rendered.contains("skipped (already exists)"));
        assert!(second_rendered.contains(".gitignore       skipped (already exists)"));
        assert!(second_rendered.contains("CLAUDE.md        skipped (already exists)"));
        assert_eq!(
            fs::read_to_string(root.join("CLAUDE.md")).expect("read existing claude md"),
            "custom guidance\n"
        );
        let gitignore = fs::read_to_string(root.join(".gitignore")).expect("read gitignore");
        assert_eq!(gitignore.matches(".claw/settings.local.json").count(), 1);
        assert_eq!(gitignore.matches(".claw/sessions/").count(), 1);
        assert_eq!(gitignore.matches(".clawhip/").count(), 1);

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn artifacts_with_status_partitions_fresh_and_idempotent_runs() {
        // #142: the structured JSON output needs to be able to partition
        // artifacts into created/updated/skipped without substring matching
        // the human-formatted `message` string.
        let root = temp_dir();
        fs::create_dir_all(&root).expect("create root");

        let fresh = initialize_repo(&root, false).expect("fresh init should succeed");
        let created_names = fresh.artifacts_with_status(InitStatus::Created);
        assert_eq!(
            created_names,
            vec![
                ".claw/".to_string(),
                ".claw.json".to_string(),
                ".gitignore".to_string(),
                "CLAUDE.md".to_string(),
            ],
            "fresh init should place all four artifacts in created[]"
        );
        assert!(
            fresh.artifacts_with_status(InitStatus::Skipped).is_empty(),
            "fresh init should have no skipped artifacts"
        );

        let second = initialize_repo(&root, false).expect("second init should succeed");
        let skipped_names = second.artifacts_with_status(InitStatus::Skipped);
        assert_eq!(
            skipped_names,
            vec![
                ".claw/".to_string(),
                ".claw.json".to_string(),
                ".gitignore".to_string(),
                "CLAUDE.md".to_string(),
            ],
            "idempotent init should place all four artifacts in skipped[]"
        );
        assert!(
            second.artifacts_with_status(InitStatus::Created).is_empty(),
            "idempotent init should have no created artifacts"
        );

        // artifact_json_entries() uses the machine-stable `json_tag()` which
        // never changes wording (unlike `label()` which says "skipped (already exists)").
        let entries = second.artifact_json_entries();
        assert_eq!(entries.len(), 4);
        for entry in &entries {
            let status = entry.get("status").and_then(|v| v.as_str()).unwrap();
            assert_eq!(
                status, "skipped",
                "machine status tag should be the bare word 'skipped', not label()'s 'skipped (already exists)'"
            );
        }

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn render_init_template_mentions_detected_python_and_nextjs_markers() {
        let root = temp_dir();
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join("pyproject.toml"), "[project]\nname = \"demo\"\n")
            .expect("write pyproject");
        fs::write(
            root.join("package.json"),
            r#"{"dependencies":{"next":"14.0.0","react":"18.0.0"},"devDependencies":{"typescript":"5.0.0"}}"#,
        )
        .expect("write package json");

        let rendered = render_init_claude_md(Path::new(&root));
        assert!(rendered.contains("Languages: Python, TypeScript."));
        assert!(rendered.contains("Frameworks/tooling markers: Next.js, React."));
        assert!(rendered.contains("pyproject.toml"));
        assert!(rendered.contains("Next.js detected"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn force_init_overwrites_existing_claude_md_and_claw_json() {
        // `--force` / `/init-force`: 已存在的 CLAUDE.md 和 .claw.json 应被预置模板
        // 覆盖，状态标记为 Overwritten；.claw/ 目录已存在则保持 Skipped。
        let root = temp_dir();
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join("CLAUDE.md"), "# custom\nuser-authored content\n")
            .expect("write existing claude md");
        fs::write(root.join(".claw.json"), "{\"old\":\"config\"}\n")
            .expect("write existing claw json");

        let report = initialize_repo(&root, true).expect("force init should succeed");
        let rendered = report.render();

        // CLAUDE.md 与 .claw.json 应被覆盖
        assert!(
            rendered.contains("CLAUDE.md        overwritten (forced)"),
            "CLAUDE.md should be overwritten under force, got: {rendered}"
        );
        assert!(
            rendered.contains(".claw.json       overwritten (forced)"),
            ".claw.json should be overwritten under force, got: {rendered}"
        );

        // .claw/ 目录已创建（本次为首次），状态为 Created
        assert!(rendered.contains(".claw/           created"));

        // 文件内容应已替换为预置模板
        let claude_md = fs::read_to_string(root.join("CLAUDE.md")).expect("read overwritten claude md");
        assert!(
            claude_md.contains("# CLAUDE.md"),
            "CLAUDE.md content should be the starter template, got: {claude_md}"
        );
        assert!(!claude_md.contains("user-authored content"));

        let claw_json = fs::read_to_string(root.join(".claw.json")).expect("read overwritten claw json");
        assert!(
            claw_json.contains("\"defaultMode\": \"dontAsk\""),
            ".claw.json should be the starter template, got: {claw_json}"
        );

        // 结构化字段：overwritten[] 应包含两者
        let overwritten = report.artifacts_with_status(InitStatus::Overwritten);
        assert_eq!(
            overwritten,
            vec![".claw.json".to_string(), "CLAUDE.md".to_string()],
            "overwritten[] should list .claw.json and CLAUDE.md"
        );

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn force_init_keeps_skipped_dir_and_handles_idempotent_force() {
        // 二次 force init：.claw/ 目录已存在 → Skipped；CLAUDE.md/.claw.json 再次 Overwritten。
        let root = temp_dir();
        fs::create_dir_all(&root).expect("create root");

        // 第一次 force init（文件不存在，Created）
        let first = initialize_repo(&root, true).expect("first force init");
        assert!(first.artifacts_with_status(InitStatus::Created).len() >= 3);

        // 第二次 force init（目录 Skipped，文件 Overwritten）
        let second = initialize_repo(&root, true).expect("second force init");
        let skipped = second.artifacts_with_status(InitStatus::Skipped);
        assert!(
            skipped.contains(&".claw/".to_string()),
            ".claw/ should be Skipped on second force init, got skipped={skipped:?}"
        );
        let overwritten = second.artifacts_with_status(InitStatus::Overwritten);
        assert_eq!(
            overwritten,
            vec![".claw.json".to_string(), "CLAUDE.md".to_string()],
            "second force init should overwrite .claw.json and CLAUDE.md"
        );

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }
}
