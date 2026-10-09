//! Container-local working-directory validation and file path resolution.
use anyhow::{Result, ensure};

pub const DEFAULT_WORKDIR: &str = "/workspace";

pub fn validate_workdir(value: &str) -> Result<String> {
    let path = value.strip_suffix('/').unwrap_or(value);
    ensure!(
        path.starts_with('/')
            && path.len() > 1
            && value.len() <= 4096
            && !path.contains('\\')
            && !path.chars().any(char::is_control)
            && path[1..]
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "workdir must be an absolute non-root container path without traversal or control characters"
    );
    ensure!(
        !["/opt/agent", "/.aporto"]
            .iter()
            .any(|reserved| path == *reserved || path.starts_with(&format!("{reserved}/")))
            && !path.starts_with("/tmp/.aporto"),
        "workdir overlaps reserved runtime resources"
    );
    Ok(path.to_owned())
}

pub fn resolve_file(workdir: &str, value: &str) -> Result<String> {
    let root = validate_workdir(workdir)?;
    if value == "." || value == root {
        return Ok(root);
    }
    let prefix = format!("{root}/");
    let relative = value.strip_prefix(&prefix).unwrap_or(value);
    ensure!(
        !relative.is_empty()
            && !relative.starts_with('/')
            && value.len() <= 4096
            && !relative.contains('\\')
            && !relative.chars().any(char::is_control)
            && relative
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "file path must remain inside the configured working directory"
    );
    Ok(format!("{root}/{relative}"))
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_choices_are_container_paths_and_cannot_alias_runtime_resources() {
        assert_eq!(
            validate_workdir("/workspace/project/").unwrap(),
            "/workspace/project"
        );
        assert_eq!(
            validate_workdir("/work/中文 project's files").unwrap(),
            "/work/中文 project's files"
        );
        for path in [
            "",
            "/",
            "project",
            "/../etc",
            "/app/../etc",
            "/app/./src",
            "//workspace",
            "/app//",
            "/opt/agent",
            "/opt/agent/pkg",
            "/.aporto/state",
            "/tmp/.aporto-processes",
            "/a\\b",
            "/a\n",
        ] {
            assert!(validate_workdir(path).is_err(), "{path:?}");
        }
        assert!(validate_workdir(&format!("/{}", "x".repeat(4096))).is_err());
    }

    #[test]
    fn files_and_commands_share_one_root_without_cross_directory_prefix_matches() {
        assert_eq!(
            resolve_file("/app/project", "result.txt").unwrap(),
            "/app/project/result.txt"
        );
        assert_eq!(
            resolve_file("/app/project", "/app/project/src/main.rs").unwrap(),
            "/app/project/src/main.rs"
        );
        for path in [
            "/workspace/a",
            "/app/project-other/a",
            "../a",
            "src/../../a",
            "/opt/agent/a",
        ] {
            assert!(resolve_file("/app/project", path).is_err());
        }
    }
}
