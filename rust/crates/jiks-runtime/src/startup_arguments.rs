//! Fixed-root, inert-preserving startup preferences. Only two exact line tokens
//! are consumed. Never pass retained lines to a command-line parser or shell.
//! Reads do not create directories/files; saves affect the next normal launch.
use anyhow::{Context, Result, ensure};
use jiks_store::{CommitStage, SettingsStore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    fs::OpenOptions,
    io::{self, Read},
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Mutex,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

pub const MAX_FILE_BYTES: usize = 128 * 1024;
const FILE_NAME: &str = "args.txt";
const MINIMIZED: &str = "--minimized";
const READ_ONLY: &str = "--read-only";
const BOM: &[u8] = b"\xef\xbb\xbf";
// This module is the only normal args.txt writer. Serialize its bounded loads
// and saves so ReplaceFile's filename-open gap cannot look like an absent file.
// The store's target lock separately protects both commit-time revision checks.
static OPERATIONS: Mutex<()> = Mutex::new(());

/// Safe metadata for IPC/UI. No source lines, filenames, paths or credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub minimized: bool,
    pub read_only: bool,
    pub inert_legacy_count: usize,
    pub legacy_contents_redacted: bool,
    pub file_exists: bool,
    pub revision: String,
}

/// The revision comes from the last loaded snapshot. It must not be omitted or
/// silently replaced by a fresh revision at save time: that would hide edits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preferences {
    pub minimized: bool,
    pub read_only: bool,
    pub expected_revision: String,
}

pub struct StartupArguments {
    pub minimized: bool,
    pub read_only: bool,
    pub inert_legacy_count: usize,
    pub revision: String,
    original: Option<Vec<u8>>,
}
impl fmt::Debug for StartupArguments {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StartupArguments")
            .field("minimized", &self.minimized)
            .field("read_only", &self.read_only)
            .field("inert_legacy_count", &self.inert_legacy_count)
            .field("file_exists", &self.original.is_some())
            .field("original", &"[redacted]")
            .finish()
    }
}
impl StartupArguments {
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            minimized: self.minimized,
            read_only: self.read_only,
            inert_legacy_count: self.inert_legacy_count,
            legacy_contents_redacted: self.inert_legacy_count != 0,
            file_exists: self.original.is_some(),
            revision: self.revision.clone(),
        }
    }
}

fn target(root: &Path) -> Result<PathBuf> {
    // The coordinator chooses root. There is deliberately no payload filename.
    Ok(std::path::absolute(root)?.join(FILE_NAME))
}
fn revision(bytes: Option<&[u8]>) -> String {
    match bytes {
        None => "missing".into(),
        Some(bytes) => format!("{:x}", Sha256::digest(bytes)),
    }
}
fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("启动参数文件未能读取；原内容未修改"),
    };
    ensure!(
        file.metadata()?.len() <= MAX_FILE_BYTES as u64,
        "启动参数文件超过128KiB限制；原内容未修改"
    );
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .context("启动参数文件未能完整读取；原内容未修改")?;
    ensure!(
        bytes.len() <= MAX_FILE_BYTES,
        "启动参数文件超过128KiB限制；原内容未修改"
    );
    Ok(Some(bytes))
}
fn text(bytes: &[u8]) -> Result<&str> {
    let bytes = bytes.strip_prefix(BOM).unwrap_or(bytes);
    let text =
        std::str::from_utf8(bytes).context("启动参数必须是有效UTF-8（可带BOM）；原内容未修改")?;
    ensure!(
        !text.contains('\0'),
        "启动参数包含无效NUL字符；未应用或修改"
    );
    Ok(text)
}
/// File.ReadAllLines in the old AppFlags consumes each whole line literally;
/// quotes, spaces, '=' and internal CLI verbs must never be re-tokenized.
fn lines(text: &str) -> impl Iterator<Item = (&str, &str)> {
    let mut remaining = text;
    std::iter::from_fn(move || {
        if remaining.is_empty() {
            return None;
        }
        let end = remaining
            .bytes()
            .position(|byte| matches!(byte, b'\r' | b'\n'))
            .unwrap_or(remaining.len());
        let rest = &remaining[end..];
        let ending = if rest.starts_with("\r\n") {
            "\r\n"
        } else if rest.starts_with('\r') {
            "\r"
        } else if rest.starts_with('\n') {
            "\n"
        } else {
            ""
        };
        let line = &remaining[..end];
        remaining = &remaining[end + ending.len()..];
        Some((line, ending))
    })
}
fn decode(original: Option<Vec<u8>>) -> Result<StartupArguments> {
    let source = text(original.as_deref().unwrap_or_default())?;
    let mut minimized = false;
    let mut read_only = false;
    let mut inert_legacy_count = 0;
    for (line, _) in lines(source) {
        match line {
            MINIMIZED => minimized = true,
            READ_ONLY => read_only = true,
            _ if !line.trim().is_empty() => inert_legacy_count += 1,
            _ => {}
        }
    }
    Ok(StartupArguments {
        minimized,
        read_only,
        inert_legacy_count,
        revision: revision(original.as_deref()),
        original,
    })
}

/// No default writes, directory creation, command execution or process probing.
pub fn load(root: &Path) -> Result<StartupArguments> {
    let _guard = OPERATIONS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    decode(read_bytes(&target(root)?)?)
}

fn updated(arguments: &StartupArguments, preferences: &Preferences) -> Result<Vec<u8>> {
    let original = arguments.original.as_deref().unwrap_or_default();
    let source = text(original)?;
    let ending = lines(source)
        .find_map(|(_, ending)| (!ending.is_empty()).then_some(ending))
        .unwrap_or("\r\n");
    let mut output = String::with_capacity(source.len() + 32);
    let mut kept_minimized = false;
    let mut kept_read_only = false;
    for (line, newline) in lines(source) {
        match line {
            MINIMIZED if !preferences.minimized => continue,
            READ_ONLY if !preferences.read_only => continue,
            MINIMIZED => kept_minimized = true,
            READ_ONLY => kept_read_only = true,
            _ => {}
        }
        output.push_str(line);
        output.push_str(newline);
    }
    for (wanted, kept, token) in [
        (preferences.minimized, kept_minimized, MINIMIZED),
        (preferences.read_only, kept_read_only, READ_ONLY),
    ] {
        if wanted && !kept {
            if !output.is_empty() && !output.ends_with(['\r', '\n']) {
                output.push_str(ending);
            }
            output.push_str(token);
            output.push_str(ending);
        }
    }
    let mut bytes = Vec::with_capacity(output.len() + 3);
    if original.starts_with(BOM) {
        bytes.extend_from_slice(BOM);
    }
    bytes.extend_from_slice(output.as_bytes());
    ensure!(
        bytes.len() <= MAX_FILE_BYTES,
        "保存后的启动参数超过128KiB限制；原内容未修改"
    );
    Ok(bytes)
}

/// Commits only the two supported lines; inert bytes, order, quotes and existing
/// line-ending bytes survive. Appending a flag may add a separator after an
/// unterminated last line. The existing file is kept as args.txt.previous.
/// The current session's read-only gate must never be changed by this result.
pub fn save(root: &Path, preferences: &Preferences) -> Result<Snapshot> {
    save_with_checkpoint(root, preferences, |_| Ok(()))
}

fn save_with_checkpoint(
    root: &Path,
    preferences: &Preferences,
    mut checkpoint: impl FnMut(CommitStage) -> io::Result<()>,
) -> Result<Snapshot> {
    let _guard = OPERATIONS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let path = target(root)?;
    let arguments = decode(read_bytes(&path)?)?;
    ensure!(
        arguments.revision == preferences.expected_revision,
        "启动参数已被其他操作修改；请重新读取后再保存"
    );
    let bytes = updated(&arguments, preferences)?;
    if arguments.original.as_deref() == Some(bytes.as_slice())
        || arguments.original.is_none() && bytes.is_empty()
    {
        return Ok(arguments.snapshot());
    }
    let store = SettingsStore::new(path.parent().context("启动参数目录缺失")?)?;
    let expected = preferences.expected_revision.as_str();
    store.write_with_checkpoint(FILE_NAME, &bytes, |stage| {
        checkpoint(stage)?;
        if matches!(
            stage,
            CommitStage::BeforeTemporary | CommitStage::BeforeCommit
        ) {
            // This runs inside the store's target lock. Do not call store.read
            // recursively here. Both checks are bounded raw snapshot reads.
            let current = read_bytes(&path)
                .map_err(|_| io::Error::other("启动参数复核失败；未提交新参数"))?;
            if revision(current.as_deref()) != expected {
                return Err(io::Error::other("启动参数已被其他操作修改；未提交新参数"));
            }
        }
        Ok(())
    })?;
    let actual = read_bytes(&path)?;
    ensure!(
        actual.as_deref() == Some(bytes.as_slice()),
        "保存后的启动参数读回不一致；请重新读取实际文件"
    );
    Ok(decode(actual)?.snapshot())
}

#[cfg(test)]
#[path = "startup_arguments/tests.rs"]
mod tests;
