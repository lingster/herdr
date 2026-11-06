use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const STAGED_CLIPBOARD_IMAGE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub(crate) struct StagedClipboardImage {
    pub(crate) path: PathBuf,
    pub(crate) paste_text: String,
}

pub(crate) fn stage(
    dir: &Path,
    client_id: u64,
    extension: &str,
    data: &[u8],
) -> io::Result<StagedClipboardImage> {
    let extension = sanitize_extension(extension);
    ensure_staging_dir(dir).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "clipboard image staging directory {} is unusable: {err}",
                dir.display()
            ),
        )
    })?;
    cleanup_stale(dir);

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);

    for attempt in 0..100 {
        let path = dir.join(format!(
            "client-{client_id}-clipboard-{unique}-{attempt}.{extension}"
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        restrict_file_options(&mut options);
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        };
        file.write_all(data)?;
        return Ok(StagedClipboardImage {
            paste_text: path.to_string_lossy().into_owned(),
            path,
        });
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to allocate unique clipboard image staging path",
    ))
}

pub(crate) fn remove_files(paths: Vec<PathBuf>) {
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

fn sanitize_extension(extension: &str) -> &'static str {
    if extension.eq_ignore_ascii_case("png") {
        "png"
    } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        "jpg"
    } else if extension.eq_ignore_ascii_case("gif") {
        "gif"
    } else if extension.eq_ignore_ascii_case("webp") {
        "webp"
    } else if extension.eq_ignore_ascii_case("bmp") {
        "bmp"
    } else {
        "png"
    }
}

fn ensure_staging_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let metadata = fs::metadata(dir)?;
    if !metadata.is_dir() {
        return Err(io::Error::other("path is not a directory"));
    }
    verify_dir_owner(dir, &metadata)?;
    restrict_dir_permissions(dir)
}

#[cfg(unix)]
fn verify_dir_owner(dir: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    ensure_owned_by(dir, metadata.uid(), euid)
}

#[cfg(windows)]
fn verify_dir_owner(_dir: &Path, _metadata: &fs::Metadata) -> io::Result<()> {
    Ok(())
}

/// A staging directory owned by another account could be read or swapped by
/// that account, and chmod on it fails with EPERM; reject it explicitly.
#[cfg(any(unix, test))]
fn ensure_owned_by(dir: &Path, owner_uid: u32, expected_uid: u32) -> io::Result<()> {
    if owner_uid == expected_uid {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "{} is owned by uid {owner_uid}, not the current user (uid {expected_uid}); \
             remove it or set server.clipboard_image_directory",
            dir.display()
        ),
    ))
}

#[cfg(unix)]
fn restrict_file_options(options: &mut fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.mode(0o600);
}

#[cfg(windows)]
fn restrict_file_options(_options: &mut fs::OpenOptions) {}

#[cfg(unix)]
fn restrict_dir_permissions(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

#[cfg(windows)]
fn restrict_dir_permissions(_dir: &Path) -> io::Result<()> {
    Ok(())
}

fn cleanup_stale(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified.elapsed().unwrap_or_default() > STAGED_CLIPBOARD_IMAGE_MAX_AGE {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_parent(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "herdr-clipboard-image-tests-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn stage_writes_image_into_the_configured_directory() {
        let parent = temp_parent("configured");
        let dir = parent.join("nested").join("clipboard-images");

        let staged = stage(&dir, 7, "png", b"image-bytes").unwrap();

        assert_eq!(staged.path.parent(), Some(dir.as_path()));
        assert_eq!(staged.paste_text, staged.path.to_string_lossy());
        assert_eq!(fs::read(&staged.path).unwrap(), b"image-bytes");
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn stage_error_names_the_directory_that_failed() {
        let parent = temp_parent("not-a-dir");
        let blocker = parent.join("clipboard-images");
        fs::write(&blocker, b"not a directory").unwrap();

        let err = match stage(&blocker, 7, "png", b"image-bytes") {
            Ok(_) => panic!("staging into a file path must fail"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains(&blocker.display().to_string()),
            "error should name the staging directory: {err}"
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn staging_dir_owned_by_another_user_is_rejected_with_its_path() {
        let dir = Path::new("/tmp/herdr-clipboard-images-1000");

        let err = ensure_owned_by(dir, 1001, 1000).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let message = err.to_string();
        assert!(
            message.contains("/tmp/herdr-clipboard-images-1000"),
            "{message}"
        );
        assert!(message.contains("1001"), "{message}");
    }

    #[test]
    fn staging_dir_owned_by_current_user_is_accepted() {
        assert!(ensure_owned_by(Path::new("/tmp/x"), 1000, 1000).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn stage_restricts_directory_and_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let parent = temp_parent("permissions");
        let dir = parent.join("clipboard-images");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();

        let staged = stage(&dir, 7, "png", b"image-bytes").unwrap();

        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let file_mode = fs::metadata(&staged.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn sanitize_extension_accepts_known_image_extensions() {
        assert_eq!(sanitize_extension("PNG"), "png");
        assert_eq!(sanitize_extension("jpeg"), "jpg");
        assert_eq!(sanitize_extension("webp"), "webp");
        assert_eq!(sanitize_extension("sh"), "png");
    }
}
