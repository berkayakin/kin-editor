//! Git support built on the `git` command line, so the user's config,
//! credential helpers, SSH keys and hooks all apply. Every function here
//! blocks; call them from a background task.

use std::{
    collections::HashMap,
    io::Write,
    ops::Range,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Change {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

impl Change {
    pub fn letter(self) -> &'static str {
        match self {
            Change::Added => "A",
            Change::Modified => "M",
            Change::Deleted => "D",
            Change::Renamed => "R",
            Change::Untracked => "U",
            Change::Conflicted => "C",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Change::Added => "Added",
            Change::Modified => "Modified",
            Change::Deleted => "Deleted",
            Change::Renamed => "Renamed",
            Change::Untracked => "Untracked",
            Change::Conflicted => "Conflict",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStatus {
    /// Absolute path.
    pub path: PathBuf,
    /// Path relative to the repository root, with forward slashes.
    pub rel: String,
    /// Change recorded in the index (staged), if any.
    pub staged: Option<Change>,
    /// Change in the working tree not yet staged, if any.
    pub unstaged: Option<Change>,
}

impl FileStatus {
    /// The most relevant change for display.
    pub fn primary(&self) -> Change {
        match (self.staged, self.unstaged) {
            (Some(Change::Conflicted), _) | (_, Some(Change::Conflicted)) => Change::Conflicted,
            (_, Some(c)) => c,
            (Some(c), None) => c,
            (None, None) => Change::Modified,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoStatus {
    pub branch: Option<String>,
    pub head_oid: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<FileStatus>,
}

impl RepoStatus {
    pub fn staged(&self) -> impl Iterator<Item = &FileStatus> {
        self.files.iter().filter(|f| f.staged.is_some() && f.staged != Some(Change::Conflicted))
    }

    pub fn unstaged(&self) -> impl Iterator<Item = &FileStatus> {
        self.files.iter().filter(|f| f.unstaged.is_some() || f.staged == Some(Change::Conflicted))
    }

    /// Status per path, plus a "contains changes" mark for every ancestor
    /// directory (used to tint folders in the file tree).
    pub fn path_map(&self, root: &Path) -> HashMap<PathBuf, Change> {
        let mut map = HashMap::new();
        for f in &self.files {
            let change = f.primary();
            map.insert(f.path.clone(), change);
            for dir in f.path.ancestors().skip(1) {
                if dir == root || !dir.starts_with(root) {
                    break;
                }
                let entry = map.entry(dir.to_path_buf()).or_insert(change);
                if change == Change::Conflicted {
                    *entry = Change::Conflicted;
                } else if *entry != Change::Conflicted {
                    *entry = Change::Modified;
                }
            }
        }
        map
    }
}

#[derive(Clone, Debug)]
pub struct Branch {
    pub name: String,
    pub remote: bool,
    pub current: bool,
}

fn command(root: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(root)
        // Never block on an interactive prompt: fail fast instead.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .stdin(Stdio::null());
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// Runs git and returns stdout, or the most useful line of stderr.
pub fn run(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = command(root)
        .args(args)
        .output()
        .map_err(|e| format!("couldn't run git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(error_text(&output.stderr, &output.stdout))
    }
}

fn run_with_stdin(root: &Path, args: &[&str], input: &[u8]) -> Result<String, String> {
    let mut child = command(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't run git: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input).map_err(|e| e.to_string())?;
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(error_text(&output.stderr, &output.stdout))
    }
}

fn error_text(stderr: &[u8], stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(if stderr.is_empty() { stdout } else { stderr }).into_owned();
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("hint:"))
        .collect();
    let line = lines
        .iter()
        .find(|l| l.starts_with("fatal:") || l.starts_with("error:"))
        .or(lines.first())
        .copied()
        .unwrap_or("git failed");
    line.trim_start_matches("fatal: ").trim_start_matches("error: ").to_string()
}

pub fn is_available() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
}

/// The working-tree root of the repository containing `path`.
pub fn discover(path: &Path) -> Option<PathBuf> {
    let dir = if path.is_dir() { path } else { path.parent()? };
    let out = run(dir, &["rev-parse", "--show-toplevel"]).ok()?;
    let root = PathBuf::from(out.trim());
    Some(root.canonicalize().unwrap_or(root))
}

pub fn init(dir: &Path) -> Result<(), String> {
    run(dir, &["init"]).map(drop)
}

pub fn status(root: &Path) -> Result<RepoStatus, String> {
    let out = run(
        root,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
        ],
    )?;
    Ok(parse_status(root, &out))
}

fn change_for(c: char) -> Option<Change> {
    match c {
        'M' | 'T' => Some(Change::Modified),
        'A' | 'C' => Some(Change::Added),
        'D' => Some(Change::Deleted),
        'R' => Some(Change::Renamed),
        'U' => Some(Change::Conflicted),
        _ => None,
    }
}

fn parse_status(root: &Path, out: &str) -> RepoStatus {
    let mut status = RepoStatus::default();
    let mut fields = out.split('\0').filter(|f| !f.is_empty());
    let file = |rel: &str, staged, unstaged| FileStatus {
        path: root.join(rel),
        rel: rel.to_string(),
        staged,
        unstaged,
    };
    while let Some(entry) = fields.next() {
        if let Some(header) = entry.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" if value != "(initial)" => status.head_oid = Some(value.to_string()),
                "branch.head" if value != "(detached)" => status.branch = Some(value.to_string()),
                "branch.upstream" => status.upstream = Some(value.to_string()),
                "branch.ab" => {
                    for part in value.split_whitespace() {
                        if let Some(n) = part.strip_prefix('+') {
                            status.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            status.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let kind = entry.chars().next().unwrap_or(' ');
        match kind {
            '1' | '2' => {
                // "1 XY sub mH mI mW hH hI path" ("2" adds a score before the path).
                let n = if kind == '1' { 8 } else { 9 };
                let parts: Vec<&str> = entry.splitn(n + 1, ' ').collect();
                if parts.len() <= n {
                    continue;
                }
                let xy: Vec<char> = parts[1].chars().collect();
                status.files.push(file(parts[n], change_for(xy[0]), change_for(xy[1])));
                if kind == '2' {
                    fields.next(); // the original path of a rename
                }
            }
            'u' => {
                let parts: Vec<&str> = entry.splitn(11, ' ').collect();
                if let Some(rel) = parts.get(10) {
                    status.files.push(file(rel, Some(Change::Conflicted), Some(Change::Conflicted)));
                }
            }
            '?' => {
                status.files.push(file(&entry[2..], None, Some(Change::Untracked)));
            }
            _ => {}
        }
    }
    status.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    status
}

/// A file's contents at HEAD, or `None` if it isn't in HEAD.
pub fn head_text(root: &Path, rel: &str) -> Option<String> {
    run(root, &["show", &format!("HEAD:{rel}")]).ok()
}

/// A file's contents in the index (staging area).
pub fn index_text(root: &Path, rel: &str) -> Option<String> {
    run(root, &["show", &format!(":{rel}")]).ok()
}

pub fn stage(root: &Path, rels: &[String]) -> Result<(), String> {
    let mut args = vec!["add", "--all", "--"];
    args.extend(rels.iter().map(String::as_str));
    run(root, &args).map(drop)
}

pub fn unstage(root: &Path, rels: &[String], has_head: bool) -> Result<(), String> {
    let mut args: Vec<&str> = if has_head {
        vec!["restore", "--staged", "--"]
    } else {
        // Before the first commit there is nothing to restore from.
        vec!["rm", "--cached", "-r", "--quiet", "--"]
    };
    args.extend(rels.iter().map(String::as_str));
    run(root, &args).map(drop)
}

/// Throws away working-tree changes. Untracked files are deleted.
pub fn discard(root: &Path, files: &[FileStatus]) -> Result<(), String> {
    let (untracked, tracked): (Vec<&FileStatus>, Vec<&FileStatus>) = files
        .iter()
        .partition(|f| f.unstaged == Some(Change::Untracked));
    for f in untracked {
        std::fs::remove_file(&f.path).map_err(|e| format!("{}: {e}", f.rel))?;
    }
    if !tracked.is_empty() {
        let mut args = vec!["restore", "--worktree", "--"];
        args.extend(tracked.iter().map(|f| f.rel.as_str()));
        run(root, &args)?;
    }
    Ok(())
}

pub fn commit(root: &Path, message: &str, amend: bool) -> Result<String, String> {
    let mut args = vec!["commit", "--quiet", "--file", "-"];
    if amend {
        args.push("--amend");
    }
    run_with_stdin(root, &args, message.as_bytes())?;
    Ok(run(root, &["log", "-1", "--format=%h %s"])?.trim().to_string())
}

pub fn last_commit_message(root: &Path) -> Option<String> {
    run(root, &["log", "-1", "--format=%B"]).ok().map(|s| s.trim_end().to_string())
}

pub fn push(root: &Path, set_upstream: Option<&str>) -> Result<String, String> {
    match set_upstream {
        Some(branch) => run(root, &["push", "--porcelain", "--set-upstream", "origin", branch]),
        None => run(root, &["push", "--porcelain"]),
    }
}

pub fn pull(root: &Path) -> Result<String, String> {
    run(root, &["pull", "--ff", "--no-edit"])
}

pub fn fetch(root: &Path) -> Result<String, String> {
    run(root, &["fetch", "--all", "--prune"])
}

pub fn has_remote(root: &Path) -> bool {
    run(root, &["remote"]).is_ok_and(|s| !s.trim().is_empty())
}

pub fn branches(root: &Path) -> Result<Vec<Branch>, String> {
    let out = run(
        root,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(HEAD)\t%(refname)",
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let (head, refname) = line.split_once('\t')?;
            let (name, remote) = if let Some(n) = refname.strip_prefix("refs/heads/") {
                (n, false)
            } else {
                (refname.strip_prefix("refs/remotes/")?, true)
            };
            if remote && name.ends_with("/HEAD") {
                return None;
            }
            Some(Branch {
                name: name.to_string(),
                remote,
                current: head == "*",
            })
        })
        .collect())
}

pub fn checkout(root: &Path, branch: &Branch) -> Result<(), String> {
    if branch.remote {
        // Check out a local tracking branch for the remote one.
        let local = branch.name.split_once('/').map_or(branch.name.as_str(), |(_, n)| n);
        run(root, &["switch", "--track", "-c", local, &branch.name])
            .or_else(|_| run(root, &["switch", local]))
            .map(drop)
    } else {
        run(root, &["switch", &branch.name]).map(drop)
    }
}

pub fn create_branch(root: &Path, name: &str) -> Result<(), String> {
    run(root, &["switch", "-c", name]).map(drop)
}

/// The URL of `origin`, or of the first remote if there's no `origin`.
pub fn remote_url(root: &Path) -> Option<String> {
    let url = run(root, &["remote", "get-url", "origin"]).ok().or_else(|| {
        let first = run(root, &["remote"]).ok()?.lines().next()?.trim().to_string();
        run(root, &["remote", "get-url", &first]).ok()
    })?;
    Some(url.trim().to_string()).filter(|u| !u.is_empty())
}

/// A browsable `https://` page for a remote URL such as
/// `git@github.com:owner/repo.git` or `ssh://git@host:22/owner/repo`.
pub fn web_url(remote: &str) -> Option<String> {
    let remote = remote.trim().trim_end_matches('/');
    let remote = remote.strip_suffix(".git").unwrap_or(remote);
    let (host, path) = if let Some(rest) = remote.strip_prefix("https://").or_else(|| remote.strip_prefix("http://")) {
        let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
        rest.split_once('/')?
    } else if let Some(rest) = remote.strip_prefix("ssh://").or_else(|| remote.strip_prefix("git://")) {
        let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
        let (host, path) = rest.split_once('/')?;
        (host.split(':').next()?, path)
    } else {
        // scp-like: [user@]host:path
        let rest = remote.rsplit_once('@').map_or(remote, |(_, r)| r);
        let (host, path) = rest.split_once(':')?;
        if host.contains('/') {
            return None;
        }
        (host, path)
    };
    (!host.is_empty() && !path.is_empty()).then(|| format!("https://{host}/{}", path.trim_start_matches('/')))
}

/// The folder name `git clone` would pick for a URL ("repo" for
/// `https://host/owner/repo.git`).
pub fn repo_name_from_url(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches(['/', '\\']);
    let url = url.strip_suffix(".git").unwrap_or(url);
    let name = url.rsplit(['/', '\\', ':']).next()?.trim();
    (!name.is_empty() && name != "." && name != "..").then(|| name.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    pub sha: String,
    pub summary: String,
    pub author: String,
    pub time: i64,
}

/// The commit `HEAD` points at, if there is one.
pub fn last_commit(root: &Path) -> Option<CommitInfo> {
    let out = run(root, &["log", "-1", "--format=%h%x1f%s%x1f%an%x1f%ct"]).ok()?;
    let mut parts = out.trim_end().splitn(4, '\x1f');
    Some(CommitInfo {
        sha: parts.next()?.to_string(),
        summary: parts.next()?.to_string(),
        author: parts.next()?.to_string(),
        time: parts.next()?.parse().ok()?,
    })
}

/// A running `git clone`. Progress is read from its stderr; `cancel` stops it.
pub struct CloneJob {
    child: std::sync::Mutex<std::process::Child>,
    stderr: std::sync::Mutex<Option<std::process::ChildStderr>>,
    /// The latest progress line, e.g. "Receiving objects:  45% (450/1000)".
    pub progress: std::sync::Mutex<String>,
    cancelled: std::sync::atomic::AtomicBool,
    dest: PathBuf,
}

/// Starts cloning `url` into `parent/name`. `shallow` fetches only the latest commit.
pub fn start_clone(parent: &Path, url: &str, name: &str, shallow: bool) -> Result<CloneJob, String> {
    let mut args = vec!["clone", "--progress"];
    if shallow {
        args.extend(["--depth", "1"]);
    }
    args.extend(["--", url, name]);
    let mut child = command(parent)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't run git: {e}"))?;
    let stderr = child.stderr.take();
    Ok(CloneJob {
        child: std::sync::Mutex::new(child),
        stderr: std::sync::Mutex::new(stderr),
        progress: std::sync::Mutex::new("Starting…".into()),
        cancelled: std::sync::atomic::AtomicBool::new(false),
        dest: parent.join(name),
    })
}

impl CloneJob {
    /// Blocks until git exits, updating `progress` along the way.
    pub fn wait(&self) -> Result<PathBuf, String> {
        use std::io::Read;
        let mut all = Vec::new();
        if let Some(mut stderr) = self.stderr.lock().unwrap().take() {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf) {
                if n == 0 {
                    break;
                }
                all.extend_from_slice(&buf[..n]);
                // Progress lines are redrawn with '\r'; keep the newest one.
                let text = String::from_utf8_lossy(&all[all.len().saturating_sub(512)..]).into_owned();
                if let Some(line) = text.rsplit(['\r', '\n']).map(str::trim).find(|l| !l.is_empty()) {
                    *self.progress.lock().unwrap() = line.trim_start_matches("remote: ").to_string();
                }
            }
        }
        let status = self.child.lock().unwrap().wait().map_err(|e| e.to_string())?;
        if self.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            // Killed, so git couldn't clean up after itself. Callers only
            // clone into folders that didn't exist, so this is all ours.
            std::fs::remove_dir_all(&self.dest).ok();
            Err("Cancelled".into())
        } else if status.success() {
            Ok(self.dest.clone())
        } else {
            Err(error_text(&all, &[]))
        }
    }

    /// Stops the clone; `wait` then removes the half-written folder.
    pub fn cancel(&self) {
        self.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut child) = self.child.try_lock() {
            child.kill().ok();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameLine {
    pub author: String,
    pub time: i64,
    pub summary: String,
    pub sha: String,
    pub uncommitted: bool,
}

/// Blame for one (0-based) line, computed against the given buffer text so
/// unsaved edits are accounted for.
pub fn blame_line(root: &Path, rel: &str, line: usize, contents: &str) -> Option<BlameLine> {
    let range = format!("{},{}", line + 1, line + 1);
    let out = run_with_stdin(
        root,
        &["blame", "--porcelain", "-L", &range, "--contents", "-", "--", rel],
        contents.as_bytes(),
    )
    .ok()?;
    let mut lines = out.lines();
    let sha = lines.next()?.split_whitespace().next()?.to_string();
    let mut blame = BlameLine {
        uncommitted: sha.chars().all(|c| c == '0'),
        sha: sha.chars().take(8).collect(),
        author: String::new(),
        time: 0,
        summary: String::new(),
    };
    for l in lines {
        if let Some(v) = l.strip_prefix("author ") {
            blame.author = v.to_string();
        } else if let Some(v) = l.strip_prefix("author-time ") {
            blame.time = v.parse().unwrap_or(0);
        } else if let Some(v) = l.strip_prefix("summary ") {
            blame.summary = v.to_string();
        }
    }
    Some(blame)
}

pub fn user_name(root: &Path) -> Option<String> {
    run(root, &["config", "user.name"]).ok().map(|s| s.trim().to_string())
}

/// "3 days ago" style relative time.
pub fn relative_time(unix: i64) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs() as i64;
    let secs = (now - unix).max(0);
    let (n, unit) = match secs {
        0..60 => return "just now".into(),
        60..3600 => (secs / 60, "minute"),
        3600..86400 => (secs / 3600, "hour"),
        86400..2_592_000 => (secs / 86400, "day"),
        2_592_000..31_536_000 => (secs / 2_592_000, "month"),
        _ => (secs / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

// ---- Line diffs ------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HunkKind {
    Added,
    Modified,
    Deleted,
}

/// A changed region between the base text and the current buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub kind: HunkKind,
    /// Rows in the current buffer (empty for deletions: the rows were
    /// removed just before `new_rows.start`).
    pub new_rows: Range<usize>,
    /// Rows in the base text.
    pub old_rows: Range<usize>,
    /// The base text of `old_rows`, including trailing newlines.
    pub old_text: String,
}

pub fn diff_hunks(base: &str, current: &str) -> Vec<Hunk> {
    use similar::DiffOp;
    let diff = similar::TextDiff::configure()
        .timeout(Duration::from_millis(200))
        .diff_lines(base, current);
    let old_lines: Vec<&str> = base.split_inclusive('\n').collect();
    let text = |r: Range<usize>| old_lines[r.start.min(old_lines.len())..r.end.min(old_lines.len())].concat();
    diff.ops()
        .iter()
        .filter_map(|op| match *op {
            DiffOp::Equal { .. } => None,
            DiffOp::Delete { old_index, old_len, new_index } => Some(Hunk {
                kind: HunkKind::Deleted,
                new_rows: new_index..new_index,
                old_rows: old_index..old_index + old_len,
                old_text: text(old_index..old_index + old_len),
            }),
            DiffOp::Insert { old_index, new_index, new_len } => Some(Hunk {
                kind: HunkKind::Added,
                new_rows: new_index..new_index + new_len,
                old_rows: old_index..old_index,
                old_text: String::new(),
            }),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => Some(Hunk {
                kind: HunkKind::Modified,
                new_rows: new_index..new_index + new_len,
                old_rows: old_index..old_index + old_len,
                old_text: text(old_index..old_index + old_len),
            }),
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
    /// A run of unchanged lines that was collapsed.
    Skipped,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    pub text: String,
}

/// A unified diff with `context` unchanged lines around each change.
pub fn unified_lines(base: &str, current: &str, context: usize) -> Vec<DiffLine> {
    use similar::ChangeTag;
    let diff = similar::TextDiff::configure()
        .timeout(Duration::from_secs(2))
        .diff_lines(base, current);
    let mut out = Vec::new();
    let mut shown_until = 0; // old-line index after the last shown line
    for group in diff.grouped_ops(context) {
        let start = group.first().map_or(0, |op| op.old_range().start);
        if start > shown_until {
            out.push(DiffLine {
                kind: DiffLineKind::Skipped,
                old_no: None,
                new_no: None,
                text: format!("{} unchanged lines", start - shown_until),
            });
        }
        shown_until = group.last().map_or(start, |op| op.old_range().end);
        for op in &group {
            for change in diff.iter_changes(op) {
                let text = change.value().trim_end_matches(['\n', '\r']).to_string();
                let (kind, old_no, new_no) = match change.tag() {
                    ChangeTag::Equal => (DiffLineKind::Context, change.old_index(), change.new_index()),
                    ChangeTag::Delete => (DiffLineKind::Removed, change.old_index(), None),
                    ChangeTag::Insert => (DiffLineKind::Added, None, change.new_index()),
                };
                out.push(DiffLine {
                    kind,
                    old_no: old_no.map(|n| n + 1),
                    new_no: new_no.map(|n| n + 1),
                    text,
                });
            }
        }
    }
    let total = base.split_inclusive('\n').count();
    if !out.is_empty() && total > shown_until {
        out.push(DiffLine {
            kind: DiffLineKind::Skipped,
            old_no: None,
            new_no: None,
            text: format!("{} unchanged lines", total - shown_until),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_web_urls() {
        assert_eq!(web_url("git@github.com:owner/repo.git").as_deref(), Some("https://github.com/owner/repo"));
        assert_eq!(web_url("https://gitlab.com/group/sub/repo.git").as_deref(), Some("https://gitlab.com/group/sub/repo"));
        assert_eq!(web_url("https://user:tok@github.com/o/r").as_deref(), Some("https://github.com/o/r"));
        assert_eq!(web_url("ssh://git@host.dev:2222/o/r.git").as_deref(), Some("https://host.dev/o/r"));
        assert_eq!(web_url("/srv/git/repo.git"), None);
    }

    #[test]
    fn clone_folder_names() {
        assert_eq!(repo_name_from_url("https://github.com/o/kin.git").as_deref(), Some("kin"));
        assert_eq!(repo_name_from_url("git@github.com:o/kin").as_deref(), Some("kin"));
        assert_eq!(repo_name_from_url("https://github.com/o/kin/").as_deref(), Some("kin"));
        assert_eq!(repo_name_from_url("").as_deref(), None);
    }

    #[test]
    fn parses_porcelain_v2() {
        let root = Path::new("/repo");
        let out = "# branch.oid abc123\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 aaa bbb src/a.js\0\
1 A. N... 000000 100644 100644 000 ccc new.js\0\
2 R. N... 100644 100644 100644 ddd eee R100 renamed.js\0old.js\0\
u UU N... 100644 100644 100644 100644 f g h conflict.js\0\
? notes.txt\0";
        let s = parse_status(root, out);
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!((s.ahead, s.behind), (2, 1));
        assert_eq!(s.files.len(), 5);
        let get = |rel: &str| s.files.iter().find(|f| f.rel == rel).unwrap().clone();
        assert_eq!(get("src/a.js").unstaged, Some(Change::Modified));
        assert_eq!(get("new.js").staged, Some(Change::Added));
        assert_eq!(get("renamed.js").staged, Some(Change::Renamed));
        assert_eq!(get("conflict.js").primary(), Change::Conflicted);
        assert_eq!(get("notes.txt").unstaged, Some(Change::Untracked));
        assert_eq!(s.staged().count(), 2);
        let map = s.path_map(root);
        assert_eq!(map.get(Path::new("/repo/src")), Some(&Change::Modified));
    }

    #[test]
    fn hunks() {
        let base = "a\nb\nc\nd\n";
        let h = diff_hunks(base, "a\nB\nc\nd\ne\n");
        assert_eq!(h.len(), 2);
        assert_eq!((h[0].kind, h[0].new_rows.clone(), h[0].old_text.as_str()), (HunkKind::Modified, 1..2, "b\n"));
        assert_eq!((h[1].kind, h[1].new_rows.clone()), (HunkKind::Added, 4..5));
        let d = diff_hunks(base, "a\nd\n");
        assert_eq!((d[0].kind, d[0].new_rows.clone(), d[0].old_text.as_str()), (HunkKind::Deleted, 1..1, "b\nc\n"));
    }

    #[test]
    fn unified_diff_collapses_unchanged_runs() {
        let base: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let current = base.replace("line 10\n", "line ten\n");
        let lines = unified_lines(&base, &current, 2);
        assert_eq!(lines[0].kind, DiffLineKind::Skipped);
        assert_eq!(lines[0].text, "7 unchanged lines");
        let removed = lines.iter().find(|l| l.kind == DiffLineKind::Removed).unwrap();
        assert_eq!((removed.old_no, removed.text.as_str()), (Some(10), "line 10"));
        let added = lines.iter().find(|l| l.kind == DiffLineKind::Added).unwrap();
        assert_eq!((added.new_no, added.text.as_str()), (Some(10), "line ten"));
        assert_eq!(lines.last().unwrap().kind, DiffLineKind::Skipped);
    }

    #[test]
    fn real_repository_round_trip() {
        if !is_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kin-git-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        init(&dir).unwrap();
        run(&dir, &["config", "user.name", "Test"]).unwrap();
        run(&dir, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        let root = discover(&dir).unwrap();
        let s = status(&root).unwrap();
        assert_eq!(s.files[0].unstaged, Some(Change::Untracked));
        stage(&root, &["a.txt".into()]).unwrap();
        assert_eq!(status(&root).unwrap().staged().count(), 1);
        commit(&root, "first", false).unwrap();
        assert_eq!(head_text(&root, "a.txt").as_deref(), Some("one\ntwo\n"));
        std::fs::write(dir.join("a.txt"), "one\n2\n").unwrap();
        let s = status(&root).unwrap();
        assert_eq!(s.files[0].unstaged, Some(Change::Modified));
        let blame = blame_line(&root, "a.txt", 0, "one\n2\n").unwrap();
        assert_eq!(blame.summary, "first");
        assert!(blame_line(&root, "a.txt", 1, "one\n2\n").unwrap().uncommitted);
        discard(&root, &s.files).unwrap();
        assert!(status(&root).unwrap().files.is_empty());
        create_branch(&root, "feature").unwrap();
        assert!(branches(&root).unwrap().iter().any(|b| b.name == "feature" && b.current));

        // Push to a bare "remote", then fetch and pull from it.
        let remote = dir.with_extension("remote.git");
        let _ = std::fs::remove_dir_all(&remote);
        run(&dir, &["init", "--bare", "--quiet", remote.to_str().unwrap()]).unwrap();
        assert!(!has_remote(&root));
        run(&root, &["remote", "add", "origin", remote.to_str().unwrap()]).unwrap();
        assert!(has_remote(&root));
        push(&root, Some("feature")).unwrap();
        let s = status(&root).unwrap();
        assert_eq!(s.upstream.as_deref(), Some("origin/feature"));
        assert_eq!((s.ahead, s.behind), (0, 0));
        fetch(&root).unwrap();
        assert!(pull(&root).unwrap().contains("Already up to date"));
        assert!(branches(&root).unwrap().iter().any(|b| b.remote && b.name == "origin/feature"));
        assert_eq!(remote_url(&root).as_deref(), remote.to_str());
        assert_eq!(last_commit(&root).unwrap().summary, "first");

        // Clone the remote back, shallow, with progress.
        let parent = dir.with_extension("clones");
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&parent).unwrap();
        run(&remote, &["symbolic-ref", "HEAD", "refs/heads/feature"]).unwrap();
        let url = format!("file://{}", remote.display());
        let job = start_clone(&parent, &url, "copy", true).unwrap();
        let cloned = job.wait().unwrap();
        assert_eq!(std::fs::read_to_string(cloned.join("a.txt")).unwrap(), "one\ntwo\n");
        // A failing clone reports git's error.
        let err = start_clone(&parent, "file:///nonexistent/kin.git", "nope", false).unwrap().wait().unwrap_err();
        assert!(!err.is_empty() && !parent.join("nope").exists(), "{err}");
        let _ = std::fs::remove_dir_all(&parent);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&remote);
    }
}
