//! 文件删除权限与启动来源的 Unix 原语；不确定的恢复依据只关闭自动恢复。
#![allow(unsafe_code)]

#[cfg(any(target_os = "linux", target_os = "macos"))]
use super::index_intent::valid_uuid;
use super::index_intent::{BootContext, BootSource};
use std::fs;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::fs::File;
use std::io;
#[cfg(target_os = "linux")]
use std::io::Read;
use std::path::Path;

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};

#[cfg(unix)]
fn access(path: &Path, mode: libc::c_int) -> Result<(), String> {
    let path_c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: CString 提供 NUL 结尾的只读路径；其存储覆盖系统调用的整个生命周期。
    let rc = unsafe { libc::faccessat(libc::AT_FDCWD, path_c.as_ptr(), mode, libc::AT_EACCESS) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!(
            "index permission check failed at {path:?}: {}",
            io::Error::last_os_error()
        ))
    }
}

#[cfg(unix)]
pub(crate) fn check_delete(dir: &Path, file: &Path) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: 凭据与 auxv 查询不解引用调用方内存。
        let ordinary = unsafe {
            libc::getuid() == libc::geteuid()
                && libc::getgid() == libc::getegid()
                && libc::getauxval(libc::AT_SECURE) == 0
        };
        if !ordinary {
            return Err("index deletion preflight requires matching real/effective credentials and a non-secure process".to_owned());
        }
    }
    access(dir, libc::W_OK | libc::X_OK)?;
    let parent = fs::symlink_metadata(dir).map_err(|e| format!("{dir:?}: {e}"))?;
    let child = fs::symlink_metadata(file).map_err(|e| format!("{file:?}: {e}"))?;
    // SAFETY: geteuid 仅返回当前线程的有效用户标识。
    let uid = unsafe { libc::geteuid() };
    if parent.mode() & 0o1000 != 0 && parent.uid() != uid && child.uid() != uid {
        return Err(format!("sticky directory prevents deleting {file:?}"));
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt as MacMetadata;
        let protected = libc::UF_IMMUTABLE | libc::SF_IMMUTABLE | libc::UF_APPEND | libc::SF_APPEND;
        if parent.st_flags() & protected != 0 || child.st_flags() & protected != 0 {
            return Err(format!(
                "immutable or append-only flags prevent deleting {file:?}"
            ));
        }
        // Darwin 的扩展 access mode 与 KAUTH_VNODE_DELETE 对应，普通 W_OK 不检查 delete ACL。
        access(file, 1 << 12)?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn check_delete(_: &Path, _: &Path) -> Result<(), String> {
    Err("index file deletion requires Unix permission preflight".to_owned())
}

pub(crate) fn check_replace(dir: &Path, tmp: &Path, target: &Path) -> Result<(), String> {
    check_delete(dir, tmp)?;
    match fs::symlink_metadata(target) {
        Ok(m) if m.file_type().is_file() => check_delete(dir, target),
        Ok(_) => Err(format!(
            "index metadata target is not a regular file: {target:?}"
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{target:?}: {e}")),
    }
}

pub(crate) fn boot_context(dir: &Path) -> BootContext {
    let epoch = read_epoch();
    let (boot_id, boot_source) = match epoch {
        Ok(value) => value,
        Err(error) => {
            eprintln!("index automatic recovery boot source unavailable: {error}");
            (None, None)
        }
    };
    BootContext {
        boot_id,
        boot_source,
        local: local_fs(dir) && local_fs(&dir.join("partitions/_default/indexes")),
    }
}

#[cfg(target_os = "linux")]
fn read_epoch() -> io::Result<(Option<String>, Option<BootSource>)> {
    let mounts = fs::read_to_string("/proc/self/mountinfo")?;
    for line in mounts.lines() {
        let Some((left, _)) = line.split_once(" - ") else {
            return Err(io::Error::other("invalid mountinfo"));
        };
        let Some(mount) = left.split_whitespace().nth(4) else {
            return Err(io::Error::other("invalid mountinfo"));
        };
        if mount == "/proc/sys/kernel/random/boot_id" {
            return Err(io::Error::other("boot_id is an overridden mount"));
        }
    }
    let mut file = File::open("/proc/sys/kernel/random/boot_id")?;
    if fs_type(&file)? != libc::PROC_SUPER_MAGIC {
        return Err(io::Error::other("boot_id is not backed by procfs"));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let text = text.trim().to_ascii_lowercase();
    if !valid_uuid(&text) {
        return Err(io::Error::other("invalid boot UUID"));
    }
    Ok((Some(text), Some(BootSource::LinuxKernelProcfs)))
}

#[cfg(target_os = "linux")]
fn fs_type(file: &File) -> io::Result<libc::c_long> {
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: fd 存活，stat 提供正确类型、足够大小及对齐的可写存储；只有系统调用成功后读取。
    if unsafe { libc::fstatfs(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: 成功的 fstatfs 已初始化输出结构。
    Ok(unsafe { stat.assume_init() }.f_type)
}
#[cfg(target_os = "linux")]
fn local_fs(path: &Path) -> bool {
    match File::open(path).and_then(|f| fs_type(&f)) {
        Ok(kind) => matches!(
            (kind as u64) & 0xffff_ffff,
            0xef53 | 0x58465342 | 0x9123683e
        ),
        Err(_) => false,
    }
}

#[cfg(target_os = "macos")]
fn read_epoch() -> io::Result<(Option<String>, Option<BootSource>)> {
    let name = c"kern.bootsessionuuid";
    let mut bytes = [0u8; 64];
    let mut len = bytes.len();
    // SAFETY: 名称 NUL 结尾，输出缓冲及 size_t 存储有效；newp 为空，只读取系统属性。
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let text = std::str::from_utf8(
        bytes
            .get(..len)
            .ok_or_else(|| io::Error::other("invalid boot UUID size"))?,
    )
    .map_err(io::Error::other)?
    .trim_end_matches('\0')
    .to_ascii_lowercase();
    if !valid_uuid(&text) {
        return Err(io::Error::other("invalid boot UUID"));
    }
    Ok((Some(text), Some(BootSource::DarwinKernBootsessionuuid)))
}
#[cfg(target_os = "macos")]
fn local_fs(path: &Path) -> bool {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: fd 存活，输出存储正确对齐并覆盖完整 statfs；失败时不读取未初始化内容。
    if unsafe { libc::fstatfs(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: 成功的 fstatfs 已初始化输出结构。
    let stat = unsafe { stat.assume_init() };
    let name: Vec<u8> = stat
        .f_fstypename
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    matches!(name.as_slice(), b"apfs" | b"hfs")
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_epoch() -> io::Result<(Option<String>, Option<BootSource>)> {
    Ok((None, None))
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn local_fs(_: &Path) -> bool {
    false
}
