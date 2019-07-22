use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::{fs, io};

// General download strategy:
// 1: Download the sha256 file (or acquire the needed sha256 for a file)
// 2: Download the file to <name>.part and check sha256 as it downloads
// 3: Only when file is fully download and sha256 verified, move file to <name>
// If the <name> file already exists, don't bother downloading it again
// If downloading fails (sha256 doesn't match), retry downloading up to 5 times.
// If retries run out, keep note of the failure somewhere.
// Also, don't update the channel file unless everything else succeeded.

quick_error! {
    #[derive(Debug)]
    pub enum DownloadError {
        Io(err: io::Error) {
            from()
        }
        Download(err: reqwest::Error) {
            from()
        }
        MismatchedHash(expected: String, actual: String) {}
    }
}

/// Download a URL and return it as a string.
fn download_string(from: &str) -> Result<String, DownloadError> {
    Ok(reqwest::get(from)?.text()?)
}

/// Append a string to a path.
pub fn append_to_path(path: &Path, suffix: &str) -> PathBuf {
    let mut new_path = path.as_os_str().to_os_string();
    new_path.push(suffix);
    PathBuf::from(new_path)
}

/// Write a string to a file, creating directories if needed.
pub fn write_file_create_dir(path: &Path, contents: &str) -> Result<(), DownloadError> {
    let mut res = fs::write(path, contents);

    if let Err(e) = &res {
        if e.kind() == io::ErrorKind::NotFound {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            res = fs::write(path, contents);
        }
    }

    Ok(res?)
}

/// Create a file, creating directories if needed.
pub fn create_file_create_dir(path: &Path) -> Result<File, DownloadError> {
    let mut file_res = File::create(path);
    if let Err(e) = &file_res {
        if e.kind() == io::ErrorKind::NotFound {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            file_res = File::create(path);
        }
    }

    Ok(file_res?)
}

pub fn move_if_exists(from: &Path, to: &Path) -> Result<(), DownloadError> {
    if from.exists() {
        fs::rename(from, to)?;
    }
    Ok(())
}

pub fn move_if_exists_with_sha256(from: &Path, to: &Path) -> Result<(), DownloadError> {
    let sha256_from_path = append_to_path(from, ".sha256");
    let sha256_to_path = append_to_path(to, ".sha256");
    move_if_exists(&sha256_from_path, &sha256_to_path)?;
    move_if_exists(&from, &to)?;
    Ok(())
}

fn one_download(url: &str, path: &Path, hash: Option<&str>) -> Result<(), DownloadError> {
    let mut http_res = reqwest::get(url)?;
    let part_path = append_to_path(path, ".part");
    let mut sha256 = Sha256::new();
    {
        let mut f = create_file_create_dir(&part_path)?;
        let mut buf = [0u8; 65536];
        loop {
            let byte_count = http_res.read(&mut buf)?;
            if byte_count == 0 {
                break;
            }
            if hash.is_some() {
                sha256.write_all(&buf[..byte_count])?;
            }
            f.write_all(&buf[..byte_count])?;
        }
    }

    let f_hash = format!("{:x}", sha256.result());

    if let Some(h) = hash {
        if f_hash == h {
            move_if_exists(&part_path, &path)?;
            Ok(())
        } else {
            Err(DownloadError::MismatchedHash(h.to_string(), f_hash))
        }
    } else {
        fs::rename(part_path, path)?;
        Ok(())
    }
}

/// Download file, verifying its hash, and retrying if needed
pub fn download(
    url: &str,
    path: &Path,
    hash: Option<&str>,
    retries: usize,
    force_download: bool,
) -> Result<(), DownloadError> {
    if path.exists() && !force_download {
        Ok(())
    } else {
        let mut res = Ok(());
        for _ in 0..=retries {
            res = match one_download(url, path, hash) {
                Ok(_) => break,
                Err(e) => {
                    Err(e)
                }
            }
        }
        if res.is_err() {
            return res;
        }
        Ok(())
    }
}

/// Download file and associated .sha256 file, verifying the hash, and retrying if needed
pub fn download_with_sha256_file(
    url: &str,
    path: &Path,
    retries: usize,
    force_download: bool,
) -> Result<(), DownloadError> {
    let sha256_url = format!("{}.sha256", url);
    let sha256_data = download_string(&sha256_url)?;

    let sha256_hash = &sha256_data[..64];
    let res = download(url, path, Some(sha256_hash), retries, force_download);
    if res.is_err() {
        return res;
    }

    let sha256_path = append_to_path(path, ".sha256");
    write_file_create_dir(&sha256_path, &sha256_data)?;

    Ok(())
}
