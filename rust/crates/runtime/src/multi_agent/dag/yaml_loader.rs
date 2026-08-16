//! YAML 声明式 DAG 定义加载器(设计文档 P0 交付项)。
//!
//! 启动时扫描 `<workspace>/.claw/dags/*.yaml|*.yml`,把每个文件反序列化为
//! [`DagDefineInput`] 并复用 [`build_dag_from_define`] 做校验 + 默认值填充,
//! 与 `dag_define` 工具的运行时注册走同一套逻辑,保证行为一致。
//!
//! 单个文件失败不阻断整体加载(返回错误列表由调用方决定上报方式),
//! 一个坏文件不会阻止其他 DAG 注册。

use std::fs;
use std::path::Path;

use super::{build_dag_from_define, Dag, DagDefineInput};

/// 加载目录下所有 YAML DAG 定义,静默跳过失败文件。
///
/// 目录不存在/不可读时返回空(启动路径的常见情况,不应视为错误)。
pub fn load_dag_definitions(dir: &Path) -> Vec<Dag> {
    load_dag_definitions_with_errors(dir).0
}

/// 加载目录下所有 YAML DAG 定义,返回 DAG 列表 + 失败诊断列表。
pub fn load_dag_definitions_with_errors(dir: &Path) -> (Vec<Dag>, Vec<String>) {
    let mut dags = Vec::new();
    let mut errors = Vec::new();

    let Ok(entries) = fs::read_dir(dir) else {
        return (dags, errors);
    };

    // 确定性顺序:按文件名排序,避免依赖文件系统遍历顺序。
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| is_yaml_file(&e.path()))
        .collect();
    files.sort_by_key(|e| e.file_name());

    for entry in files {
        match load_single_dag(&entry.path()) {
            Ok(dag) => dags.push(dag),
            Err(err) => errors.push(err),
        }
    }

    (dags, errors)
}

/// 加载单个 YAML 文件为一个 `Dag`。
fn load_single_dag(path: &Path) -> Result<Dag, String> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("dag-loader: read {}: {e}", path.display()))?;
    let input: DagDefineInput = serde_yaml::from_str(&raw)
        .map_err(|e| format!("dag-loader: parse {}: {e}", path.display()))?;
    build_dag_from_define(input).map_err(|e| format!("dag-loader: {}: {e}", path.display()))
}

fn is_yaml_file(path: &Path) -> bool {
    path.extension().is_some_and(|ext| {
        let ext = ext.to_string_lossy().to_ascii_lowercase();
        ext == "yaml" || ext == "yml"
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("claw-yaml-loader-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn loads_valid_dag_with_dependencies() {
        let dir = temp_dir("valid");
        fs::write(
            dir.join("pipeline.yaml"),
            r#"
dag_id: yaml-pipeline
name: YAML Pipeline
nodes:
  - id: analyze
    task: Analyze the code
    depends_on: []
  - id: implement
    label: Implement
    task: Implement the fix
    depends_on: [analyze]
    verify_command: cargo test
    max_retries: 3
"#,
        )
        .expect("write yaml");

        let (dags, errors) = load_dag_definitions_with_errors(&dir);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(dags.len(), 1);
        let dag = &dags[0];
        assert_eq!(dag.id, "yaml-pipeline");
        assert_eq!(dag.name, "YAML Pipeline");
        assert_eq!(dag.nodes.len(), 2);
        // 默认值填充:capability→Execute, mode→Fork, max_retries→2
        assert_eq!(dag.nodes[0].id, "analyze");
        assert_eq!(dag.nodes[0].label, "analyze");
        assert_eq!(dag.nodes[0].capability, crate::multi_agent::SubagentCapability::Execute);
        assert_eq!(dag.nodes[0].mode, crate::multi_agent::CoordinationMode::Fork);
        assert_eq!(dag.nodes[1].depends_on, vec!["analyze"]);
        assert_eq!(dag.nodes[1].max_retries, 3);
        assert_eq!(dag.nodes[1].verify_command.as_deref(), Some("cargo test"));

        cleanup(&dir);
    }

    #[test]
    fn missing_dir_yields_empty() {
        let (dags, errors) = load_dag_definitions_with_errors(&Path::new("Z:\\nonexistent-dir-xyz"));
        assert!(dags.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn ignores_non_yaml_files() {
        let dir = temp_dir("ignore");
        fs::write(dir.join("note.txt"), "not a dag").expect("write txt");
        fs::write(dir.join("a.yml"), "dag_id: y\nnodes: [{id: n, task: t}]\n").expect("write yml");

        let (dags, errors) = load_dag_definitions_with_errors(&dir);
        assert!(errors.is_empty());
        assert_eq!(dags.len(), 1);
        assert_eq!(dags[0].id, "y");

        cleanup(&dir);
    }

    #[test]
    fn bad_file_reported_but_others_load() {
        let dir = temp_dir("bad");
        fs::write(dir.join("bad.yaml"), "dag_id: [unclosed\n").expect("write bad");
        // good.yaml 含未知键 dag_id2,serde_yaml 默认忽略未知字段 → 应解析成功
        fs::write(
            dir.join("good.yaml"),
            "dag_id: good\ndag_id2: x\nnodes: [{id: n, task: t}]\n",
        )
        .expect("write good");

        let (dags, errors) = load_dag_definitions_with_errors(&dir);
        // 坏文件被报告,但好文件仍然加载 → 一个坏文件不阻断其他 DAG
        assert_eq!(errors.len(), 1, "bad.yaml should be reported");
        assert!(errors[0].contains("bad.yaml"));
        assert_eq!(dags.len(), 1, "good.yaml should still load");
        assert_eq!(dags[0].id, "good");

        cleanup(&dir);
    }

    #[test]
    fn unknown_dependency_rejected() {
        let dir = temp_dir("unkdep");
        fs::write(
            dir.join("bad.yaml"),
            "dag_id: bad\nnodes: [{id: n, task: t, depends_on: [ghost]}]\n",
        )
        .expect("write");

        let (dags, errors) = load_dag_definitions_with_errors(&dir);
        assert!(dags.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("unknown node 'ghost'"));

        cleanup(&dir);
    }
}
