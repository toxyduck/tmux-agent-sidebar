use std::io::Write;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::git::{self, VcsKind};
use crate::tmux;

pub const DIFF_VIEWER_OPTION: &str = "@sidebar_diff_viewer_command";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PatchTarget {
    kind: String,
    root: String,
}

pub fn parse_viewer_command(value: &str) -> Result<Vec<String>, String> {
    let argv: Vec<String> = serde_json::from_str(value)
        .map_err(|_| "@sidebar_diff_viewer_command must be a JSON argv array".to_string())?;
    if argv.first().is_none_or(String::is_empty) {
        return Err("@sidebar_diff_viewer_command must name an executable".into());
    }
    Ok(argv)
}

pub fn open_popup(kind: VcsKind, root: String) -> Result<(), String> {
    let config = tmux::get_option(DIFF_VIEWER_OPTION)
        .ok_or_else(|| format!("{DIFF_VIEWER_OPTION} is not configured"))?;
    let viewer = parse_viewer_command(&config)?;
    resolve_executable(&viewer[0])?;
    let target = PatchTarget {
        kind: kind.label().to_ascii_lowercase(),
        root,
    };
    let encoded = serde_json::to_string(&target).map_err(|e| e.to_string())?;
    let executable =
        std::env::current_exe().map_err(|e| format!("cannot resolve sidebar binary: {e}"))?;
    let command = format!(
        "{} view-patch {}",
        shell_quote(&executable.to_string_lossy()),
        shell_quote(&encoded)
    );
    tmux::run_tmux_capture(&["display-popup", "-E", "-w", "100%", "-h", "100%", &command])
        .map(|_| ())
}

pub fn cmd_view_patch(args: &[String]) -> i32 {
    let target: PatchTarget = match args.first().and_then(|v| serde_json::from_str(v).ok()) {
        Some(target) => target,
        None => {
            eprintln!("No diff target selected");
            return 2;
        }
    };
    let kind = match target.kind.as_str() {
        "git" => VcsKind::Git,
        "arc" => VcsKind::Arc,
        _ => {
            eprintln!("Invalid diff target");
            return 2;
        }
    };
    let argv = match tmux::get_option(DIFF_VIEWER_OPTION)
        .as_deref()
        .ok_or_else(|| format!("{DIFF_VIEWER_OPTION} is not configured"))
        .and_then(parse_viewer_command)
    {
        Ok(argv) => argv,
        Err(error) => {
            eprintln!("{error}");
            return 2;
        }
    };
    let patch = match git::vcs_patch_bytes(kind, &target.root) {
        Ok(patch) => patch,
        Err(error) => {
            eprintln!("Cannot create patch: {error}");
            return 2;
        }
    };
    let mut child = match Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!("Cannot launch diff viewer: {error}");
            return 127;
        }
    };
    if let Some(mut stdin) = child.stdin.take()
        && stdin.write_all(&patch).is_err()
    {
        return 1;
    }
    child
        .wait()
        .map(|status| status.code().unwrap_or(1))
        .unwrap_or(1)
}

fn resolve_executable(program: &str) -> Result<(), String> {
    if program.contains('/') {
        return executable_file(std::path::Path::new(program))
            .then_some(())
            .ok_or_else(|| format!("diff viewer not found: {program}"));
    }
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(program))
                .find(|path| executable_file(path))
        })
        .map(|_| ())
        .ok_or_else(|| format!("diff viewer not found in PATH: {program}"))
}

#[cfg(unix)]
fn executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && path
            .metadata()
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}
#[cfg(not(unix))]
fn executable_file(path: &std::path::Path) -> bool {
    path.is_file()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_hunk_argv() {
        assert_eq!(
            parse_viewer_command(r#"["hunk","patch","-"]"#).unwrap(),
            ["hunk", "patch", "-"]
        );
    }
    #[test]
    fn rejects_shell_string() {
        assert!(parse_viewer_command("hunk patch -").is_err());
    }
    #[test]
    fn quotes_single_quote_for_posix_shell() {
        assert_eq!(shell_quote("a'b"), "'a'\"'\"'b'");
    }
    #[cfg(unix)]
    #[test]
    fn rejects_non_executable_viewer() {
        use std::os::unix::fs::PermissionsExt;
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(resolve_executable(file.path().to_str().unwrap()).is_err());
    }
    #[test]
    fn popup_targets_are_serialized_arguments_not_shared_options() {
        let first = serde_json::to_string(&PatchTarget {
            kind: "git".into(),
            root: "/one".into(),
        })
        .unwrap();
        let second = serde_json::to_string(&PatchTarget {
            kind: "arc".into(),
            root: "/two".into(),
        })
        .unwrap();
        assert_ne!(first, second);
    }
}
