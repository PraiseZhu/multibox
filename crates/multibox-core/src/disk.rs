//! The volume a repository lives on: how full it is, what holds it, and the
//! two things Multibox can do about it. It can grow the Dev Drive `setup`
//! made, and delete the original that a move with `init` kept.
//!
//! Only Windows has anything to grow. A Dev Drive there is a volume inside a
//! virtual disk file on another volume, and the file has a maximum size. APFS
//! volumes share their container's free space, and Multibox makes no volumes
//! on macOS or Linux.
//!
//! Nothing here shrinks a volume. Windows resizes NTFS and RAW partitions
//! only, not ReFS, and a smaller maximum would not give the host disk back
//! anything: a dynamic virtual disk only takes what has been written to it.

use crate::git::{self, Repo};
use crate::{Error, Result};
use multibox_clone::{PurgeReport, Volume};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const GIB: u64 = 1 << 30;

/// The largest VHDX Windows makes: 64 TiB.
const MAX_IMAGE_GIB: u64 = 64 * 1024;

#[derive(Clone, Debug)]
pub struct DiskStatus {
    /// The volume the repository is on.
    pub volume: Volume,
    /// Set when that volume lives in a virtual disk file, as a Dev Drive
    /// made by `git multibox setup` does.
    pub image: Option<DiskImage>,
}

#[derive(Clone, Debug)]
pub struct DiskImage {
    pub file: PathBuf,
    /// What the file takes now. It grows as the volume inside is written and
    /// does not shrink when files there are deleted.
    pub file_bytes: u64,
    /// The volume holding the file, whose free space the volume inside
    /// really draws on.
    pub host: Option<Volume>,
}

impl DiskStatus {
    /// What can still be written. A volume in a file reports the room left
    /// below its maximum, but every byte written there grows the file, so the
    /// volume holding the file may run out first.
    pub fn room(&self) -> u64 {
        match self.image.as_ref().and_then(|image| image.host.as_ref()) {
            Some(host) => self.volume.free.min(host.free),
            None => self.volume.free,
        }
    }

    /// Whether the volume holding the image has less room than the volume
    /// inside it reports.
    pub fn limited_by_host(&self) -> bool {
        self.room() < self.volume.free
    }

    /// Whether Multibox can grow this volume: one inside a virtual disk file,
    /// on Windows.
    pub fn can_grow(&self) -> bool {
        cfg!(windows) && self.image.is_some()
    }
}

/// Growing takes the volume offline for a few seconds, and has not been tried
/// on a real Dev Drive yet. Until it has, the window and the command offer it
/// only with `MULTIBOX_EXPERIMENTAL_GROW=1` set.
pub fn grow_enabled() -> bool {
    std::env::var_os("MULTIBOX_EXPERIMENTAL_GROW").is_some_and(|value| value == "1")
}

/// Reads the volume holding `path` and, where there is one, the virtual disk
/// file it lives in. Quick: it asks the system, and reads no files.
pub fn disk_status(path: &Path) -> Result<DiskStatus> {
    let volume = multibox_clone::volume_at(path)?;
    let image = multibox_clone::backing_image(path)
        .ok()
        .flatten()
        .map(|file| DiskImage {
            file_bytes: fs::metadata(&file).map(|meta| meta.len()).unwrap_or(0),
            host: multibox_clone::volume_at(&file).ok(),
            file,
        });
    Ok(DiskStatus { volume, image })
}

// ---- the original a move kept -------------------------------------------

/// Where the moved repository remembers the original `init` left behind. In
/// the git directory, so it is never mistaken for part of the project.
const KEPT_FILE: &str = "multibox-kept.json";

/// What `init` appends to the project's folder name for the original it keeps.
pub const KEPT_SUFFIX: &str = ".multibox-original";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeptOriginal {
    /// Where the original is now: its old path with [`KEPT_SUFFIX`] added.
    pub path: PathBuf,
    /// The old path, which now links to the moved project.
    pub moved_from: PathBuf,
    /// When the move happened, in seconds since the Unix epoch.
    pub moved_at: u64,
    /// What it held then, as far as the move copied it.
    pub bytes: Option<u64>,
}

fn read_kept(common_dir: &Path) -> Vec<KeptOriginal> {
    let Ok(text) = fs::read_to_string(common_dir.join(KEPT_FILE)) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let text = |entry: &Value, key: &str| entry.get(key).and_then(Value::as_str).map(PathBuf::from);
    value
        .get("kept")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some(KeptOriginal {
                path: text(entry, "path")?,
                moved_from: text(entry, "moved_from").unwrap_or_default(),
                moved_at: entry.get("moved_at").and_then(Value::as_u64).unwrap_or(0),
                bytes: entry.get("bytes").and_then(Value::as_u64),
            })
        })
        .collect()
}

fn write_kept(common_dir: &Path, kept: &[KeptOriginal]) -> std::io::Result<()> {
    let file = common_dir.join(KEPT_FILE);
    if kept.is_empty() {
        return match fs::remove_file(&file) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        };
    }
    let entries: Vec<Value> = kept
        .iter()
        .map(|kept| {
            json!({
                "path": kept.path.display().to_string(),
                "moved_from": kept.moved_from.display().to_string(),
                "moved_at": kept.moved_at,
                "bytes": kept.bytes,
            })
        })
        .collect();
    let value = json!({ "version": 1, "kept": entries });
    fs::write(file, serde_json::to_string_pretty(&value)? + "\n")
}

/// A size for a sentence: GiB for a project, smaller units for less.
pub fn size_text(bytes: u64) -> String {
    const MIB: u64 = 1 << 20;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{} KiB", bytes.div_ceil(1024))
    }
}

/// `path` with the platform's own separators: git reports Windows paths with
/// forward slashes, and joining a name on adds a backslash.
pub(crate) fn native(path: &Path) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(path.to_string_lossy().replace('/', "\\"))
    } else {
        path.to_path_buf()
    }
}

/// Notes the original a move kept, in the moved repository's git directory.
pub(crate) fn remember_kept(common_dir: &Path, kept: KeptOriginal) -> std::io::Result<()> {
    let kept = KeptOriginal {
        path: native(&kept.path),
        moved_from: native(&kept.moved_from),
        ..kept
    };
    let mut all = read_kept(common_dir);
    all.retain(|other| other.path != kept.path);
    all.push(kept);
    write_kept(common_dir, &all)
}

fn same_place(a: &Path, b: &Path) -> bool {
    a == b || matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

/// The originals that moves of this repository kept and that are still on
/// disk. A record whose directory is gone is forgotten.
pub fn kept_originals(repo_dir: &Path) -> Vec<KeptOriginal> {
    let Ok(repo) = Repo::discover(repo_dir) else {
        return Vec::new();
    };
    let all = read_kept(&repo.common_dir);
    let (present, gone): (Vec<_>, Vec<_>) = all.into_iter().partition(|kept| kept.path.is_dir());
    if !gone.is_empty() {
        let _ = write_kept(&repo.common_dir, &present);
    }
    present
}

/// What the kept original has that the moved project lacks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeptCheck {
    /// Branches, tags and stashes of the original whose commits the project
    /// does not have.
    pub missing: Vec<String>,
    /// Files with changes in the original's worktree that differ from the
    /// project's copy, or that the project lacks.
    pub differing: Vec<String>,
}

impl KeptCheck {
    /// Deleting the original loses nothing.
    pub fn is_clear(&self) -> bool {
        self.missing.is_empty() && self.differing.is_empty()
    }
}

/// Compares the original a move kept with the project it became. The move
/// made them equal, so anything found was done in the original afterwards.
pub fn check_kept(project: &Path, kept: &Path) -> Result<KeptCheck> {
    let mut check = KeptCheck::default();

    // Every commit a ref of the original names must be in the project.
    let refs = git::output(kept, &["for-each-ref", "--format=%(objectname) %(refname)"])?;
    let refs: Vec<(&str, &str)> = refs
        .lines()
        .filter_map(|line| line.split_once(' '))
        .collect();
    if !refs.is_empty() {
        let input: String = refs.iter().map(|(id, _)| format!("{id}\n")).collect();
        let answer = git_with_input(project, &["cat-file", "--batch-check"], &input)?;
        for ((_, name), line) in refs.iter().zip(answer.lines()) {
            if line.ends_with(" missing") {
                check.missing.push((*name).to_owned());
            }
        }
    }

    // Every change in the original's worktree must be in the project's.
    let status = git::output(
        kept,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    let mut fields = status.split('\0').filter(|field| !field.is_empty());
    while let Some(field) = fields.next() {
        let (code, path) = field.split_at(3.min(field.len()));
        if code.contains('R') || code.contains('C') {
            // The source of a rename or copy follows; the new name decides.
            fields.next();
        }
        // A file deleted in the original holds nothing to lose.
        if code.contains('D') {
            continue;
        }
        if !same_file(&kept.join(path), &project.join(path)) {
            check.differing.push(path.to_owned());
        }
    }
    Ok(check)
}

fn same_file(a: &Path, b: &Path) -> bool {
    let (Ok(meta_a), Ok(meta_b)) = (fs::metadata(a), fs::metadata(b)) else {
        return false;
    };
    if meta_a.is_dir() || meta_b.is_dir() {
        return meta_a.is_dir() && meta_b.is_dir();
    }
    meta_a.len() == meta_b.len() && matches!((fs::read(a), fs::read(b)), (Ok(a), Ok(b)) if a == b)
}

fn git_with_input(dir: &Path, args: &[&str], input: &str) -> Result<String> {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    multibox_clone::no_console_window(&mut command);
    let mut child = command
        .spawn()
        .map_err(|err| Error::new(format!("cannot run git: {err}")))?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let input = input.to_owned();
    // Written from another thread, so a full output pipe cannot stall both.
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child
        .wait_with_output()
        .map_err(|err| Error::new(format!("cannot run git: {err}")))?;
    let _ = writer.join();
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(Error::new(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
}

/// Deletes an original a move of `project` kept. Only a directory the
/// project's record names is deleted, never through a link, and never the
/// project itself.
pub fn delete_kept(project: &Path, kept: &Path) -> Result<PurgeReport> {
    let repo = Repo::discover(project)?;
    let known = read_kept(&repo.common_dir)
        .into_iter()
        .any(|record| same_place(&record.path, kept));
    let named = kept
        .file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(KEPT_SUFFIX));
    if !known || !named {
        return Err(Error::new(format!(
            "refusing to delete {}: not an original a move of this project kept",
            kept.display()
        )));
    }
    if fs::symlink_metadata(kept)?.file_type().is_symlink() {
        return Err(Error::new(format!(
            "refusing to delete {}: it is a link",
            kept.display()
        )));
    }
    if same_place(kept, &repo.worktree) || same_place(kept, &repo.common_dir) {
        return Err(Error::new(format!(
            "refusing to delete {}: it is the project itself",
            kept.display()
        )));
    }
    let report = multibox_clone::purge(kept, None)?;
    if !kept.exists() {
        let mut all = read_kept(&repo.common_dir);
        all.retain(|record| !same_place(&record.path, kept) && record.path != kept);
        write_kept(&repo.common_dir, &all)?;
    }
    Ok(report)
}

// ---- growing a Dev Drive --------------------------------------------------

#[derive(Clone, Debug)]
pub struct GrowPlan {
    /// The virtual disk file.
    pub file: PathBuf,
    /// Where the volume inside is mounted, such as `V:\`.
    pub root: String,
    /// The new maximum size of the virtual disk.
    pub size_gib: u64,
    /// The volume's size now.
    pub current_bytes: u64,
}

/// Checks that the volume of `status` can grow to `size_gib`. Nothing is
/// changed.
pub fn plan_grow(status: &DiskStatus, size_gib: u64) -> Result<GrowPlan> {
    if !cfg!(windows) {
        return Err(Error::new(
            "only a Dev Drive in a virtual disk file on Windows can grow; volumes here share their disk's free space",
        ));
    }
    let image = status.image.as_ref().ok_or_else(|| {
        Error::new(format!(
            "{} is not in a virtual disk file; Multibox only grows the Dev Drive it made",
            status.volume.root
        ))
    })?;
    let extension = image
        .file
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase());
    if !matches!(extension.as_deref(), Some("vhdx" | "vhd")) {
        return Err(Error::new(format!(
            "{} is not a VHD or VHDX file",
            image.file.display()
        )));
    }
    // The volume is a little smaller than its disk, so a size must clear it
    // by a whole GiB to be an increase.
    let current_gib = status.volume.total.div_ceil(GIB);
    if size_gib <= current_gib {
        return Err(Error::new(format!(
            "{} is already about {current_gib} GiB; give a larger size",
            status.volume.root
        )));
    }
    if size_gib > MAX_IMAGE_GIB {
        return Err(Error::new(format!(
            "a virtual disk can be at most {MAX_IMAGE_GIB} GiB"
        )));
    }
    Ok(GrowPlan {
        file: image.file.clone(),
        root: status.volume.root.clone(),
        size_gib,
        current_bytes: status.volume.total,
    })
}

fn ps_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The elevated script. It changes nothing until it holds the volume locked,
/// which Windows grants only while no program has a file or folder open on
/// it, and whatever happens after it detached the virtual disk, it attaches
/// it again.
pub fn render_grow_script(plan: &GrowPlan, result_file: &Path) -> String {
    let header = format!(
        "$vhdx = {}\n$root = {}\n$sizeMb = {}\n$resultFile = {}\n",
        ps_quote(&plan.file.display().to_string()),
        ps_quote(&plan.root),
        plan.size_gib * 1024,
        ps_quote(&result_file.display().to_string()),
    );
    format!("{header}{GROW_SCRIPT}")
}

const GROW_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'

function Report($ok, $message) {
    @{ ok = $ok; message = $message } | ConvertTo-Json | Set-Content -LiteralPath $resultFile -Encoding UTF8
}

function Run-Diskpart($lines) {
    $script = [IO.Path]::GetTempFileName()
    $lines | Set-Content -LiteralPath $script -Encoding ASCII
    $output = & diskpart.exe /s $script 2>&1 | Out-String
    $code = $LASTEXITCODE
    Remove-Item -LiteralPath $script
    if ($code -ne 0) { throw "diskpart failed ($code): $output" }
}

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class MultiboxVolume {
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr CreateFileW(string name, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool DeviceIoControl(IntPtr device, uint code, IntPtr input, uint inputSize, IntPtr output, uint outputSize, out uint returned, IntPtr overlapped);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool CloseHandle(IntPtr handle);

    // Locks the volume, which Windows grants only when nothing has a file
    // open on it, then dismounts it. The handle keeps everyone else out
    // until it is closed. Returns IntPtr.Zero when the volume is in use.
    public static IntPtr Lock(string device) {
        IntPtr handle = CreateFileW(device, 0xC0000000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero);
        if (handle == new IntPtr(-1)) {
            throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
        }
        uint returned;
        for (int attempt = 0; attempt < 10; attempt++) {
            // FSCTL_LOCK_VOLUME. A scanner or the indexer may hold a file
            // for a moment, so it is asked a few times.
            if (DeviceIoControl(handle, 0x00090018, IntPtr.Zero, 0, IntPtr.Zero, 0, out returned, IntPtr.Zero)) {
                // FSCTL_DISMOUNT_VOLUME
                if (!DeviceIoControl(handle, 0x00090020, IntPtr.Zero, 0, IntPtr.Zero, 0, out returned, IntPtr.Zero)) {
                    int error = Marshal.GetLastWin32Error();
                    CloseHandle(handle);
                    throw new System.ComponentModel.Win32Exception(error);
                }
                return handle;
            }
            System.Threading.Thread.Sleep(500);
        }
        CloseHandle(handle);
        return IntPtr.Zero;
    }
}
'@

try {
    $image = Get-DiskImage -ImagePath $vhdx
    if (-not $image.Attached) { throw "$vhdx is not attached; nothing was changed" }
    $disk = $image | Get-Disk
    if ($disk.BusType -ne 'File Backed Virtual' -or $disk.Location -ne $vhdx) {
        throw "disk $($disk.Number) is not backed by $vhdx; nothing was changed"
    }
    $partition = Get-Partition -DiskNumber $disk.Number | Where-Object { $_.AccessPaths -contains $root } | Select-Object -First 1
    if (-not $partition) { throw "$root is not on $vhdx; nothing was changed" }
    if ($image.Size -ge [int64]$sizeMb * 1MB) { throw "$vhdx is already that large; nothing was changed" }
    $partitionNumber = $partition.PartitionNumber
    $letter = if ("$($partition.DriveLetter)" -match '^[A-Za-z]$') { $partition.DriveLetter } else { $null }
    $device = ($partition | Get-Volume).Path.TrimEnd('\')

    $lock = [MultiboxVolume]::Lock($device)
    if ($lock -eq [IntPtr]::Zero) {
        Report $false 'in-use'
        exit
    }
    $detached = $false
    $grown = $false
    try {
        Run-Diskpart @("select vdisk file=`"$vhdx`"", 'detach vdisk')
        $detached = $true
        Run-Diskpart @("select vdisk file=`"$vhdx`"", "expand vdisk maximum=$sizeMb")
        $grown = $true
    } finally {
        [MultiboxVolume]::CloseHandle($lock) | Out-Null
        if ($detached) {
            try {
                Run-Diskpart @("select vdisk file=`"$vhdx`"", 'attach vdisk')
            } catch {
                throw "the virtual disk is detached and could not be attached again: $($_.Exception.Message). Restart Windows, or run diskpart with: select vdisk file=`"$vhdx`", attach vdisk"
            }
        }
    }

    # Then the partition and the file system, into the new room.
    $disk = Get-DiskImage -ImagePath $vhdx | Get-Disk
    $partition = Get-Partition -DiskNumber $disk.Number -PartitionNumber $partitionNumber
    if ($letter -and $partition.DriveLetter -ne $letter) {
        $partition | Set-Partition -NewDriveLetter $letter
    }
    try {
        $supported = Get-PartitionSupportedSize -DiskNumber $disk.Number -PartitionNumber $partitionNumber
        if ($supported.SizeMax -gt $partition.Size) {
            Resize-Partition -DiskNumber $disk.Number -PartitionNumber $partitionNumber -Size $supported.SizeMax
        }
    } catch {
        Report $false "the virtual disk now has room for $sizeMb MB, but the volume could not be extended into it: $($_.Exception.Message). Disk Management can extend $root by hand."
        exit
    }
    Report $true 'ok'
} catch {
    Report $false $_.Exception.Message
}
"#;

/// Grows the virtual disk and the volume in it. Windows asks for
/// administrator approval. While it runs the volume is gone for a few
/// seconds, so it refuses unless nothing has a file or folder open there.
#[cfg(windows)]
pub fn grow(plan: &GrowPlan) -> Result<()> {
    let directory = std::env::temp_dir().join(format!("multibox-grow-{}", std::process::id()));
    fs::create_dir_all(&directory)?;
    let script = directory.join("grow.ps1");
    let result = directory.join("result.json");

    // Windows PowerShell reads a BOM-less script as the legacy code page.
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(render_grow_script(plan, &result).as_bytes());
    fs::write(&script, bytes)?;

    let launcher = format!(
        "Start-Process powershell.exe -Verb RunAs -Wait -WindowStyle Hidden -WorkingDirectory {} -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File',{})",
        ps_quote(&directory.display().to_string()),
        ps_quote(&format!("\"{}\"", script.display()))
    );
    // Started from outside the volume, or this very process would be what
    // keeps it in use.
    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-Command", &launcher])
        .current_dir(&directory);
    multibox_clone::no_console_window(&mut command);
    let status = command.status()?;
    let outcome = fs::read_to_string(&result);
    let _ = fs::remove_dir_all(&directory);
    if !status.success() {
        return Err(Error::new(
            "the administrator prompt was declined; nothing was changed",
        ));
    }
    let outcome = outcome.map_err(|_| Error::new("growing finished without reporting a result"))?;
    let value: Value = serde_json::from_str(outcome.trim_start_matches('\u{feff}').trim())
        .map_err(|_| Error::new(format!("growing failed: {}", outcome.trim())))?;
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    match value.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(()),
        _ if message == "in-use" => Err(Error::new(format!(
            "{} is in use: a program has a file or folder open on it, such as an editor, an IDE, a terminal working there or a Cindy session. Close them and try again, from a terminal on another drive; nothing was changed",
            plan.root
        ))),
        _ => Err(Error::new(format!("growing failed: {message}"))),
    }
}

#[cfg(not(windows))]
pub fn grow(_plan: &GrowPlan) -> Result<()> {
    Err(Error::new("only a Dev Drive on Windows can grow"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(root: &str, free: u64, total: u64) -> Volume {
        Volume {
            root: root.to_owned(),
            filesystem: "ReFS".to_owned(),
            reflink: true,
            free,
            total,
            fixed: true,
        }
    }

    fn in_file(free: u64, host_free: u64) -> DiskStatus {
        DiskStatus {
            volume: volume("V:\\", free, 1024 * GIB),
            image: Some(DiskImage {
                file: PathBuf::from("E:\\Multibox\\multibox.vhdx"),
                file_bytes: 200 * GIB,
                host: Some(volume("E:\\", host_free, 2048 * GIB)),
            }),
        }
    }

    #[test]
    fn room_is_bounded_by_the_disk_holding_the_file() {
        assert_eq!(in_file(800 * GIB, 300 * GIB).room(), 300 * GIB);
        assert!(in_file(800 * GIB, 300 * GIB).limited_by_host());
        assert_eq!(in_file(800 * GIB, 900 * GIB).room(), 800 * GIB);
        assert!(!in_file(800 * GIB, 900 * GIB).limited_by_host());
    }

    #[test]
    fn a_real_disk_has_its_own_room() {
        let status = DiskStatus {
            volume: volume("D:\\", 5 * GIB, 10 * GIB),
            image: None,
        };
        assert_eq!(status.room(), 5 * GIB);
        assert!(!status.can_grow());
        assert!(plan_grow(&status, 20).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn growing_needs_a_larger_size() {
        let status = in_file(800 * GIB, 900 * GIB);
        assert!(plan_grow(&status, 1024).is_err());
        assert!(plan_grow(&status, 100).is_err());
        let plan = plan_grow(&status, 1536).unwrap();
        assert_eq!(plan.size_gib, 1536);
        let script = render_grow_script(&plan, Path::new("C:\\result.json"));
        assert!(script.contains("$sizeMb = 1572864"));
        assert!(script.contains("$root = 'V:\\'"));
        assert!(script.contains("FSCTL_LOCK_VOLUME"));
        // Detaching only ever happens after the lock is held.
        assert!(script.find("::Lock(").unwrap() < script.find("'detach vdisk'").unwrap());
    }

    fn sh(dir: &Path, args: &[&str]) {
        git::output(dir, args).unwrap_or_else(|err| panic!("git {args:?}: {err}"));
    }

    /// A project and an original kept beside it, as a move leaves them: the
    /// same commits, files and changes.
    fn moved() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("game");
        fs::create_dir(&original).unwrap();
        sh(&original, &["init", "-q", "-b", "main"]);
        sh(&original, &["config", "user.email", "test@example.com"]);
        sh(&original, &["config", "user.name", "test"]);
        sh(&original, &["config", "core.autocrlf", "false"]);
        fs::write(original.join("a.txt"), "one\n").unwrap();
        sh(&original, &["add", "."]);
        sh(&original, &["commit", "-q", "-m", "first"]);
        fs::write(original.join("a.txt"), "one\nchanged\n").unwrap();
        fs::write(original.join("new.txt"), "untracked\n").unwrap();

        let project = temp.path().join("moved");
        let manifest = multibox_clone::scan(
            &original,
            &multibox_clone::ScanRequest {
                files: Vec::new(),
                links: Vec::new(),
                trees: vec![String::new()],
                filter: &|_: &str, _: bool| multibox_clone::Decision::Include,
                threads: None,
            },
        )
        .unwrap();
        fs::create_dir(&project).unwrap();
        multibox_clone::execute(&manifest, &original, &project, &Default::default()).unwrap();
        let kept = temp.path().join(format!("game{KEPT_SUFFIX}"));
        fs::rename(&original, &kept).unwrap();
        remember_kept(
            &project.join(".git"),
            KeptOriginal {
                path: kept.clone(),
                moved_from: original,
                moved_at: 1,
                bytes: Some(10),
            },
        )
        .unwrap();
        (temp, project, kept)
    }

    #[test]
    fn an_untouched_original_holds_nothing_the_project_lacks() {
        let (_temp, project, kept) = moved();
        assert_eq!(kept_originals(&project).len(), 1);
        assert!(check_kept(&project, &kept).unwrap().is_clear());
    }

    #[test]
    fn work_done_in_the_original_after_the_move_is_found() {
        let (_temp, project, kept) = moved();
        fs::write(kept.join("a.txt"), "edited after the move\n").unwrap();
        assert_eq!(check_kept(&project, &kept).unwrap().differing, ["a.txt"]);

        sh(&kept, &["add", "."]);
        sh(&kept, &["commit", "-q", "-m", "late"]);
        let check = check_kept(&project, &kept).unwrap();
        assert_eq!(check.missing, ["refs/heads/main"]);
    }

    #[test]
    fn only_a_recorded_original_is_deleted() {
        let (temp, project, kept) = moved();
        assert!(delete_kept(&project, &project).is_err());
        let stranger = temp.path().join(format!("other{KEPT_SUFFIX}"));
        fs::create_dir(&stranger).unwrap();
        assert!(delete_kept(&project, &stranger).is_err());
        assert!(stranger.is_dir());

        let report = delete_kept(&project, &kept).unwrap();
        assert!(report.failures.is_empty());
        assert!(!kept.exists());
        assert!(project.join("a.txt").is_file());
        assert!(kept_originals(&project).is_empty());
    }

    #[test]
    fn kept_originals_are_remembered_and_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        let kept = KeptOriginal {
            path: Path::new("projects").join("game.multibox-original"),
            moved_from: Path::new("projects").join("game"),
            moved_at: 1_700_000_000,
            bytes: None,
        };
        remember_kept(common, kept.clone()).unwrap();
        remember_kept(common, kept.clone()).unwrap();
        assert_eq!(read_kept(common), vec![kept]);
        write_kept(common, &[]).unwrap();
        assert!(read_kept(common).is_empty());
        assert!(!common.join(KEPT_FILE).exists());
    }
}
