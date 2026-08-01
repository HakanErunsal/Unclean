//! Provides platform replacement primitives behind one narrow shared boundary.

use std::fs;
use std::io;
use std::path::Path;

/// Replaces an existing target with a same-volume file while retaining platform metadata.
pub(crate) fn replace_file(target: &Path, replacement: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        replace_file_windows(target, replacement)
    }
    #[cfg(not(windows))]
    {
        fs::rename(replacement, target)
    }
}

/// Moves a same-directory file into a target path that does not exist.
pub(crate) fn install_file(target: &Path, replacement: &Path) -> io::Result<()> {
    fs::rename(replacement, target)
}

/// Copies one file into an absent recovery path with Windows attributes and security properties.
#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) fn copy_file(source: &Path, destination: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::CopyFileW;

    let source = windows_path(source)?;
    let destination = windows_path(destination)?;
    // SAFETY: Both path buffers end with one NUL, remain alive for the call, and contain no interior NUL.
    if unsafe { CopyFileW(source.as_ptr(), destination.as_ptr(), 1) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Copies one file into an absent recovery path on non-Windows test hosts.
#[cfg(not(windows))]
pub(crate) fn copy_file(source: &Path, destination: &Path) -> io::Result<()> {
    if destination.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "backup destination already exists",
        ));
    }
    fs::copy(source, destination).map(|_| ())
}

/// Changes the Windows read-only file attribute without changing access-control entries.
#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) fn set_readonly(path: &Path, readonly: bool) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_READONLY, GetFileAttributesW, INVALID_FILE_ATTRIBUTES, SetFileAttributesW,
    };

    let path = windows_path(path)?;
    // SAFETY: The path buffer ends with one NUL, remains alive for both calls, and contains no interior NUL.
    let attributes = unsafe { GetFileAttributesW(path.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        return Err(io::Error::last_os_error());
    }
    let updated = if readonly {
        attributes | FILE_ATTRIBUTE_READONLY
    } else {
        attributes & !FILE_ATTRIBUTE_READONLY
    };
    // SAFETY: The path buffer has the same valid lifetime and encoding checked for GetFileAttributesW.
    if unsafe { SetFileAttributesW(path.as_ptr(), updated) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Changes write permission for the owner while retaining other Unix mode bits.
#[cfg(unix)]
pub(crate) fn set_readonly(path: &Path, readonly: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path)?;
    let mut permissions = metadata.permissions();
    let mode = permissions.mode();
    permissions.set_mode(if readonly {
        mode & !0o222
    } else {
        mode | 0o200
    });
    fs::set_permissions(path, permissions)
}

#[cfg(not(any(windows, unix)))]
pub(crate) fn set_readonly(_path: &Path, _readonly: bool) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this platform does not support read-only attribute updates",
    ))
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn replace_file_windows(target: &Path, replacement: &Path) -> io::Result<()> {
    use std::ptr;

    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    let target = windows_path(target)?;
    let replacement = windows_path(replacement)?;
    // SAFETY: Both path buffers end with one NUL, remain alive for the call, and contain no interior NUL; reserved pointers stay null as required by ReplaceFileW.
    let result = unsafe {
        ReplaceFileW(
            target.as_ptr(),
            replacement.as_ptr(),
            ptr::null(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn windows_path(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    const BACKSLASH: u16 = b'\\' as u16;
    const QUESTION: u16 = b'?' as u16;
    let raw = path
        .as_os_str()
        .encode_wide()
        .map(|character| {
            if character == u16::from(b'/') {
                BACKSLASH
            } else {
                character
            }
        })
        .collect::<Vec<_>>();
    if raw.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains an interior NUL",
        ));
    }
    let extended_prefix = [BACKSLASH, BACKSLASH, QUESTION, BACKSLASH];
    let mut output = if raw.starts_with(&extended_prefix) {
        raw
    } else if raw.starts_with(&[BACKSLASH, BACKSLASH]) {
        let mut output = extended_prefix.to_vec();
        output.extend("UNC\\".encode_utf16());
        output.extend_from_slice(&raw[2..]);
        output
    } else if path.is_absolute() {
        let mut output = extended_prefix.to_vec();
        output.extend(raw);
        output
    } else {
        raw
    };
    output.push(0);
    Ok(output)
}

#[cfg(all(test, windows))]
mod tests {
    use std::error::Error;
    use std::fs;

    use tempfile::tempdir;

    use super::{install_file, replace_file};

    #[test]
    fn replacement_keeps_the_target_path_and_consumes_the_temporary_file()
    -> Result<(), Box<dyn Error>> {
        let temp = tempdir()?;
        let target = temp.path().join("target.txt");
        let replacement = temp.path().join("replacement.txt");
        fs::write(&target, b"before")?;
        fs::write(&replacement, b"after")?;

        replace_file(&target, &replacement)?;

        assert_eq!(fs::read(&target)?, b"after");
        assert!(!replacement.exists());
        Ok(())
    }

    #[test]
    fn install_moves_a_file_into_an_absent_target() -> Result<(), Box<dyn Error>> {
        let temp = tempdir()?;
        let target = temp.path().join("target.txt");
        let replacement = temp.path().join("replacement.txt");
        fs::write(&replacement, b"after")?;

        install_file(&target, &replacement)?;

        assert_eq!(fs::read(&target)?, b"after");
        assert!(!replacement.exists());
        Ok(())
    }
}
