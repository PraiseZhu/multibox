//! Windows backend, built on `CopyFileExW`. It beats a hand-written copy several
//! times over on small files, and from Windows 11 24H2 it block-clones on ReFS by
//! itself, in one step and twice as fast as issuing the clone by hand. Older Dev
//! Drives get an explicit `FSCTL_DUPLICATE_EXTENTS_TO_FILE`.

use crate::{Times, Volume};
use std::ffi::c_void;
use std::fs::{self, FileTimes, OpenOptions};
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{FileTimesExt, FileTypeExt, OpenOptionsExt, symlink_dir, symlink_file};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_FILE_EXISTS, ERROR_PATH_NOT_FOUND, ERROR_PRIVILEGE_NOT_HELD, GENERIC_READ,
    GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    COPY_FILE_FAIL_IF_EXISTS, CREATE_NEW, CopyFileExW, CreateFileW, FILE_ATTRIBUTE_ARCHIVE,
    FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_INTEGRITY_STREAM, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SPARSE_FILE,
    FILE_ATTRIBUTE_SYSTEM, FILE_BASIC_INFO, FILE_END_OF_FILE_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_SEQUENTIAL_SCAN, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
    FILE_WRITE_ATTRIBUTES, FileBasicInfo, FileEndOfFileInfo, FileStandardInfo, GetDiskFreeSpaceExW,
    GetDiskFreeSpaceW, GetDriveTypeW, GetFileInformationByHandleEx, GetLogicalDrives,
    GetVolumeInformationW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    GetVolumePathNamesForVolumeNameW, OPEN_EXISTING, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

// From winnt.h / winioctl.h; not all of these are exposed by windows-sys.
const FILE_SUPPORTS_BLOCK_REFCOUNTING: u32 = 0x0800_0000;
const FSCTL_DUPLICATE_EXTENTS_TO_FILE: u32 = 0x0009_8344;
const FSCTL_GET_INTEGRITY_INFORMATION: u32 = 0x0009_027C;
const FSCTL_SET_INTEGRITY_INFORMATION: u32 = 0x0009_C280;
const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;

/// Windows 11 24H2, where `CopyFileExW` started cloning on ReFS.
const NATIVE_CLONE_BUILD: u32 = 26100;

/// A single duplicate-extents call must stay under 4 GiB.
const MAX_CLONE_CHUNK: u64 = 1 << 30;

#[repr(C)]
struct DuplicateExtentsData {
    file_handle: HANDLE,
    source_file_offset: i64,
    target_file_offset: i64,
    byte_count: i64,
}

#[repr(C)]
#[derive(Default)]
struct GetIntegrityBuffer {
    checksum_algorithm: u16,
    reserved: u16,
    flags: u32,
    checksum_chunk_size: u32,
    cluster_size: u32,
}

#[repr(C)]
struct SetIntegrityBuffer {
    checksum_algorithm: u16,
    reserved: u16,
    flags: u32,
}

#[derive(Clone, Debug)]
pub struct Capability {
    pub reflink: bool,
    pub same_volume: bool,
    pub filesystem: String,
    pub cluster_size: u32,
    /// `CopyFileExW` clones by itself, so no explicit ioctl is needed.
    pub clones_natively: bool,
}

pub enum Outcome {
    Reflinked,
    Copied,
    Fallback,
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn check(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn existing_ancestor(path: &Path) -> PathBuf {
    let mut current = path;
    loop {
        if current.exists() {
            return current.to_path_buf();
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return path.to_path_buf(),
        }
    }
}

fn volume_root(path: &Path) -> io::Result<Vec<u16>> {
    let path = wide(&existing_ancestor(path));
    let mut root = vec![0u16; path.len().max(64) + 2];
    check(unsafe { GetVolumePathNameW(path.as_ptr(), root.as_mut_ptr(), root.len() as u32) })?;
    let end = root
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(root.len());
    root.truncate(end + 1);
    Ok(root)
}

struct VolumeInfo {
    serial: u32,
    flags: u32,
    filesystem: String,
}

fn volume_info(root: &[u16]) -> io::Result<VolumeInfo> {
    let (mut serial, mut max_name, mut flags) = (0u32, 0u32, 0u32);
    let mut name = [0u16; 64];
    check(unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            null_mut(),
            0,
            &mut serial,
            &mut max_name,
            &mut flags,
            name.as_mut_ptr(),
            name.len() as u32,
        )
    })?;
    let end = name
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(name.len());
    Ok(VolumeInfo {
        serial,
        flags,
        filesystem: String::from_utf16_lossy(&name[..end]),
    })
}

fn cluster_size(root: &[u16]) -> io::Result<u32> {
    let (mut sectors, mut bytes, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
    check(unsafe {
        GetDiskFreeSpaceW(
            root.as_ptr(),
            &mut sectors,
            &mut bytes,
            &mut free,
            &mut total,
        )
    })?;
    Ok(sectors * bytes)
}

/// Reports what the filesystem can do between `src` and `dst`. Either path may
/// not exist yet; the nearest existing ancestor stands in for it.
pub fn probe(src: &Path, dst: &Path) -> io::Result<Capability> {
    let src_root = volume_root(src)?;
    let dst_root = volume_root(dst)?;
    let src_info = volume_info(&src_root)?;
    let dst_info = volume_info(&dst_root)?;
    let same_volume = src_info.serial == dst_info.serial;
    Ok(Capability {
        reflink: same_volume && dst_info.flags & FILE_SUPPORTS_BLOCK_REFCOUNTING != 0,
        same_volume,
        filesystem: dst_info.filesystem,
        cluster_size: cluster_size(&dst_root)?,
        clones_natively: os_build().is_some_and(|(build, _)| build >= NATIVE_CLONE_BUILD),
    })
}

/// Keeps a console program started by `command` from opening a console window
/// of its own. That happens when this process has no console to share, as in a
/// windowed program embedding these crates, which would otherwise flash one
/// for every git it runs. A process with a console is left as it was.
pub fn no_console_window(command: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if unsafe { GetConsoleWindow() }.is_null() {
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Windows build and revision. `ver` is the one source that reports the real
/// version without a manifest, and its digits do not depend on the language.
pub fn os_build() -> Option<(u32, u32)> {
    let output = no_console_window(Command::new("cmd").args(["/c", "ver"]))
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let start = text.find("10.0.")? + "10.0.".len();
    let digits: String = text[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let (build, revision) = digits.split_once('.').unwrap_or((digits.as_str(), "0"));
    Some((build.parse().ok()?, revision.parse().unwrap_or(0)))
}

/// Every lettered volume on the machine. Removable or unready drives are listed
/// only if they answer.
pub fn volumes() -> Vec<Volume> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|bit| mask & (1 << bit) != 0)
        .filter_map(|bit| {
            let root = format!("{}:\\", (b'A' + bit) as char);
            let wide_root: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
            volume_from_root(&wide_root).ok()
        })
        .collect()
}

fn volume_from_root(root: &[u16]) -> io::Result<Volume> {
    const DRIVE_FIXED: u32 = 3;
    let info = volume_info(root)?;
    let (mut free, mut total) = (0u64, 0u64);
    check(unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut free, &mut total, null_mut()) })?;
    let end = root
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(root.len());
    Ok(Volume {
        root: String::from_utf16_lossy(&root[..end]),
        filesystem: info.filesystem,
        reflink: info.flags & FILE_SUPPORTS_BLOCK_REFCOUNTING != 0,
        free,
        total,
        fixed: unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_FIXED,
    })
}

/// The volume holding `path`, also when it is mounted in a folder rather than
/// at a drive letter.
pub fn volume_at(path: &Path) -> io::Result<Volume> {
    volume_from_root(&volume_root(path)?)
}

/// The `\\?\Volume{…}\` name of the volume mounted at `root`.
fn volume_name(root: &[u16]) -> io::Result<Vec<u16>> {
    let mut name = vec![0u16; 64];
    check(unsafe {
        GetVolumeNameForVolumeMountPointW(root.as_ptr(), name.as_mut_ptr(), name.len() as u32)
    })?;
    Ok(name)
}

/// Where a volume named `\\?\Volume{…}\` is mounted: a drive letter if it has
/// one, else its first folder, else the name itself.
fn mount_point(name: &[u16]) -> String {
    let mut paths = vec![0u16; 1024];
    let mut needed = 0u32;
    let found = unsafe {
        GetVolumePathNamesForVolumeNameW(
            name.as_ptr(),
            paths.as_mut_ptr(),
            paths.len() as u32,
            &mut needed,
        )
    } != 0;
    let first = found
        .then(|| {
            paths
                .split(|unit| *unit == 0)
                .find(|path| !path.is_empty())
                .map(String::from_utf16_lossy)
        })
        .flatten();
    first.unwrap_or_else(|| {
        let end = name
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(name.len());
        String::from_utf16_lossy(&name[..end])
    })
}

/// The virtual disk file the volume holding `path` lives in, as with a Dev
/// Drive `git multibox setup` made, or `None` for a volume on a real disk.
/// Asks the volume itself, which needs no administrator rights.
pub fn backing_image(path: &Path) -> io::Result<Option<PathBuf>> {
    use windows_sys::Win32::Storage::Vhd::{
        GET_STORAGE_DEPENDENCY_FLAG_HOST_VOLUMES, GetStorageDependencyInformation,
        STORAGE_DEPENDENCY_INFO, STORAGE_DEPENDENCY_INFO_TYPE_2, STORAGE_DEPENDENCY_INFO_VERSION_2,
    };
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

    let name = volume_name(&volume_root(path)?)?;
    // A volume opens without the name's trailing backslash, and asking
    // about it needs no access to its data.
    let end = name
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(name.len());
    let mut device: Vec<u16> = name[..end].to_vec();
    if device.last() == Some(&u16::from(b'\\')) {
        device.pop();
    }
    device.push(0);
    let raw = unsafe {
        CreateFileW(
            device.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let volume = Handle(raw);

    // Entries point into the same buffer, after the fixed part; u64s keep it
    // aligned for them.
    let mut buffer = vec![0u64; 512];
    loop {
        let info = buffer.as_mut_ptr().cast::<STORAGE_DEPENDENCY_INFO>();
        unsafe { (*info).Version = STORAGE_DEPENDENCY_INFO_VERSION_2 };
        let mut used = 0u32;
        let status = unsafe {
            GetStorageDependencyInformation(
                volume.0,
                GET_STORAGE_DEPENDENCY_FLAG_HOST_VOLUMES,
                (buffer.len() * 8) as u32,
                info,
                &mut used,
            )
        };
        if status == ERROR_INSUFFICIENT_BUFFER && (used as usize) > buffer.len() * 8 {
            buffer = vec![0u64; (used as usize).div_ceil(8)];
            continue;
        }
        if status != 0 {
            // A volume on a real disk depends on no file: Windows answers
            // that with an error rather than an empty list.
            return Ok(None);
        }
        let count = unsafe { (*info).NumberEntries } as usize;
        let entries: *const STORAGE_DEPENDENCY_INFO_TYPE_2 =
            unsafe { (*info).Anonymous.Version2Entries.as_ptr() };
        let text = |pointer: *mut u16| -> Option<Vec<u16>> {
            if pointer.is_null() {
                return None;
            }
            let mut length = 0;
            while unsafe { *pointer.add(length) } != 0 {
                length += 1;
            }
            Some(unsafe { std::slice::from_raw_parts(pointer, length) }.to_vec())
        };
        // The file nearest the volume; a differencing disk has its parents
        // further up.
        let nearest = (0..count)
            .map(|index| unsafe { &*entries.add(index) })
            .min_by_key(|entry| entry.AncestorLevel);
        let Some(entry) = nearest else {
            return Ok(None);
        };
        let (Some(mut host), Some(relative)) = (
            text(entry.HostVolumeName),
            text(entry.DependentVolumeRelativePath),
        ) else {
            return Ok(None);
        };
        // Named without the trailing backslash the lookup of its mount
        // points needs.
        if host.last() != Some(&u16::from(b'\\')) {
            host.push(u16::from(b'\\'));
        }
        host.push(0);
        let mut file = PathBuf::from(mount_point(&host));
        file.push(String::from_utf16_lossy(&relative).trim_start_matches('\\'));
        return Ok(Some(file));
    }
}

/// Bytes available to the caller on the volume holding `path`.
pub fn free_space(path: &Path) -> io::Result<u64> {
    let root = volume_root(path)?;
    let mut available = 0u64;
    check(unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut available, null_mut(), null_mut()) })?;
    Ok(available)
}

fn open_source(path: &Path) -> io::Result<Handle> {
    let path = wide(path);
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_SEQUENTIAL_SCAN,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(Handle(handle))
    }
}

fn create_target(path: &Path) -> io::Result<Handle> {
    let path = wide(path);
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE | FILE_WRITE_ATTRIBUTES,
            0,
            null(),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let err = io::Error::last_os_error();
        // CREATE_NEW reports a missing parent as ERROR_PATH_NOT_FOUND, which
        // must not be mistaken for the source having vanished.
        if err.raw_os_error() == Some(ERROR_PATH_NOT_FOUND as i32) {
            return Err(io::Error::other(format!("target directory missing: {err}")));
        }
        if err.raw_os_error() == Some(ERROR_FILE_EXISTS as i32) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, err));
        }
        Err(err)
    } else {
        Ok(Handle(handle))
    }
}

fn set_length(file: &Handle, length: u64) -> io::Result<()> {
    let info = FILE_END_OF_FILE_INFO {
        EndOfFile: length as i64,
    };
    check(unsafe {
        SetFileInformationByHandle(
            file.0,
            FileEndOfFileInfo,
            &info as *const _ as *const c_void,
            size_of::<FILE_END_OF_FILE_INFO>() as u32,
        )
    })
}

fn ioctl(file: &Handle, code: u32, input: *const c_void, input_len: usize) -> io::Result<()> {
    let mut returned = 0u32;
    check(unsafe {
        DeviceIoControl(
            file.0,
            code,
            input,
            input_len as u32,
            null_mut(),
            0,
            &mut returned,
            null_mut(),
        )
    })
}

fn match_integrity(source: &Handle, target: &Handle) -> io::Result<()> {
    let mut info = GetIntegrityBuffer::default();
    let mut returned = 0u32;
    check(unsafe {
        DeviceIoControl(
            source.0,
            FSCTL_GET_INTEGRITY_INFORMATION,
            null(),
            0,
            &mut info as *mut _ as *mut c_void,
            size_of::<GetIntegrityBuffer>() as u32,
            &mut returned,
            null_mut(),
        )
    })?;
    let set = SetIntegrityBuffer {
        checksum_algorithm: info.checksum_algorithm,
        reserved: 0,
        flags: info.flags,
    };
    ioctl(
        target,
        FSCTL_SET_INTEGRITY_INFORMATION,
        &set as *const _ as *const c_void,
        size_of::<SetIntegrityBuffer>(),
    )
}

fn reflink(
    source: &Handle,
    target: &Handle,
    size: u64,
    attributes: u32,
    cluster: u32,
) -> io::Result<()> {
    if attributes & FILE_ATTRIBUTE_SPARSE_FILE != 0 {
        ioctl(target, FSCTL_SET_SPARSE, null(), 0)?;
    }
    if attributes & FILE_ATTRIBUTE_INTEGRITY_STREAM != 0 {
        match_integrity(source, target)?;
    }
    set_length(target, size)?;
    // The range must be cluster-aligned; rounding past end-of-file is allowed.
    let cluster = u64::from(cluster.max(1));
    let rounded = size.div_ceil(cluster) * cluster;
    let mut offset = 0;
    while offset < rounded {
        let count = (rounded - offset).min(MAX_CLONE_CHUNK);
        let data = DuplicateExtentsData {
            file_handle: source.0,
            source_file_offset: offset as i64,
            target_file_offset: offset as i64,
            byte_count: count as i64,
        };
        ioctl(
            target,
            FSCTL_DUPLICATE_EXTENTS_TO_FILE,
            &data as *const _ as *const c_void,
            size_of::<DuplicateExtentsData>(),
        )?;
        offset += count;
    }
    Ok(())
}

fn reflink_into(source: &Handle, target: &Handle, capability: &Capability) -> io::Result<()> {
    let mut basic: FILE_BASIC_INFO = unsafe { zeroed() };
    let mut standard: FILE_STANDARD_INFO = unsafe { zeroed() };
    check(unsafe {
        GetFileInformationByHandleEx(
            source.0,
            FileBasicInfo,
            &mut basic as *mut _ as *mut c_void,
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    })?;
    check(unsafe {
        GetFileInformationByHandleEx(
            source.0,
            FileStandardInfo,
            &mut standard as *mut _ as *mut c_void,
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
    })?;
    reflink(
        source,
        target,
        standard.EndOfFile as u64,
        basic.FileAttributes,
        capability.cluster_size,
    )?;
    basic.FileAttributes &= FILE_ATTRIBUTE_READONLY
        | FILE_ATTRIBUTE_HIDDEN
        | FILE_ATTRIBUTE_SYSTEM
        | FILE_ATTRIBUTE_ARCHIVE
        | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;
    if basic.FileAttributes == 0 {
        basic.FileAttributes = FILE_ATTRIBUTE_NORMAL;
    }
    check(unsafe {
        SetFileInformationByHandle(
            target.0,
            FileBasicInfo,
            &basic as *const _ as *const c_void,
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    })
}

fn explicit_reflink(src: &Path, dst: &Path, capability: &Capability) -> io::Result<()> {
    let source = open_source(src)?;
    let target = create_target(dst)?;
    let result = reflink_into(&source, &target, capability);
    drop(target);
    if result.is_err() {
        let _ = fs::remove_file(dst);
    }
    result
}

fn system_copy(src: &Path, dst: &Path) -> io::Result<()> {
    let (from, to) = (wide(src), wide(dst));
    let copied = unsafe {
        CopyFileExW(
            from.as_ptr(),
            to.as_ptr(),
            None,
            null(),
            null_mut(),
            COPY_FILE_FAIL_IF_EXISTS,
        )
    };
    match check(copied) {
        Err(err) if err.raw_os_error() == Some(ERROR_FILE_EXISTS as i32) => {
            Err(io::Error::new(io::ErrorKind::AlreadyExists, err))
        }
        other => other,
    }
}

pub fn clone_file(src: &Path, dst: &Path, capability: &Capability) -> io::Result<Outcome> {
    if capability.reflink && !capability.clones_natively {
        match explicit_reflink(src, dst, capability) {
            Ok(()) => return Ok(Outcome::Reflinked),
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(err),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => return Err(err),
            Err(_) => {
                system_copy(src, dst)?;
                return Ok(Outcome::Fallback);
            }
        }
    }
    system_copy(src, dst)?;
    Ok(if capability.reflink {
        Outcome::Reflinked
    } else {
        Outcome::Copied
    })
}

/// Recreates a link. Junctions and directory symlinks fall back to a junction
/// when the account lacks the symlink privilege, which is the default on
/// Windows without Developer Mode. A file symlink has no such fallback.
pub fn create_link(src: &Path, dst: &Path, target: &Path) -> io::Result<()> {
    let is_dir = fs::symlink_metadata(src)
        .map(|meta| meta.file_type().is_symlink_dir())
        .unwrap_or(false);
    if !is_dir {
        return symlink_file(target, dst);
    }
    match symlink_dir(target, dst) {
        Err(err) if err.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD as i32) => {
            make_junction(dst, target)
        }
        other => other,
    }
}

/// Makes `link`, which must not exist yet, a junction to the directory
/// `target`. Junctions need no privilege, unlike directory symlinks.
///
/// The reparse point is written directly rather than with `cmd /c mklink /J`.
/// cmd reads `&`, `|`, `^` and `%` in its arguments as its own syntax, so a
/// folder named `R&D` would have broken the link or, worse, run whatever
/// followed the `&` as a command.
pub fn make_junction(link: &Path, target: &Path) -> io::Result<()> {
    const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    // A junction names an absolute path. A relative target is read from the
    // link's own directory, as a symlink's would be.
    let target = match link.parent() {
        Some(parent) if target.is_relative() => parent.join(target),
        _ => target.to_path_buf(),
    };
    let target = std::path::absolute(&target)?;
    let text = target.to_string_lossy();
    let plain = text
        .strip_prefix(r"\\?\")
        .unwrap_or(&text)
        .replace('/', "\\");
    let print: Vec<u16> = plain.encode_utf16().collect();
    let substitute: Vec<u16> = format!(r"\??\{plain}").encode_utf16().collect();
    // A mount point's REPARSE_DATA_BUFFER: the tag and the length of what
    // follows the 8-byte header, then the offset and length in bytes of the
    // substitute and the print name, then both names, each ending in a NUL.
    let names = (substitute.len() + 1 + print.len() + 1) * 2;
    let length = u16::try_from(8 + names)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "junction target too long"))?;
    let mut buffer = Vec::with_capacity(16 + names);
    buffer.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buffer.extend(length.to_le_bytes());
    buffer.extend(0u16.to_le_bytes());
    for field in [
        0,
        substitute.len() * 2,
        (substitute.len() + 1) * 2,
        print.len() * 2,
    ] {
        buffer.extend((field as u16).to_le_bytes());
    }
    for unit in substitute.iter().chain(&[0]).chain(&print).chain(&[0]) {
        buffer.extend(unit.to_le_bytes());
    }

    fs::create_dir(link)?;
    let made = open_for_reparse(link).and_then(|dir| {
        ioctl(
            &dir,
            FSCTL_SET_REPARSE_POINT,
            buffer.as_ptr().cast(),
            buffer.len(),
        )
    });
    if made.is_err() {
        let _ = fs::remove_dir(link);
    }
    made
}

/// The directory itself, not what a reparse point in it leads to.
fn open_for_reparse(path: &Path) -> io::Result<Handle> {
    let path = wide(path);
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            0,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(Handle(handle))
    }
}

pub fn set_dir_times(path: &Path, times: &Times) -> io::Result<()> {
    let dir = OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let mut update = FileTimes::new().set_modified(times.modified);
    if let Some(created) = times.created {
        update = update.set_created(created);
    }
    dir.set_times(update)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junctions_take_names_that_cmd_would_misread() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("R&D %PATH% ^x");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("inside.txt"), "seen").unwrap();

        let link = temp.path().join("link&echo hi %OS%");
        make_junction(&link, &target).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(link.join("inside.txt")).unwrap(), "seen");
        assert_eq!(
            fs::canonicalize(&link).unwrap(),
            fs::canonicalize(&target).unwrap()
        );

        // Relative, from the link's own directory, as a symlink is read.
        let relative = temp.path().join("relative");
        make_junction(&relative, Path::new("R&D %PATH% ^x")).unwrap();
        assert_eq!(
            fs::read_to_string(relative.join("inside.txt")).unwrap(),
            "seen"
        );

        // Removing a junction leaves what it led to.
        fs::remove_dir(&link).unwrap();
        assert!(target.join("inside.txt").exists());
    }

    #[test]
    fn a_junction_never_replaces_an_existing_directory() {
        let temp = tempfile::tempdir().unwrap();
        let link = temp.path().join("link");
        fs::create_dir(&link).unwrap();
        assert!(make_junction(&link, temp.path()).is_err());
        assert!(link.is_dir());
    }
}
