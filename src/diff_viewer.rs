use std::io::Write;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::git::{self, VcsKind};
use crate::tmux;

pub const DIFF_VIEWER_OPTION: &str = "@sidebar_diff_viewer_command";
const PATCH_TARGET_OPTION: &str = "@sidebar_diff_patch_target";

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
    let target = PatchTarget {
        kind: kind.label().to_ascii_lowercase(),
        root,
    };
    let encoded = serde_json::to_string(&target).map_err(|e| e.to_string())?;
    tmux::run_tmux_capture(&["set", "-g", PATCH_TARGET_OPTION, &encoded])?;
    let executable =
        std::env::current_exe().map_err(|e| format!("cannot resolve sidebar binary: {e}"))?;
    let command = format!("{} view-patch", shell_quote(&executable.to_string_lossy()));
    tmux::run_tmux_capture(&["display-popup", "-E", "-w", "100%", "-h", "100%", &command])
        .map(|_| ())
}

pub fn cmd_view_patch() -> i32 {
    let target: PatchTarget =
        match tmux::get_option(PATCH_TARGET_OPTION).and_then(|v| serde_json::from_str(&v).ok()) {
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
    let patch = match git::vcs_patch(kind, &target.root) {
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
        && stdin.write_all(patch.as_bytes()).is_err()
    {
        return 1;
    }
    child
        .wait()
        .map(|status| status.code().unwrap_or(1))
        .unwrap_or(1)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\\"'\\\"'"))
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
}
