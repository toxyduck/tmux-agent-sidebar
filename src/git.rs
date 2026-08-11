use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// How long a PR lookup stays fresh before `PrCache` refetches it. PR numbers
/// change only on branch switches (already keyed) or when a new PR is created
/// for the current branch — the TTL bounds the latency of that second case.
pub const PR_CACHE_TTL: Duration = Duration::from_secs(300);

/// A file entry with its status indicator, name, and per-file diff stats.
#[derive(Debug, Clone, PartialEq)]
pub struct GitFileEntry {
    pub status: char,
    pub name: String,
    pub path: String,
    pub additions: usize,
    pub deletions: usize,
}

/// All git information gathered in a single background pass
#[derive(Debug, Clone, Default)]
pub struct GitData {
    pub diff_stat: Option<(usize, usize)>,
    pub branch: String,
    pub ahead_behind: Option<(usize, usize)>,
    pub staged_files: Vec<GitFileEntry>,
    pub unstaged_files: Vec<GitFileEntry>,
    pub untracked_files: Vec<String>,
    pub remote_url: String,
    pub pr_number: Option<String>,
}

/// A repository touched by one or more agent panes. `root` is the VCS root,
/// never a pane cwd, so worktree subdirectories collapse into one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcsEntry {
    pub kind: VcsKind,
    pub root: String,
    pub branch: String,
    pub diff_stat: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcsKind {
    Git,
    Arc,
}

impl VcsKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Git => "Git",
            Self::Arc => "Arc",
        }
    }
}

/// Detect the VCS at `path`. Arc is tested first: an Arc mount can contain a
/// git metadata directory but must never be queried with git commands.
pub fn fetch_vcs_entry(path: &str) -> Option<VcsEntry> {
    if let Some(root) = run_command_text(path, "arc", &["root"]) {
        let branch = run_command_text(&root, "arc", &["status", "--branch", "--json"])
            .and_then(|json| parse_arc_branch_json(&json))
            .or_else(|| {
                run_command_text(&root, "arc", &["status", "--branch"])
                    .and_then(|text| parse_arc_branch_text(&text))
            })
            .unwrap_or_else(|| "HEAD".into());
        // Overlapping index/worktree files are rebuilt from exported HEAD.
        let diff_stat = arc_patch_bytes(&root)
            .map(|patch| patch_stat(&patch))
            .unwrap_or((0, 0));
        return Some(VcsEntry {
            kind: VcsKind::Arc,
            root,
            branch,
            diff_stat: Some(diff_stat),
        });
    }
    let root = run_git(path, &["rev-parse", "--show-toplevel"])?;
    let branch =
        run_git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "HEAD".into());
    let unstaged = run_git(&root, &["diff", "--shortstat"])
        .as_deref()
        .and_then(parse_diff_stat)
        .unwrap_or((0, 0));
    let staged = run_git(&root, &["diff", "--cached", "--shortstat"])
        .as_deref()
        .and_then(parse_diff_stat)
        .unwrap_or((0, 0));
    let untracked = git_untracked_paths(&root)
        .map(|paths| paths.len())
        .unwrap_or(0);
    let diff_stat = Some((unstaged.0 + staged.0 + untracked, unstaged.1 + staged.1));
    Some(VcsEntry {
        kind: VcsKind::Git,
        root,
        branch,
        diff_stat,
    })
}

/// Fetch every distinct VCS root/branch currently represented by agent panes.
pub fn fetch_vcs_entries(paths: impl IntoIterator<Item = String>) -> Vec<VcsEntry> {
    let mut entries: Vec<_> = paths
        .into_iter()
        .filter_map(|path| fetch_vcs_entry(&path))
        .collect();
    entries.sort_by(|a, b| {
        (a.root.as_str(), a.branch.as_str()).cmp(&(b.root.as_str(), b.branch.as_str()))
    });
    entries.dedup_by(|a, b| a.kind == b.kind && a.root == b.root && a.branch == b.branch);
    entries
}

/// Current patch for a repository. Git includes both index and worktree
/// changes; Arc delegates patch generation to `arc diff --git`.
pub fn vcs_patch(kind: VcsKind, root: &str) -> Result<String, String> {
    String::from_utf8(vcs_patch_bytes(kind, root)?).map_err(|_| "patch is not UTF-8".into())
}

/// Return the exact bytes supplied by the VCS. No output is trimmed: a final
/// newline is part of a unified diff and must reach the viewer unchanged.
pub fn vcs_patch_bytes(kind: VcsKind, root: &str) -> Result<Vec<u8>, String> {
    match kind {
        VcsKind::Git => {
            let mut patch = git_output(
                root,
                &["diff", "HEAD", "--no-color", "--binary", "--"],
                false,
            )?
            .stdout;
            for path in git_untracked_paths(root)? {
                // `--no-index` intentionally returns 1 when a diff exists.
                patch.extend(
                    git_output(
                        root,
                        &[
                            "diff",
                            "--no-index",
                            "--no-color",
                            "--binary",
                            "/dev/null",
                            &path,
                        ],
                        true,
                    )?
                    .stdout,
                );
            }
            Ok(patch)
        }
        VcsKind::Arc => arc_patch_bytes(root),
    }
}

fn command_output(
    path: &str,
    program: &str,
    args: &[&str],
    allow_diff_exit: bool,
) -> Result<Output, String> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot launch {program}: {error}"))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            let status = child.wait().map_err(|error| error.to_string())?;
            let stdout = stdout_reader
                .join()
                .map_err(|_| "stdout reader panicked".to_string())?
                .map_err(|error| error.to_string())?;
            let stderr = stderr_reader
                .join()
                .map_err(|_| "stderr reader panicked".to_string())?
                .map_err(|error| error.to_string())?;
            let output = Output {
                status,
                stdout,
                stderr,
            };
            if output.status.success() || (allow_diff_exit && output.status.code() == Some(1)) {
                return Ok(output);
            }
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(format!("{program} timed out"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Arc accepts Unix filenames as raw OS strings. This is deliberately kept
/// beside the byte-path snapshot path rather than converting through UTF-8.
fn command_output_os(path: &str, program: &str, args: &[OsString]) -> Result<Output, String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(path)
        .output()
        .map_err(|error| format!("cannot launch {program}: {error}"))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn run_command_text(path: &str, program: &str, args: &[&str]) -> Option<String> {
    let output = command_output(path, program, args, false).ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn git_output(path: &str, args: &[&str], allow_diff_exit: bool) -> Result<Output, String> {
    command_output(path, "git", args, allow_diff_exit)
}

fn arc_patch_bytes(root: &str) -> Result<Vec<u8>, String> {
    arc_patch_bytes_with_program(root, "arc")
}

fn arc_diff_args(cached: bool) -> &'static [&'static str] {
    if cached {
        &["diff", "--cached", "--no-color", "--git", "--binary"]
    } else {
        &["diff", "--no-color", "--git", "--binary"]
    }
}

fn arc_patch_bytes_with_program(root: &str, program: &str) -> Result<Vec<u8>, String> {
    // Arc documents the index and worktree forms separately. Keep their
    // native sections where disjoint; rebuild an overlap from exported HEAD
    // and the final filesystem state so one file is emitted exactly once.
    let cached = command_output(root, program, arc_diff_args(true), false)
        .map_err(|error| format!("cannot create Arc cached patch: {error}"))?
        .stdout;
    let worktree = command_output(root, program, arc_diff_args(false), false)
        .map_err(|error| format!("cannot create Arc worktree patch: {error}"))?
        .stdout;
    let cached_sections = patch_sections(&cached);
    let worktree_sections = patch_sections(&worktree);
    let overlap = overlapping_snapshots(&cached_sections, &worktree_sections);
    let mut patch = Vec::new();
    for section in cached_sections.into_iter().chain(worktree_sections) {
        if section
            .path
            .as_ref()
            .is_none_or(|path| !overlap.iter().any(|snapshot| snapshot.matches(path)))
        {
            patch.extend(section.bytes);
        }
    }
    if !overlap.is_empty() {
        patch.extend(arc_snapshot_patch(root, program, &overlap)?);
    }
    for path in arc_untracked_paths_with_program(root, program)? {
        patch.extend(arc_untracked_patch(root, &path)?);
    }
    Ok(patch)
}

#[derive(Debug)]
struct PatchSection {
    path: Option<PatchPaths>,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PatchPaths {
    old: Option<Vec<u8>>,
    new: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
struct SnapshotPath {
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
}

impl SnapshotPath {
    fn matches(&self, paths: &PatchPaths) -> bool {
        [paths.old.as_ref(), paths.new.as_ref()]
            .into_iter()
            .flatten()
            .any(|path| self.before.as_ref() == Some(path) || self.after.as_ref() == Some(path))
    }
}

fn overlapping_snapshots(cached: &[PatchSection], worktree: &[PatchSection]) -> Vec<SnapshotPath> {
    let mut snapshots = Vec::new();
    for index in cached.iter().filter_map(|section| section.path.as_ref()) {
        for final_change in worktree.iter().filter_map(|section| section.path.as_ref()) {
            let index_final = index.new.as_ref().or(index.old.as_ref());
            let worktree_identity = final_change.old.as_ref().or(final_change.new.as_ref());
            if index_final.is_some() && index_final == worktree_identity {
                snapshots.push(SnapshotPath {
                    before: index.old.clone(),
                    after: final_change.new.clone().or(final_change.old.clone()),
                });
            }
        }
    }
    snapshots
}

fn patch_sections(patch: &[u8]) -> Vec<PatchSection> {
    let starts = patch
        .windows(11)
        .enumerate()
        .filter_map(|(index, window)| {
            (index == 0 || patch[index - 1] == b'\n')
                .then_some(window)
                .filter(|window| *window == b"diff --git ")
                .map(|_| index)
        })
        .collect::<Vec<_>>();
    if starts.is_empty() {
        return if patch.is_empty() {
            Vec::new()
        } else {
            vec![PatchSection {
                path: None,
                bytes: patch.to_vec(),
            }]
        };
    }
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            let end = starts.get(index + 1).copied().unwrap_or(patch.len());
            let bytes = patch[*start..end].to_vec();
            PatchSection {
                path: patch_section_paths(&bytes),
                bytes,
            }
        })
        .collect()
}

fn patch_section_paths(section: &[u8]) -> Option<PatchPaths> {
    let line = section.split(|byte| *byte == b'\n').next()?;
    let value = line.strip_prefix(b"diff --git ")?;
    let old = parse_git_path_token(value)?;
    let remainder = &value[git_path_token_len(value)?..];
    let new = parse_git_path_token(remainder.strip_prefix(b" ")?)?;
    let mut paths = PatchPaths {
        old: (old != b"/dev/null").then(|| old.strip_prefix(b"a/").unwrap_or(&old).to_vec()),
        new: (new != b"/dev/null").then(|| new.strip_prefix(b"b/").unwrap_or(&new).to_vec()),
    };
    if section.windows(15).any(|line| line == b"\n--- /dev/null\n") {
        paths.old = None;
    }
    if section.windows(15).any(|line| line == b"\n+++ /dev/null\n") {
        paths.new = None;
    }
    Some(paths)
}

fn git_path_token_len(value: &[u8]) -> Option<usize> {
    if value.first() != Some(&b'"') {
        return value.iter().position(|byte| *byte == b' ');
    }
    let mut escaped = false;
    for (index, byte) in value.iter().enumerate().skip(1) {
        if !escaped && *byte == b'"' {
            return Some(index + 1);
        }
        escaped = !escaped && *byte == b'\\';
    }
    None
}

fn parse_git_path_token(value: &[u8]) -> Option<Vec<u8>> {
    if value.first() != Some(&b'"') {
        return Some(value.split(|byte| *byte == b' ').next()?.to_vec());
    }
    let mut result = Vec::new();
    let mut index = 1;
    while index < value.len() {
        match value[index] {
            b'"' => return Some(result),
            b'\\' if index + 1 < value.len() => {
                index += 1;
                match value[index] {
                    b'n' => result.push(b'\n'),
                    b't' => result.push(b'\t'),
                    b'\\' | b'"' => result.push(value[index]),
                    digit @ b'0'..=b'7' if index + 2 < value.len() => {
                        let next = &value[index..index + 3];
                        if next.iter().all(|byte| matches!(byte, b'0'..=b'7')) {
                            result.push(
                                ((digit - b'0') << 6) | ((next[1] - b'0') << 3) | (next[2] - b'0'),
                            );
                            index += 2;
                        } else {
                            return None;
                        }
                    }
                    _ => return None,
                }
            }
            byte => result.push(byte),
        }
        index += 1;
    }
    None
}

fn arc_snapshot_patch(
    root: &str,
    program: &str,
    paths: &[SnapshotPath],
) -> Result<Vec<u8>, String> {
    let before = secure_tempdir("tmux-agent-sidebar-arc-before-")?;
    let after = secure_tempdir("tmux-agent-sidebar-arc-after-")?;
    let before_paths = paths
        .iter()
        .filter_map(|path| path.before.as_deref())
        .map(|path| safe_arc_relative_path_bytes(root, path))
        .collect::<Result<Vec<_>, _>>()?;
    if !before_paths.is_empty() {
        let mut args = vec![OsString::from("export"), OsString::from("HEAD")];
        args.extend(
            paths
                .iter()
                .filter_map(|path| path.before.as_deref())
                .map(os_string_from_bytes),
        );
        args.push(OsString::from("--to"));
        args.push(before.path().as_os_str().to_os_string());
        command_output_os(root, program, &args)
            .map_err(|error| format!("cannot export Arc HEAD snapshot: {error}"))?;
    }
    for path in paths.iter().filter_map(|path| path.after.as_deref()) {
        let source = safe_arc_relative_path_bytes(root, path)?;
        if !source.exists() && fs::symlink_metadata(&source).is_err() {
            continue;
        }
        let destination = after.path().join(
            source
                .strip_prefix(root)
                .map_err(|error| error.to_string())?,
        );
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        copy_snapshot_file(&source, &destination)?;
    }
    let output = git_output(
        root,
        &[
            "diff",
            "--no-index",
            "--no-color",
            "--binary",
            "--",
            before.path().to_str().ok_or("non-UTF8 temp path")?,
            after.path().to_str().ok_or("non-UTF8 temp path")?,
        ],
        true,
    )
    .map_err(|error| format!("cannot format Arc snapshot patch with git: {error}"))?;
    Ok(normalize_snapshot_prefixes(
        output.stdout,
        before.path(),
        after.path(),
    ))
}

fn secure_tempdir(prefix: &str) -> Result<tempfile::TempDir, String> {
    let dir = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(dir.path())
            .map_err(|error| error.to_string())?
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(dir.path(), permissions).map_err(|error| error.to_string())?;
    }
    Ok(dir)
}

#[cfg(unix)]
fn os_string_from_bytes(path: &[u8]) -> OsString {
    OsString::from_vec(path.to_vec())
}

#[cfg(not(unix))]
fn os_string_from_bytes(path: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(path).into_owned())
}

fn safe_arc_relative_path_bytes(root: &str, path: &[u8]) -> Result<PathBuf, String> {
    #[cfg(unix)]
    let path = PathBuf::from(os_string_from_bytes(path));
    #[cfg(not(unix))]
    let path = PathBuf::from(String::from_utf8_lossy(path).into_owned());
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(format!("unsafe Arc untracked path: {:?}", path));
    }
    let root = fs::canonicalize(root).map_err(|error| error.to_string())?;
    let candidate = root.join(path);
    let parent = candidate
        .parent()
        .ok_or_else(|| "untracked path has no parent".to_string())?;
    let parent = fs::canonicalize(parent).map_err(|error| error.to_string())?;
    if !parent.starts_with(&root) {
        return Err("Arc untracked path escapes mount".to_string());
    }
    Ok(candidate)
}

fn copy_snapshot_file(source: &Path, destination: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    if metadata.file_type().is_symlink() {
        std::os::unix::fs::symlink(
            fs::read_link(source).map_err(|error| error.to_string())?,
            destination,
        )
        .map_err(|error| error.to_string())?;
        return Ok(());
    }
    if metadata.is_file() {
        fs::copy(source, destination).map_err(|error| error.to_string())?;
        fs::set_permissions(destination, metadata.permissions())
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn normalize_snapshot_prefixes(patch: Vec<u8>, before: &Path, after: &Path) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(patch.len());
    for line in patch.split_inclusive(|byte| *byte == b'\n') {
        if [
            b"diff --git ".as_slice(),
            b"--- ",
            b"+++ ",
            b"rename from ",
            b"rename to ",
            b"copy from ",
            b"copy to ",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
        {
            normalized.extend(normalize_snapshot_header(line, before, after));
        } else {
            normalized.extend_from_slice(line);
        }
    }
    normalized
}

fn normalize_snapshot_header(line: &[u8], before: &Path, after: &Path) -> Vec<u8> {
    let mut line = line.to_vec();
    for (prefix, path) in [(b'a', before), (b'b', before), (b'a', after), (b'b', after)] {
        let mut needle = vec![prefix];
        needle.extend_from_slice(path.as_os_str().as_encoded_bytes());
        needle.push(b'/');
        let replacement = [prefix, b'/'];
        let mut normalized = Vec::with_capacity(line.len());
        let mut rest = line.as_slice();
        while let Some(index) = rest
            .windows(needle.len())
            .position(|window| window == needle)
        {
            normalized.extend_from_slice(&rest[..index]);
            normalized.extend_from_slice(&replacement);
            rest = &rest[index + needle.len()..];
        }
        normalized.extend_from_slice(rest);
        line = normalized;
    }
    line
}

fn git_untracked_paths(root: &str) -> Result<Vec<String>, String> {
    let output = git_output(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        false,
    )?;
    let fields = output.stdout.split(|byte| *byte == 0);
    Ok(fields
        .filter(|field| field.starts_with(b"?? "))
        .map(|field| String::from_utf8_lossy(&field[3..]).into_owned())
        .collect())
}

fn arc_untracked_paths_with_program(root: &str, program: &str) -> Result<Vec<String>, String> {
    let status = command_output(root, program, &["status", "--json"], false)?;
    let value: serde_json::Value =
        serde_json::from_slice(&status.stdout).map_err(|error| error.to_string())?;
    let mut paths = BTreeSet::new();
    collect_arc_untracked(&value, false, &mut paths);
    Ok(paths
        .into_iter()
        .filter(|path| safe_arc_relative_path(root, path).is_ok())
        .collect())
}

/// Only traverse Arc's documented status containers. Arbitrary JSON strings
/// must not be mistaken for repository paths.
fn collect_arc_untracked(
    value: &serde_json::Value,
    in_untracked: bool,
    paths: &mut BTreeSet<String>,
) {
    match value {
        serde_json::Value::String(path) if in_untracked => {
            paths.insert(path.clone());
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_arc_untracked(value, in_untracked, paths);
            }
        }
        serde_json::Value::Object(object) => {
            let is_untracked = object
                .get("status")
                .or_else(|| object.get("code"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|status| status.eq_ignore_ascii_case("untracked") || status == "??");
            if in_untracked || is_untracked {
                for key in ["path", "file", "name"] {
                    if let Some(path) = object.get(key).and_then(serde_json::Value::as_str) {
                        paths.insert(path.to_owned());
                    }
                }
            }
            for key in ["untracked", "files", "entries", "changes", "status"] {
                if let Some(value) = object.get(key) {
                    collect_arc_untracked(value, in_untracked || key == "untracked", paths);
                }
            }
        }
        _ => {}
    }
}

fn safe_arc_relative_path(root: &str, path: &str) -> Result<PathBuf, String> {
    let relative = Path::new(path);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(format!("unsafe Arc untracked path: {path:?}"));
    }
    let root = fs::canonicalize(root).map_err(|error| error.to_string())?;
    let candidate = root.join(relative);
    let parent = candidate
        .parent()
        .ok_or_else(|| "untracked path has no parent".to_string())?;
    let parent = fs::canonicalize(parent).map_err(|error| error.to_string())?;
    if !parent.starts_with(&root) {
        return Err(format!("Arc untracked path escapes mount: {path:?}"));
    }
    Ok(candidate)
}

fn arc_untracked_patch(root: &str, path: &str) -> Result<Vec<u8>, String> {
    let file = safe_arc_relative_path(root, path)?;
    let metadata = fs::symlink_metadata(&file)
        .map_err(|error| format!("cannot read Arc untracked {path}: {error}"))?;
    #[cfg(unix)]
    let (mode, content) = if metadata.file_type().is_symlink() {
        (
            "120000",
            std::fs::read_link(&file)
                .map_err(|error| error.to_string())?
                .as_os_str()
                .as_encoded_bytes()
                .to_vec(),
        )
    } else if metadata.is_file() {
        let mode = if metadata.permissions().mode() & 0o111 != 0 {
            "100755"
        } else {
            "100644"
        };
        (mode, fs::read(&file).map_err(|error| error.to_string())?)
    } else {
        return Ok(Vec::new());
    };
    #[cfg(not(unix))]
    let (mode, content) = if metadata.is_file() {
        (
            "100644",
            fs::read(&file).map_err(|error| error.to_string())?,
        )
    } else {
        return Ok(Vec::new());
    };
    Ok(new_file_patch(path.as_bytes(), mode, &content))
}

fn git_quote_path(path: &[u8], prefix: &[u8]) -> Vec<u8> {
    let mut full = prefix.to_vec();
    full.extend(path);
    let quote = full
        .iter()
        .any(|&byte| !matches!(byte, b'!'..=b'~') || matches!(byte, b'"' | b'\\'));
    if !quote {
        return full;
    }
    let mut quoted = vec![b'"'];
    for byte in full {
        match byte {
            b'"' | b'\\' => {
                quoted.push(b'\\');
                quoted.push(byte);
            }
            b'\n' => quoted.extend(b"\\n"),
            b'\t' => quoted.extend(b"\\t"),
            byte if matches!(byte, b'!'..=b'~') => quoted.push(byte),
            byte => quoted.extend(format!("\\{:03o}", byte).bytes()),
        }
    }
    quoted.push(b'"');
    quoted
}

fn new_file_patch(path: &[u8], mode: &str, content: &[u8]) -> Vec<u8> {
    let a = git_quote_path(path, b"a/");
    let b = git_quote_path(path, b"b/");
    let mut patch = b"diff --git ".to_vec();
    patch.extend(&a);
    patch.push(b' ');
    patch.extend(&b);
    patch.push(b'\n');
    patch.extend(format!("new file mode {mode}\n").bytes());
    patch.extend(b"--- /dev/null\n+++ ");
    patch.extend(&b);
    patch.push(b'\n');
    if content.contains(&0) || std::str::from_utf8(content).is_err() {
        patch.extend(b"Binary files /dev/null and ");
        patch.extend(&b);
        patch.extend(b" differ\n");
        return patch;
    }
    if content.is_empty() {
        return patch;
    }
    let lines = content.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    let ends_newline = content.ends_with(b"\n");
    let count = lines.len() - usize::from(ends_newline);
    patch.extend(format!("@@ -0,0 +1,{count} @@\n").bytes());
    for line in lines.into_iter().take(count) {
        patch.push(b'+');
        patch.extend(line);
        patch.push(b'\n');
    }
    if !ends_newline {
        patch.extend(b"\\ No newline at end of file\n");
    }
    patch
}

fn patch_stat(patch: &[u8]) -> (usize, usize) {
    let mut additions = 0;
    let mut deletions = 0;
    for line in patch.split(|byte| *byte == b'\n') {
        if line.starts_with(b"+++") || line.starts_with(b"---") {
            continue;
        }
        if line.starts_with(b"+") {
            additions += 1;
        }
        if line.starts_with(b"-") {
            deletions += 1;
        }
    }
    (additions, deletions)
}

pub(crate) fn parse_arc_branch_json(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    ["branch", "current_branch", "currentBranch"]
        .into_iter()
        .find_map(|key| {
            value
                .get(key)
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        })
        .filter(|branch| !branch.is_empty())
}

pub(crate) fn parse_arc_branch_text(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("* ")
            .or_else(|| line.trim().strip_prefix("current: "))
            .map(str::to_owned)
    })
}

impl GitData {
    pub fn changed_file_count(&self) -> usize {
        self.staged_files.len() + self.unstaged_files.len() + self.untracked_files.len()
    }
}

/// Fetch all git data for a given path. Runs blocking subprocess calls.
/// Designed to be called from a background thread.
pub fn fetch_git_data(path: &str) -> GitData {
    let mut data = GitData::default();

    // Parse git status --short to classify files into staged/unstaged/untracked
    if let Some(text) = run_git(path, &["status", "--short"]) {
        parse_status_short(&text, &mut data);
    }

    if let Some(text) = run_git(path, &["diff", "--shortstat"]) {
        data.diff_stat = parse_diff_stat(&text);
    }

    if let Some(text) = run_git(path, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        data.branch = text;
    }

    if let Some(text) = run_git(
        path,
        &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
    ) {
        let parts: Vec<&str> = text.split('\t').collect();
        if parts.len() == 2 {
            let ahead = parts[0].parse().unwrap_or(0);
            let behind = parts[1].parse().unwrap_or(0);
            data.ahead_behind = Some((ahead, behind));
        }
    }

    apply_numstat(
        path,
        &["diff", "--cached", "--numstat"],
        &mut data.staged_files,
    );
    apply_numstat(path, &["diff", "--numstat"], &mut data.unstaged_files);

    if let Some(text) = run_git(path, &["remote", "get-url", "origin"]) {
        data.remote_url = normalize_git_url(&text);
    }

    data
}

/// Fetch the PR number for the current branch at `path` via `gh pr view`.
/// Returns `None` when there is no PR, `gh` is missing, or the call fails or
/// times out. Bounded by a 5s deadline so a hung `gh` cannot stall the git
/// polling thread.
pub fn fetch_pr_number(path: &str) -> Option<String> {
    let mut child = Command::new("gh")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["pr", "view", "--json", "number", "-q", ".number"])
        .current_dir(path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success()
                    && let Some(stdout) = child.stdout.take()
                {
                    use std::io::Read;
                    let mut buf = String::new();
                    let mut reader = stdout;
                    let _ = reader.read_to_string(&mut buf);
                    let num = buf.trim().to_string();
                    if !num.is_empty() {
                        return Some(num);
                    }
                }
                return None;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}

/// Single-slot PR-number cache keyed by `(path, branch)` with a TTL. The git
/// poll thread owns one instance and calls [`PrCache::get_or_fetch`] every
/// tick; same-key repeat calls within `PR_CACHE_TTL` return cached values
/// without hitting `gh`.
#[derive(Debug, Default)]
pub struct PrCache {
    entry: Option<PrCacheEntry>,
}

#[derive(Debug)]
struct PrCacheEntry {
    path: String,
    branch: String,
    pr_number: Option<String>,
    cached_at: Instant,
}

impl PrCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached PR number for `(path, branch)` when fresh, otherwise
    /// call `fetcher`, store its result, and return it. Short-circuits to
    /// `None` without invoking the fetcher or touching the cache when the
    /// branch is unset (empty string) or detached (`git rev-parse
    /// --abbrev-ref HEAD` prints the literal `HEAD` in that state) — a PR is
    /// inherently branch-scoped, so there is no meaningful cache key.
    pub fn get_or_fetch<F>(
        &mut self,
        path: &str,
        branch: &str,
        now: Instant,
        fetcher: F,
    ) -> Option<String>
    where
        F: FnOnce(&str) -> Option<String>,
    {
        if branch.is_empty() || branch == "HEAD" {
            return None;
        }
        let fresh = self.entry.as_ref().is_some_and(|e| {
            e.path == path && e.branch == branch && now.duration_since(e.cached_at) < PR_CACHE_TTL
        });
        if fresh {
            return self.entry.as_ref().and_then(|e| e.pr_number.clone());
        }
        let pr = fetcher(path);
        self.entry = Some(PrCacheEntry {
            path: path.to_string(),
            branch: branch.to_string(),
            pr_number: pr.clone(),
            cached_at: now,
        });
        pr
    }
}

/// Parse `git status --short` output into staged/unstaged/untracked categories.
///
/// Each line has the format `XY filename` where:
/// - X = index (staged) status
/// - Y = worktree (unstaged) status
/// - `??` = untracked
pub(crate) fn parse_status_short(text: &str, data: &mut GitData) {
    for line in text.lines() {
        if line.len() < 3 {
            continue;
        }
        let x = line.as_bytes()[0] as char;
        let y = line.as_bytes()[1] as char;
        // Handle renames: "R  old -> new" format
        let raw_name = &line[3..];
        let full_path = normalize_git_path(raw_name);
        let is_dir = full_path.ends_with('/');
        let name_trimmed = full_path.trim_end_matches('/');
        let mut basename = name_trimmed
            .rsplit('/')
            .next()
            .unwrap_or(name_trimmed)
            .to_string();
        if is_dir {
            basename.push('/');
        }

        if x == '?' && y == '?' {
            data.untracked_files.push(basename);
            continue;
        }

        // Staged: X is M, A, D, R, or C
        if matches!(x, 'M' | 'A' | 'D' | 'R' | 'C') {
            let status = if x == 'R' || x == 'C' { 'M' } else { x };
            data.staged_files.push(GitFileEntry {
                status,
                name: basename.clone(),
                path: full_path.clone(),
                additions: 0,
                deletions: 0,
            });
        }

        // Unstaged: Y is M or D
        if matches!(y, 'M' | 'D') {
            data.unstaged_files.push(GitFileEntry {
                status: y,
                name: basename,
                path: full_path,
                additions: 0,
                deletions: 0,
            });
        }
    }
}

/// Apply numstat diff data to a list of file entries.
fn apply_numstat(path: &str, args: &[&str], entries: &mut [GitFileEntry]) {
    if let Some(text) = run_git(path, args) {
        let numstat = parse_numstat(&text);
        for entry in entries {
            if let Some((add, del)) = numstat
                .get(entry.path.as_str())
                .or_else(|| numstat.get(entry.name.as_str()))
            {
                entry.additions = *add;
                entry.deletions = *del;
            }
        }
    }
}

/// Parse `git diff --numstat` output into a map of filename -> (additions, deletions).
fn parse_numstat(text: &str) -> std::collections::HashMap<String, (usize, usize)> {
    let mut map = std::collections::HashMap::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 3 {
            let add: usize = parts[0].parse().unwrap_or(0);
            let del: usize = parts[1].parse().unwrap_or(0);
            let path = normalize_git_path(parts[2]);
            let basename = path.rsplit('/').next().unwrap_or(path.as_str()).to_string();
            map.insert(path.clone(), (add, del));
            if basename != path {
                map.insert(basename, (add, del));
            }
        }
    }
    map
}

fn normalize_git_path(path: &str) -> String {
    let path = path.trim();
    if let Some((_, new_path)) = path.rsplit_once(" -> ") {
        new_path.trim().to_string()
    } else if let Some((_, new_path)) = path.rsplit_once(" => ") {
        new_path.trim().to_string()
    } else {
        path.to_string()
    }
}

pub(crate) fn run_git(path: &str, args: &[&str]) -> Option<String> {
    let mut cmd_args = vec!["-C", path];
    cmd_args.extend_from_slice(args);
    let output = Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(&cmd_args)
        .output()
        .ok()?;
    if output.status.success() {
        let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    } else {
        None
    }
}

/// Run a git command in `path` and return stderr on non-zero exit. Used by the
/// worktree spawn/remove flow so the UI can show an actionable error message.
pub fn run_git_capture(path: &str, args: &[&str]) -> Result<String, String> {
    let mut cmd_args = vec!["-C", path];
    cmd_args.extend_from_slice(args);
    let output = Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(&cmd_args)
        .output()
        .map_err(|e| format!("failed to spawn git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            format!("git exited with status {}", output.status)
        } else {
            stderr
        })
    }
}

/// Resolve the top-level directory of the git repository containing `path`.
pub fn repo_root(path: &str) -> Option<String> {
    run_git(path, &["rev-parse", "--show-toplevel"])
}

/// `true` when `<repo>/refs/heads/<branch>` exists, i.e. the branch name is
/// already taken.
pub fn branch_exists(repo: &str, branch: &str) -> bool {
    run_git_capture(
        repo,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok()
}

/// `git worktree add <worktree_path> -b <branch>` from inside `repo`. Errors
/// bubble up with stderr.
pub fn worktree_add(repo: &str, worktree_path: &str, branch: &str) -> Result<(), String> {
    run_git_capture(repo, &["worktree", "add", worktree_path, "-b", branch]).map(|_| ())
}

/// `git worktree remove --force <worktree_path>`. `--force` is used
/// because the sidebar's remove flow only runs when the user explicitly
/// picks "close window + remove worktree" — agent sessions routinely
/// leave untracked state behind and git would otherwise strand the
/// worktree. Users who want to keep the checkout have `w` (window only).
pub fn worktree_remove(repo: &str, worktree_path: &str) -> Result<(), String> {
    run_git_capture(repo, &["worktree", "remove", "--force", worktree_path]).map(|_| ())
}

/// `git branch -D <branch>`. Used by the spawn rollback path to drop
/// the branch ref that `git worktree add -b` just created — removing
/// only the worktree leaves the branch behind, which later spawns
/// would then collide with via `branch_exists`.
pub fn branch_delete(repo: &str, branch: &str) -> Result<(), String> {
    run_git_capture(repo, &["branch", "-D", branch]).map(|_| ())
}

pub(crate) fn parse_diff_stat(text: &str) -> Option<(usize, usize)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut insertions = 0usize;
    let mut deletions = 0usize;
    for part in text.split(',') {
        let part = part.trim();
        if part.contains("insertion") {
            insertions = part
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
        } else if part.contains("deletion") {
            deletions = part
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
        }
    }
    Some((insertions, deletions))
}

pub(crate) fn normalize_git_url(url: &str) -> String {
    let url = url.trim();
    if let Some(rest) = url.strip_prefix("git@") {
        let converted = rest.replace(':', "/");
        let cleaned = converted.strip_suffix(".git").unwrap_or(&converted);
        format!("https://{cleaned}")
    } else if url.starts_with("https://") || url.starts_with("http://") {
        url.strip_suffix(".git").unwrap_or(url).to_string()
    } else {
        url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_diff_stat_both() {
        let result = parse_diff_stat("2 files changed, 4 insertions(+), 2 deletions(-)");
        assert_eq!(result, Some((4, 2)));
    }

    #[test]
    fn parse_diff_stat_insertions_only() {
        let result = parse_diff_stat("1 file changed, 5 insertions(+)");
        assert_eq!(result, Some((5, 0)));
    }

    #[test]
    fn parse_diff_stat_deletions_only() {
        let result = parse_diff_stat("1 file changed, 3 deletions(-)");
        assert_eq!(result, Some((0, 3)));
    }

    #[test]
    fn parse_diff_stat_empty() {
        assert_eq!(parse_diff_stat(""), None);
    }

    #[test]
    fn parse_diff_stat_whitespace() {
        assert_eq!(parse_diff_stat("   "), None);
    }

    #[test]
    fn normalize_git_url_ssh() {
        assert_eq!(
            normalize_git_url("git@github.com:user/repo.git"),
            "https://github.com/user/repo"
        );
    }

    #[test]
    fn normalize_git_url_https_with_git() {
        assert_eq!(
            normalize_git_url("https://github.com/user/repo.git"),
            "https://github.com/user/repo"
        );
    }

    #[test]
    fn normalize_git_url_https_clean() {
        assert_eq!(
            normalize_git_url("https://github.com/user/repo"),
            "https://github.com/user/repo"
        );
    }

    #[test]
    fn normalize_git_url_unknown_format() {
        assert_eq!(normalize_git_url("/local/path/repo"), "/local/path/repo");
    }

    // ─── parse_status_short tests ────────────────────────────────

    #[test]
    fn parse_status_short_staged_modified() {
        let mut data = GitData::default();
        parse_status_short("M  src/app.rs", &mut data);
        assert_eq!(data.staged_files.len(), 1);
        assert_eq!(data.staged_files[0].status, 'M');
        assert_eq!(data.staged_files[0].name, "app.rs");
        assert_eq!(data.staged_files[0].path, "src/app.rs");
        assert!(data.unstaged_files.is_empty());
        assert!(data.untracked_files.is_empty());
    }

    #[test]
    fn parse_status_short_staged_added() {
        let mut data = GitData::default();
        parse_status_short("A  new.rs", &mut data);
        assert_eq!(data.staged_files.len(), 1);
        assert_eq!(data.staged_files[0].status, 'A');
        assert_eq!(data.staged_files[0].name, "new.rs");
    }

    #[test]
    fn parse_status_short_unstaged_modified() {
        let mut data = GitData::default();
        parse_status_short(" M config.toml", &mut data);
        assert!(data.staged_files.is_empty());
        assert_eq!(data.unstaged_files.len(), 1);
        assert_eq!(data.unstaged_files[0].status, 'M');
        assert_eq!(data.unstaged_files[0].name, "config.toml");
    }

    #[test]
    fn parse_status_short_both_staged_and_unstaged() {
        let mut data = GitData::default();
        parse_status_short("MM src/lib.rs", &mut data);
        assert_eq!(data.staged_files.len(), 1);
        assert_eq!(data.staged_files[0].status, 'M');
        assert_eq!(data.unstaged_files.len(), 1);
        assert_eq!(data.unstaged_files[0].status, 'M');
    }

    #[test]
    fn parse_status_short_untracked() {
        let mut data = GitData::default();
        parse_status_short("?? tmp/debug.log", &mut data);
        assert!(data.staged_files.is_empty());
        assert!(data.unstaged_files.is_empty());
        assert_eq!(data.untracked_files, vec!["debug.log"]);
    }

    #[test]
    fn parse_status_short_untracked_directory() {
        let mut data = GitData::default();
        parse_status_short("?? docs/superpowers/specs/", &mut data);
        assert_eq!(data.untracked_files, vec!["specs/"]);
    }

    #[test]
    fn parse_status_short_untracked_top_level_directory() {
        let mut data = GitData::default();
        parse_status_short("?? mydir/", &mut data);
        assert_eq!(data.untracked_files, vec!["mydir/"]);
    }

    #[test]
    fn parse_status_short_deleted() {
        let mut data = GitData::default();
        parse_status_short("D  old.rs", &mut data);
        assert_eq!(data.staged_files.len(), 1);
        assert_eq!(data.staged_files[0].status, 'D');
    }

    #[test]
    fn parse_status_short_unstaged_deleted() {
        let mut data = GitData::default();
        parse_status_short(" D removed.rs", &mut data);
        assert!(data.staged_files.is_empty());
        assert_eq!(data.unstaged_files.len(), 1);
        assert_eq!(data.unstaged_files[0].status, 'D');
    }

    #[test]
    fn parse_status_short_rename() {
        let mut data = GitData::default();
        parse_status_short("R  old.rs -> new.rs", &mut data);
        assert_eq!(data.staged_files.len(), 1);
        assert_eq!(data.staged_files[0].status, 'M'); // renames shown as M
        assert_eq!(data.staged_files[0].name, "new.rs");
        assert_eq!(data.staged_files[0].path, "new.rs");
    }

    #[test]
    fn parse_status_short_same_basename_keeps_distinct_paths() {
        let mut data = GitData::default();
        parse_status_short("M  src/app.rs\nM  tests/app.rs", &mut data);
        assert_eq!(data.staged_files.len(), 2);
        assert_eq!(data.staged_files[0].name, "app.rs");
        assert_eq!(data.staged_files[0].path, "src/app.rs");
        assert_eq!(data.staged_files[1].name, "app.rs");
        assert_eq!(data.staged_files[1].path, "tests/app.rs");
    }

    #[test]
    fn parse_numstat_keys_full_paths_before_basename_fallback() {
        let map = parse_numstat("1\t0\tsrc/app.rs\n2\t1\ttests/app.rs");
        assert_eq!(map.get("src/app.rs"), Some(&(1, 0)));
        assert_eq!(map.get("tests/app.rs"), Some(&(2, 1)));
    }

    #[test]
    fn parse_status_short_multiple_lines() {
        let mut data = GitData::default();
        parse_status_short(
            "M  src/app.rs\nA  src/new.rs\n M config.toml\n?? tmp/log",
            &mut data,
        );
        assert_eq!(data.staged_files.len(), 2); // M staged + A staged
        assert_eq!(data.unstaged_files.len(), 1); // M unstaged
        assert_eq!(data.untracked_files.len(), 1); // ?? untracked
    }

    #[test]
    fn parse_status_short_empty() {
        let mut data = GitData::default();
        parse_status_short("", &mut data);
        assert!(data.staged_files.is_empty());
        assert!(data.unstaged_files.is_empty());
        assert!(data.untracked_files.is_empty());
    }

    // ─── PrCache tests ───────────────────────────────────────────────

    use std::cell::Cell;

    fn counting_fetcher<'a>(
        count: &'a Cell<usize>,
        result: Option<&'static str>,
    ) -> impl FnOnce(&str) -> Option<String> + 'a {
        move |_path: &str| {
            count.set(count.get() + 1);
            result.map(|s| s.to_string())
        }
    }

    #[test]
    fn pr_cache_first_lookup_invokes_fetcher() {
        let mut cache = PrCache::new();
        let calls = Cell::new(0);
        let now = Instant::now();
        let pr = cache.get_or_fetch("/a", "main", now, counting_fetcher(&calls, Some("42")));
        assert_eq!(pr.as_deref(), Some("42"));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn pr_cache_same_key_within_ttl_hits() {
        let mut cache = PrCache::new();
        let calls = Cell::new(0);
        let now = Instant::now();
        cache.get_or_fetch("/a", "main", now, counting_fetcher(&calls, Some("42")));
        let pr = cache.get_or_fetch(
            "/a",
            "main",
            now + Duration::from_secs(1),
            counting_fetcher(&calls, Some("99")),
        );
        assert_eq!(pr.as_deref(), Some("42"), "cached value should be returned");
        assert_eq!(calls.get(), 1, "fetcher must not run on a cache hit");
    }

    #[test]
    fn pr_cache_branch_change_refetches() {
        let mut cache = PrCache::new();
        let calls = Cell::new(0);
        let now = Instant::now();
        cache.get_or_fetch("/a", "main", now, counting_fetcher(&calls, Some("42")));
        let pr = cache.get_or_fetch("/a", "feature", now, counting_fetcher(&calls, Some("77")));
        assert_eq!(pr.as_deref(), Some("77"));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn pr_cache_path_change_refetches() {
        let mut cache = PrCache::new();
        let calls = Cell::new(0);
        let now = Instant::now();
        cache.get_or_fetch("/a", "main", now, counting_fetcher(&calls, Some("42")));
        let pr = cache.get_or_fetch("/b", "main", now, counting_fetcher(&calls, Some("77")));
        assert_eq!(pr.as_deref(), Some("77"));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn pr_cache_ttl_expiry_refetches() {
        let mut cache = PrCache::new();
        let calls = Cell::new(0);
        let now = Instant::now();
        cache.get_or_fetch("/a", "main", now, counting_fetcher(&calls, Some("42")));
        let pr = cache.get_or_fetch(
            "/a",
            "main",
            now + PR_CACHE_TTL + Duration::from_secs(1),
            counting_fetcher(&calls, Some("77")),
        );
        assert_eq!(pr.as_deref(), Some("77"));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn pr_cache_empty_branch_skips_and_does_not_pollute_cache() {
        let mut cache = PrCache::new();
        let calls = Cell::new(0);
        let now = Instant::now();
        // Empty branch → no fetch, None returned.
        let pr = cache.get_or_fetch(
            "/a",
            "",
            now,
            counting_fetcher(&calls, Some("should-not-run")),
        );
        assert!(pr.is_none());
        assert_eq!(calls.get(), 0);
        // Cache must still be empty: a real branch must invoke the fetcher.
        let pr = cache.get_or_fetch("/a", "main", now, counting_fetcher(&calls, Some("42")));
        assert_eq!(pr.as_deref(), Some("42"));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn pr_cache_detached_head_skips_and_does_not_pollute_cache() {
        // `git rev-parse --abbrev-ref HEAD` prints the literal "HEAD" when the
        // working tree is detached. A PR is always branch-scoped so that case
        // must short-circuit just like an empty branch.
        let mut cache = PrCache::new();
        let calls = Cell::new(0);
        let now = Instant::now();
        let pr = cache.get_or_fetch(
            "/a",
            "HEAD",
            now,
            counting_fetcher(&calls, Some("should-not-run")),
        );
        assert!(pr.is_none());
        assert_eq!(calls.get(), 0);
        // Cache must be untouched: the next real branch still triggers a fetch.
        let pr = cache.get_or_fetch("/a", "main", now, counting_fetcher(&calls, Some("42")));
        assert_eq!(pr.as_deref(), Some("42"));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn pr_cache_none_result_is_cached() {
        let mut cache = PrCache::new();
        let calls = Cell::new(0);
        let now = Instant::now();
        // First call returns None (no PR exists) — that None should be cached.
        cache.get_or_fetch("/a", "main", now, counting_fetcher(&calls, None));
        let pr = cache.get_or_fetch(
            "/a",
            "main",
            now + Duration::from_secs(1),
            counting_fetcher(&calls, Some("should-not-run")),
        );
        assert!(pr.is_none());
        assert_eq!(calls.get(), 1, "second call must hit the cached None");
    }

    #[test]
    fn parses_arc_status_branch_json_and_detached_state() {
        assert_eq!(
            parse_arc_branch_json(r#"{"branch":"users/alice/topic"}"#).as_deref(),
            Some("users/alice/topic")
        );
        assert_eq!(parse_arc_branch_json(r#"{"branch":null}"#), None);
        assert_eq!(
            parse_arc_branch_text("  main\n* users/alice/topic\n").as_deref(),
            Some("users/alice/topic")
        );
        assert_eq!(parse_arc_branch_text("detached HEAD\n"), None);
    }

    #[test]
    fn git_patch_contains_staged_unstaged_untracked_and_final_newline() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap()
        };
        assert!(run(&["init", "-q"]).status.success());
        assert!(
            run(&["config", "user.email", "test@example.invalid"])
                .status
                .success()
        );
        assert!(run(&["config", "user.name", "Test"]).status.success());
        std::fs::write(dir.path().join("tracked.txt"), "base\n").unwrap();
        assert!(run(&["add", "tracked.txt"]).status.success());
        assert!(run(&["commit", "-qm", "base"]).status.success());
        std::fs::write(dir.path().join("tracked.txt"), "staged\n").unwrap();
        assert!(run(&["add", "tracked.txt"]).status.success());
        std::fs::write(dir.path().join("tracked.txt"), "unstaged\n").unwrap();
        std::fs::write(dir.path().join("new file.txt"), "untracked\n").unwrap();
        let patch = vcs_patch_bytes(VcsKind::Git, root).unwrap();
        let patch = String::from_utf8(patch).unwrap();
        assert!(patch.contains("--- a/tracked.txt"));
        assert!(patch.contains("new file.txt"));
        assert!(patch.ends_with('\n'));
    }

    #[test]
    fn arc_diff_commands_use_documented_index_and_worktree_forms() {
        assert_eq!(
            arc_diff_args(true),
            ["diff", "--cached", "--no-color", "--git", "--binary"]
        );
        assert_eq!(
            arc_diff_args(false),
            ["diff", "--no-color", "--git", "--binary"]
        );
    }

    #[cfg(unix)]
    fn fake_arc(root: &Path, status_json: &str, diff: &[u8], exit_diff: i32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let program = root.join("fake-arc");
        let diff_file = root.join("diff-output");
        fs::write(&diff_file, diff).unwrap();
        let status_file = root.join("status-output");
        fs::write(&status_file, status_json).unwrap();
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = diff ]; then printf '%s\\n' \"$@\" > {args}; [ \"$2\" = --cached ] && exit 0; cat {diff}; exit {exit_diff}; fi\ncat {status}\n",
            args = root.join("args").display(),
            diff = diff_file.display(),
            status = status_file.display(),
        );
        fs::write(&program, script).unwrap();
        let mut permissions = fs::metadata(&program).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&program, permissions).unwrap();
        program
    }

    #[cfg(unix)]
    #[test]
    fn arc_patch_preserves_raw_stdout_and_fake_argv() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let raw = b"diff --git a/tracked b/tracked\n@@ -1 +1 @@\n-old\n+final\n\xff";
        let fake = fake_arc(root, r#"{"untracked":[]}"#, raw, 0);
        assert_eq!(
            arc_patch_bytes_with_program(root.to_str().unwrap(), fake.to_str().unwrap()).unwrap(),
            raw
        );
        assert_eq!(
            fs::read_to_string(root.join("args")).unwrap(),
            "diff\n--no-color\n--git\n--binary\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn arc_patch_reports_cli_failure() {
        let dir = tempfile::tempdir().unwrap();
        let fake = fake_arc(dir.path(), r#"{"untracked":[]}"#, b"", 7);
        let error =
            arc_patch_bytes_with_program(dir.path().to_str().unwrap(), fake.to_str().unwrap())
                .unwrap_err();
        assert!(error.contains("cannot create Arc"));
    }

    #[cfg(unix)]
    #[test]
    fn arc_overlap_is_emitted_once_from_head_to_final_snapshot() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("tracked.txt"), "final\n").unwrap();
        let program = dir.path().join("fake-arc-overlap");
        let script =
            "#!/bin/sh\nif [ \"$1\" = diff ]; then\n  if [ \"$2\" = --cached ]; then printf '%s' 'diff --git a/tracked.txt b/tracked.txt\n--- a/tracked.txt\n+++ b/tracked.txt\n@@ -1 +1 @@\n-base\n+staged\n'; else printf '%s' 'diff --git a/tracked.txt b/tracked.txt\n--- a/tracked.txt\n+++ b/tracked.txt\n@@ -1 +1 @@\n-staged\n+final\n'; fi\n  exit 0\nfi\nif [ \"$1\" = export ]; then\n  while [ \"$1\" != --to ]; do shift; done; shift; mkdir -p \"$1\"; printf 'base\\n' > \"$1/tracked.txt\"; exit 0\nfi\nprintf '%s' '{\"untracked\":[]}'\n"
                .to_owned();
        fs::write(&program, script).unwrap();
        let mut permissions = fs::metadata(&program).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&program, permissions).unwrap();
        let patch = String::from_utf8(
            arc_patch_bytes_with_program(dir.path().to_str().unwrap(), program.to_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(patch.matches("diff --git ").count(), 1);
        assert!(patch.contains("-base"));
        assert!(patch.contains("+final"));
    }

    #[test]
    fn arc_status_json_variants_deduplicate_untracked_paths() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{"untracked":["a file",{"path":"b\"q"}],"files":[{"status":"??","path":"a file"}],"changes":[{"code":"untracked","name":"c"}],"ignore":"not-a-path"}"#,
        ).unwrap();
        let mut paths = BTreeSet::new();
        collect_arc_untracked(&value, false, &mut paths);
        assert_eq!(
            paths.into_iter().collect::<Vec<_>>(),
            vec!["a file", "b\"q", "c"]
        );
    }

    #[test]
    fn arc_status_rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        assert!(safe_arc_relative_path(dir.path().to_str().unwrap(), "../outside").is_err());
        assert!(safe_arc_relative_path(dir.path().to_str().unwrap(), "/outside").is_err());
    }

    #[test]
    fn arc_patch_sections_parse_quoted_paths() {
        let sections = patch_sections(
            b"diff --git \"a/space \\\"quote\\\"\" \"b/space \\\"quote\\\"\"\n--- \"a/space \\\"quote\\\"\"\n",
        );
        assert_eq!(
            sections[0]
                .path
                .as_ref()
                .and_then(|paths| paths.new.as_deref()),
            Some(b"space \"quote\"".as_slice())
        );
    }

    #[test]
    fn arc_normalizes_git_snapshot_paths() {
        let before = Path::new("/tmp/before");
        let after = Path::new("/tmp/after");
        let patch = normalize_snapshot_prefixes(
            b"diff --git a/tmp/before/file b/tmp/after/file\n--- a/tmp/before/file\n+++ b/tmp/after/file\n".to_vec(),
            before,
            after,
        );
        assert_eq!(patch, b"diff --git a/file b/file\n--- a/file\n+++ b/file\n");
    }

    #[test]
    fn arc_normalizes_only_path_headers_not_hunks_or_binary_payload() {
        let patch = normalize_snapshot_prefixes(
            b"diff --git a/tmp/before/file b/tmp/after/file\n--- a/tmp/before/file\n+++ b/tmp/after/file\n+sentinel /tmp/before/file\nGIT binary patch\nliteral 3\n/tmp/after/file\n".to_vec(),
            Path::new("/tmp/before"),
            Path::new("/tmp/after"),
        );
        assert!(patch.starts_with(b"diff --git a/file b/file\n--- a/file\n+++ b/file\n"));
        assert!(patch.ends_with(
            b"+sentinel /tmp/before/file\nGIT binary patch\nliteral 3\n/tmp/after/file\n"
        ));
    }

    #[test]
    fn arc_patch_sections_keep_non_utf8_paths_as_bytes() {
        let sections = patch_sections(b"diff --git \"a/non\\377utf8\" \"b/non\\377utf8\"\n");
        assert_eq!(
            sections[0]
                .path
                .as_ref()
                .and_then(|paths| paths.new.as_deref()),
            Some(b"non\xffutf8".as_slice())
        );
    }

    #[test]
    fn arc_overlap_new_file_does_not_have_head_snapshot_path() {
        let cached = patch_sections(
            b"diff --git a/new b/new\nnew file mode 100644\n--- /dev/null\n+++ b/new\n",
        );
        let worktree = patch_sections(b"diff --git a/new b/new\n--- a/new\n+++ b/new\n");
        let snapshots = overlapping_snapshots(&cached, &worktree);
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].before, None);
        assert_eq!(snapshots[0].after.as_deref(), Some(b"new".as_slice()));
    }

    #[cfg(unix)]
    #[test]
    fn arc_snapshot_new_file_skips_pathless_export_and_has_no_temp_headers() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("new"), "final\n").unwrap();
        let fake = dir.path().join("fake-arc-no-export");
        fs::write(&fake, "#!/bin/sh\necho unexpected export >&2\nexit 9\n").unwrap();
        let mut permissions = fs::metadata(&fake).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake, permissions).unwrap();
        let patch = String::from_utf8(
            arc_snapshot_patch(
                dir.path().to_str().unwrap(),
                fake.to_str().unwrap(),
                &[SnapshotPath {
                    before: None,
                    after: Some(b"new".to_vec()),
                }],
            )
            .unwrap(),
        )
        .unwrap();
        assert!(patch.contains("diff --git a/new b/new"));
        assert!(!patch.contains("tmux-agent-sidebar-arc-"));
    }

    #[test]
    fn arc_normalizes_new_and_deleted_snapshot_headers() {
        let patch = normalize_snapshot_prefixes(
            b"diff --git a/tmp/after/new b/tmp/after/new\n--- /dev/null\n+++ b/tmp/after/new\ndiff --git a/tmp/before/old b/tmp/before/old\n--- a/tmp/before/old\n+++ /dev/null\n".to_vec(),
            Path::new("/tmp/before"),
            Path::new("/tmp/after"),
        );
        assert_eq!(patch, b"diff --git a/new b/new\n--- /dev/null\n+++ b/new\ndiff --git a/old b/old\n--- a/old\n+++ /dev/null\n");
    }

    #[test]
    fn arc_overlap_rename_uses_old_head_and_new_final_path() {
        let cached = patch_sections(
            b"diff --git a/old b/new\nsimilarity index 100%\nrename from old\nrename to new\n",
        );
        let worktree = patch_sections(b"diff --git a/new b/new\n--- a/new\n+++ b/new\n");
        let snapshots = overlapping_snapshots(&cached, &worktree);
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].before.as_deref(), Some(b"old".as_slice()));
        assert_eq!(snapshots[0].after.as_deref(), Some(b"new".as_slice()));
    }

    #[cfg(unix)]
    #[test]
    fn arc_temp_snapshot_directory_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = secure_tempdir("tmux-agent-sidebar-arc-test-").unwrap();
        assert_eq!(
            fs::metadata(dir.path()).unwrap().permissions().mode() & 0o077,
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn arc_untracked_patches_cover_text_modes_links_binary_and_quoted_paths() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("text space \"quote\""), "line").unwrap();
        fs::write(root.join("empty"), "").unwrap();
        fs::write(root.join("exec"), "#!/bin/sh\n").unwrap();
        let mut permissions = fs::metadata(root.join("exec")).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(root.join("exec"), permissions).unwrap();
        symlink("target", root.join("link")).unwrap();
        fs::write(root.join("binary"), [0, 255]).unwrap();
        let text = String::from_utf8(
            arc_untracked_patch(root.to_str().unwrap(), "text space \"quote\"").unwrap(),
        )
        .unwrap();
        assert!(text.contains("new file mode 100644"));
        assert!(text.contains("\\ No newline at end of file"));
        assert!(text.contains("a/text\\040space"), "{text}");
        assert!(text.contains("\\\"quote\\\""));
        let empty =
            String::from_utf8(arc_untracked_patch(root.to_str().unwrap(), "empty").unwrap())
                .unwrap();
        assert!(empty.contains("new file mode 100644"));
        assert!(!empty.contains("@@"));
        assert!(
            String::from_utf8(arc_untracked_patch(root.to_str().unwrap(), "exec").unwrap())
                .unwrap()
                .contains("new file mode 100755")
        );
        assert!(
            String::from_utf8(arc_untracked_patch(root.to_str().unwrap(), "link").unwrap())
                .unwrap()
                .contains("new file mode 120000")
        );
        assert!(
            String::from_utf8(arc_untracked_patch(root.to_str().unwrap(), "binary").unwrap())
                .unwrap()
                .contains("Binary files /dev/null and b/binary differ")
        );
    }
}
