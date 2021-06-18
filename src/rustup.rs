use crate::download::{
    append_to_path, copy_file_create_dir_with_sha256, download, download_with_sha256_file,
    move_if_exists, move_if_exists_with_sha256, write_file_create_dir, DownloadError,
};
use crate::mirror::{ConfigMirror, ConfigRustup, MirrorError};
use crate::progress_bar::{
    current_step_prefix, padded_prefix_message, progress_bar, ProgressBarMessage,
};
use console::style;
use reqwest::header::HeaderValue;
use scoped_threadpool::Pool;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{fs, io};
use thiserror::Error;

// The allowed platforms to validate the configuration
// Note: These platforms should match the list on https://rust-lang.github.io/rustup/installation/other.html

/// Unix platforms
static PLATFORMS_UNIX: &[&str] = &[
    "aarch64-fuschia",
    "aarch64-linux-android",
    "aarch64-pc-windows-msvc",
    "aarch64-unknown-hermit",
    "aarch64-unknown-linux-gnu",
    "aarch64-unknown-none",
    "aarch64-unknown-none-softfloat",
    "aarch64-unknown-redox",
    "arm-linux-androideabi",
    "arm-unknown-linux-gnueabi",
    "arm-unknown-linux-gnueabihf",
    "arm-unknown-linux-musleabi",
    "arm-unknown-linux-musleabihf",
    "armebv7r-none-eabi",
    "armebv7r-none-eabihf",
    "armv5te-unknown-linux-gnueabi",
    "armv5te-unknown-linux-musleabi",
    "armv7-apple-ios",
    "armv7-linux-androideabi",
    "armv7-unknown-linux-gnueabi",
    "armv7-unknown-linux-gnueabihf",
    "armv7s-apple-ios",
    "asmjs-unknown-emscripten",
    "i386-apple-ios",
    "i586-pc-windows-msvc",
    "i586-unknown-linux-gnu",
    "i586-unknown-linux-musl",
    "i686-apple-darwin",
    "i686-linux-android",
    "i686-unknown-freebsd",
    "i686-unknown-linux-gnu",
    "i686-unknown-linux-musl",
    "mips-unknown-linux-gnu",
    "mips64-unknown-linux-gnuabi64",
    "mips64-unknown-linux-muslabi64",
    "mips64el-unknown-linux-gnuabi64",
    "mips64el-unknown-linux-muslabi64",
    "mipsel-unknown-linux-gnu",
    "mipsisa32r6el-unknown-linux-gnu",
    "mipsisa64r6-unknown-linux-gnuabi64",
    "mipsisa64r6el-unknown-linux-gnuabi64",
    "nvptx64-nvidia-cuda",
    "powerpc-unknown-linux-gnu",
    "powerpc64-unknown-linux-gnu",
    "powerpc64le-unknown-linux-gnu",
    "riscv32gc-unknown-linux-gnu",
    "riscv32i-unknown-none-elf",
    "riscv32imac-unknown-none-elf",
    "riscv32imc-unknown-none-elf",
    "riscv64gc-unknown-none-elf",
    "riscv64imac-unknown-none-elf",
    "s390x-unknown-linux-gnu",
    "sparc64-unknown-linux-gnu",
    "sparcv9-sun-solaris",
    "thumbv6m-none-eabi",
    "thumbv7em-none-eabi",
    "thumbv7neon-linux-androideabi",
    "thumbv7neon-unknown-linux-gnueabihf",
    "wasm32-unknown-emscripten",
    "wasm32-unknown-unknown",
    "wasm32-wasi",
    "x86_64-apple-darwin",
    "x86_64-apple-ios",
    "x86_64-fortanix-unknown-sgx",
    "x86_64-fuschia",
    "x86_64-linux-android",
    "x86_64-pc-solaris",
    "x86_64-rumprun-netbsd",
    "x86_64-sun-solaris",
    "x86_64-unknown-freebsd",
    "x86_64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnux32",
    "x86_64-unknown-linux-musl",
    "x86_64-unknown-netbsd",
    "x86_64-unknown-redox",
];

/// Windows platforms (platforms where rustup-init has a .exe extension)
static PLATFORMS_WINDOWS: &[&str] = &[
    "i586-pc-windows-msvc",
    "i686-pc-windows-gnu",
    "i686-pc-windows-msvc",
    "x86_64-pc-windows-gnu",
    "x86_64-pc-windows-msvc",
];
#[derive(Error, Debug)]
pub enum SyncError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Download error: {0}")]
    Download(#[from] DownloadError),
    #[error("TOML deserialization error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("TOML serialization error: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("Path prefix strip error: {0}")]
    StripPrefix(#[from] std::path::StripPrefixError),
    #[error("Failed {count} downloads")]
    FailedDownloads { count: usize },
}

#[derive(Deserialize, Debug)]
struct TargetUrls {
    url: String,
    hash: String,
    xz_url: String,
    xz_hash: String,
}

#[derive(Deserialize, Debug)]
struct Target {
    available: bool,

    #[serde(flatten)]
    target_urls: Option<TargetUrls>,
}

#[derive(Deserialize, Debug)]
struct Pkg {
    version: String,
    target: HashMap<String, Target>,
}

#[derive(Deserialize, Debug)]
struct Channel {
    #[serde(alias = "manifest-version")]
    manifest_version: String,
    date: String,
    pkg: HashMap<String, Pkg>,
}

#[derive(Deserialize, Debug)]
struct Release {
    #[serde(alias = "schema-version")]
    schema_version: String,
    version: String,
}

#[derive(Deserialize, Debug)]
pub struct Platforms {
    unix: Vec<String>,
    windows: Vec<String>,
}

pub fn get_platforms(rustup: &ConfigRustup) -> Result<Platforms, MirrorError> {
    let unix = match &rustup.platforms_unix {
        Some(p) => {
            let bad_platforms: Vec<&String> = p
                .iter()
                .filter(|x| !PLATFORMS_UNIX.contains(&x.as_str()))
                .collect();
            if !bad_platforms.is_empty() {
                eprintln!("Bad values in unix platforms: {:?}", bad_platforms);
                return Err(MirrorError::Config(
                    "bad value for 'platforms_unix'".to_string(),
                ));
            }
            p.clone()
        }
        None => {
            eprintln!("Info: no 'platforms_unix' specified in 'rustup' section of 'mirror.toml', mirroring all platforms.");
            PLATFORMS_UNIX.iter().map(|x| x.to_string()).collect()
        }
    };
    let windows = match &rustup.platforms_windows {
        Some(p) => {
            let bad_platforms: Vec<&String> = p
                .iter()
                .filter(|x| !PLATFORMS_WINDOWS.contains(&x.as_str()))
                .collect();
            if !bad_platforms.is_empty() {
                eprintln!("Bad values in windows platforms: {:?}", bad_platforms);
                return Err(MirrorError::Config(
                    "bad value for 'platforms_windows'".to_string(),
                ));
            }
            p.clone()
        }
        None => {
            eprintln!("Info: no 'platforms_windows' specified in 'rustup' section of 'mirror.toml', mirroring all platforms.");
            PLATFORMS_WINDOWS.iter().map(|x| x.to_string()).collect()
        }
    };
    Ok(Platforms { unix, windows })
}

/// Synchronize one rustup-init file.
pub fn sync_one_init(
    path: &Path,
    source: &str,
    platform: &str,
    is_exe: bool,
    rustup_version: &str,
    retries: usize,
    user_agent: &HeaderValue,
) -> Result<(), DownloadError> {
    let local_path = path
        .join("rustup")
        .join("archive")
        .join(rustup_version)
        .join(platform)
        .join(if is_exe {
            "rustup-init.exe"
        } else {
            "rustup-init"
        });

    let archive_path = path.join("rustup/dist").join(platform).join(if is_exe {
        "rustup-init.exe"
    } else {
        "rustup-init"
    });

    let source_url = if is_exe {
        format!("{}/rustup/dist/{}/rustup-init.exe", source, platform)
    } else {
        format!("{}/rustup/dist/{}/rustup-init", source, platform)
    };

    download_with_sha256_file(&source_url, &local_path, retries, false, user_agent)?;

    copy_file_create_dir_with_sha256(&local_path, &archive_path)?;

    Ok(())
}

/// Synchronize all rustup-init files.
pub fn sync_rustup_init(
    path: &Path,
    source: &str,
    prefix: String,
    threads: usize,
    retries: usize,
    user_agent: &HeaderValue,
    platforms: &Platforms,
) -> Result<(), SyncError> {
    let count = platforms.unix.len() + platforms.windows.len();

    let (pb_thread, sender) = progress_bar(Some(count), prefix);

    let errors_occurred = AtomicUsize::new(0);

    // Download rustup release file
    let release_url = format!("{}/rustup/release-stable.toml", source);
    let release_path = path.join("rustup/release-stable.toml");
    let release_part_path = append_to_path(&release_path, ".part");

    download(
        &release_url,
        &release_part_path,
        None,
        retries,
        false,
        user_agent,
    )?;

    let rustup_version = get_rustup_version(&release_part_path)?;

    move_if_exists(&release_part_path, &release_path)?;

    Pool::new(threads as u32).scoped(|scoped| {
        let error_occurred = &errors_occurred;
        for platform in &platforms.unix {
            let s = sender.clone();
            let rustup_version = rustup_version.clone();
            scoped.execute(move || {
                if let Err(e) = sync_one_init(
                    path,
                    source,
                    platform.as_str(),
                    false,
                    &rustup_version,
                    retries,
                    user_agent,
                ) {
                    match e {
                        DownloadError::NotFound {
                            status: _,
                            url: _,
                            data: _,
                        } => {}
                        _ => {
                            s.send(ProgressBarMessage::Println(format!(
                                "Downloading {} failed: {:?}",
                                path.display(),
                                e
                            )))
                            .expect("Channel send should not fail");
                            error_occurred.fetch_add(1, Ordering::Release);
                        }
                    }
                }
                s.send(ProgressBarMessage::Increment)
                    .expect("Channel send should not fail");
            })
        }

        for platform in &platforms.windows {
            let s = sender.clone();
            let rustup_version = rustup_version.clone();
            scoped.execute(move || {
                if let Err(e) = sync_one_init(
                    path,
                    source,
                    platform,
                    true,
                    &rustup_version,
                    retries,
                    user_agent,
                ) {
                    match e {
                        DownloadError::NotFound {
                            status: _,
                            url: _,
                            data: _,
                        } => {}
                        _ => {
                            s.send(ProgressBarMessage::Println(format!(
                                "Downloading {} failed: {:?}",
                                path.display(),
                                e
                            )))
                            .expect("Channel send should not fail");
                            error_occurred.fetch_add(1, Ordering::Release);
                        }
                    }
                }
                s.send(ProgressBarMessage::Increment)
                    .expect("Channel send should not fail");
            })
        }
    });

    sender
        .send(ProgressBarMessage::Done)
        .expect("Channel send should not fail");
    pb_thread.join().expect("Thread join should not fail");

    let errors = errors_occurred.load(Ordering::Acquire);
    if errors == 0 {
        Ok(())
    } else {
        Err(SyncError::FailedDownloads { count: errors })
    }
}

/// Get the rustup file downloads, in pairs of URLs and sha256 hashes.
pub fn rustup_download_list(
    path: &Path,
    download_dev: bool,
    platforms: &Platforms,
) -> Result<(String, Vec<(String, String)>), SyncError> {
    let channel_str = fs::read_to_string(path).map_err(DownloadError::Io)?;
    let channel: Channel = toml::from_str(&channel_str)?;

    Ok((
        channel.date,
        channel
            .pkg
            .into_iter()
            .filter(|(pkg_name, _)| download_dev || pkg_name == "rustc-dev")
            .flat_map(|(_, pkg)| {
                pkg.target
                    .into_iter()
                    .filter(|(name, _)| {
                        platforms.unix.contains(&name)
                            || platforms.windows.contains(&name)
                            || name == "*" // The * platform contains rust-src, always download
                    })
                    .flat_map(|(_, target)| -> Vec<(String, String)> {
                        target
                            .target_urls
                            .map(|urls| vec![(urls.url, urls.hash), (urls.xz_url, urls.xz_hash)])
                            .into_iter()
                            .flatten()
                            .map(|(url, hash)| {
                                (url.split('/').collect::<Vec<&str>>()[3..].join("/"), hash)
                            })
                            .collect()
                    })
            })
            .collect(),
    ))
}

pub fn sync_one_rustup_target(
    path: &Path,
    source: &str,
    url: &str,
    hash: &str,
    retries: usize,
    user_agent: &HeaderValue,
) -> Result<(), DownloadError> {
    // Chop off the source portion of the URL, to mimic the rest of the path
    //let target_url = path.join(url[source.len()..].trim_start_matches("/"));
    let target_url = format!("{}/{}", source, url);
    let target_path = path.join(url);

    download(
        &target_url,
        &target_path,
        Some(hash),
        retries,
        false,
        user_agent,
    )?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChannelHistoryFile {
    pub versions: HashMap<String, Vec<String>>,
}

pub fn latest_dates_from_channel_history(
    channel_history: &ChannelHistoryFile,
    versions: usize,
) -> Vec<String> {
    let mut dates: Vec<String> = channel_history
        .versions
        .keys()
        .map(|x| x.to_string())
        .collect();
    dates.sort();
    dates.reverse();
    dates.truncate(versions);
    dates
}

pub fn clean_old_files(
    path: &Path,
    keep_stables: Option<usize>,
    keep_betas: Option<usize>,
    keep_nightlies: Option<usize>,
    pinned_rust_versions: Option<&Vec<String>>,
    prefix: String,
) -> Result<(), SyncError> {
    // Handle all of stable/beta/nightly
    let mut files_to_keep: HashSet<String> = HashSet::new();
    if let Some(s) = keep_stables {
        let mut stable = get_channel_history(path, "stable")?;
        let latest_dates = latest_dates_from_channel_history(&stable, s);
        for date in latest_dates {
            if let Some(t) = stable.versions.get_mut(&date) {
                t.iter().for_each(|t| {
                    files_to_keep.insert(t.to_string());
                });
            }
        }
    }
    if let Some(b) = keep_betas {
        let mut beta = get_channel_history(path, "beta")?;
        let latest_dates = latest_dates_from_channel_history(&beta, b);
        for date in latest_dates {
            if let Some(t) = beta.versions.get_mut(&date) {
                t.iter().for_each(|t| {
                    files_to_keep.insert(t.to_string());
                });
            }
        }
    }
    if let Some(n) = keep_nightlies {
        let mut nightly = get_channel_history(path, "nightly")?;
        let latest_dates = latest_dates_from_channel_history(&nightly, n);
        for date in latest_dates {
            if let Some(t) = nightly.versions.get_mut(&date) {
                t.iter().for_each(|t| {
                    files_to_keep.insert(t.to_string());
                });
            }
        }
    }
    if let Some(pinned_versions) = pinned_rust_versions {
        for version in pinned_versions {
            let mut pinned = get_channel_history(path, &version)?;
            let latest_dates = latest_dates_from_channel_history(&pinned, 1);
            for date in latest_dates {
                if let Some(t) = pinned.versions.get_mut(&date) {
                    t.iter().for_each(|t| {
                        files_to_keep.insert(t.to_string());
                    });
                }
            }
        }
    }

    let dist_path = path.join("dist");
    let mut files_to_delete: Vec<String> = vec![];

    for dir in fs::read_dir(dist_path)? {
        let dir = dir?.path();
        if dir.is_dir() {
            for full_path in fs::read_dir(dir)? {
                let full_path = full_path?.path();
                let file_path = full_path.strip_prefix(path)?;
                if let Some(file_path) = file_path.to_str() {
                    if !files_to_keep.contains(file_path) {
                        files_to_delete.push(file_path.to_string());
                    }
                }
            }
        }
    }

    // Progress bar!
    let (pb_thread, sender) = progress_bar(Some(files_to_delete.len()), prefix);

    for f in files_to_delete {
        if let Err(e) = fs::remove_file(path.join(&f)) {
            sender
                .send(ProgressBarMessage::Println(format!(
                    "Could not remove file {}: {:?}",
                    f, e
                )))
                .expect("Channel send should not fail");
        }
        sender
            .send(ProgressBarMessage::Increment)
            .expect("Channel send should not fail");
    }

    sender
        .send(ProgressBarMessage::Done)
        .expect("Channel send should not fail");
    pb_thread.join().expect("Thread join should not fail");

    Ok(())
}

pub fn get_channel_history(path: &Path, channel: &str) -> Result<ChannelHistoryFile, SyncError> {
    let channel_history_path = path.join(format!("mirror-{}-history.toml", channel));
    if channel_history_path.exists() {
        let ch_data = fs::read_to_string(channel_history_path)?;
        Ok(toml::from_str(&ch_data)?)
    } else {
        Ok(ChannelHistoryFile {
            versions: HashMap::new(),
        })
    }
}

pub fn add_to_channel_history(
    path: &Path,
    channel: &str,
    date: &str,
    files: &[(String, String)],
) -> Result<(), SyncError> {
    let mut channel_history = get_channel_history(path, channel)?;
    channel_history.versions.insert(
        date.to_string(),
        files.iter().map(|(f, _)| f.to_string()).collect(),
    );

    let ch_data = toml::to_string(&channel_history)?;

    let channel_history_path = path.join(format!("mirror-{}-history.toml", channel));
    write_file_create_dir(&channel_history_path, &ch_data)?;

    Ok(())
}

/// Get the current rustup version from release-stable.toml.
pub fn get_rustup_version(path: &Path) -> Result<String, SyncError> {
    let release_data: Release = toml::from_str(&fs::read_to_string(path)?)?;
    Ok(release_data.version)
}

/// Synchronize a rustup channel (stable, beta, or nightly).
#[allow(clippy::too_many_arguments)]
pub fn sync_rustup_channel(
    path: &Path,
    source: &str,
    threads: usize,
    prefix: String,
    channel: &str,
    retries: usize,
    user_agent: &HeaderValue,
    download_dev: bool,
    platforms: &Platforms,
) -> Result<(), SyncError> {
    // Download channel file
    let channel_url = format!("{}/dist/channel-rust-{}.toml", source, channel);
    let channel_path = path.join(format!("dist/channel-rust-{}.toml", channel));
    let channel_part_path = append_to_path(&channel_path, ".part");
    download_with_sha256_file(&channel_url, &channel_part_path, retries, true, user_agent)?;

    // Open toml file, find all files to download
    let (date, files) = rustup_download_list(&channel_part_path, download_dev, &platforms)?;

    // Create progress bar
    let (pb_thread, sender) = progress_bar(Some(files.len()), prefix);

    let errors_occurred = AtomicUsize::new(0);

    // Download files
    Pool::new(threads as u32).scoped(|scoped| {
        let error_occurred = &errors_occurred;
        for (url, hash) in &files {
            let s = sender.clone();
            scoped.execute(move || {
                if let Err(e) =
                    sync_one_rustup_target(&path, &source, &url, &hash, retries, user_agent)
                {
                    s.send(ProgressBarMessage::Println(format!(
                        "Downloading {} failed: {:?}",
                        path.display(),
                        e
                    )))
                    .expect("Channel send should not fail");
                    error_occurred.fetch_add(1, Ordering::Release);
                }
                s.send(ProgressBarMessage::Increment)
                    .expect("Channel send should not fail");
            })
        }
    });

    // Wait for progress bar to finish
    sender
        .send(ProgressBarMessage::Done)
        .expect("Channel send should not fail");
    pb_thread.join().expect("Thread join should not fail");

    let errors = errors_occurred.load(Ordering::Acquire);
    if errors == 0 {
        // Write channel history file
        add_to_channel_history(path, channel, &date, &files)?;
        move_if_exists_with_sha256(&channel_part_path, &channel_path)?;
        Ok(())
    } else {
        Err(SyncError::FailedDownloads { count: errors })
    }
}

/// Synchronize rustup.
pub fn sync(
    path: &Path,
    mirror: &ConfigMirror,
    rustup: &ConfigRustup,
    user_agent: &HeaderValue,
) -> Result<(), MirrorError> {
    let platforms = get_platforms(&rustup)?;
    // Default to not downloading rustc-dev
    let download_dev = rustup.download_dev.unwrap_or(false);

    let num_pinned_versions = rustup.pinned_rust_versions.as_ref().map_or(0, |v| v.len());
    let num_steps = 1 + // sync rustup-init
                    1 + 1 + 1 + // sync latest stable, beta, nightly
                    num_pinned_versions + // sync pinned rust versions
                    1; // clean old files
    let mut step = 0;

    eprintln!("{}", style("Syncing Rustup repositories...").bold());

    // Mirror rustup-init
    step += 1;
    let prefix = padded_prefix_message(step, num_steps, "Syncing rustup-init files");
    if let Err(e) = sync_rustup_init(
        path,
        &rustup.source,
        prefix,
        rustup.download_threads,
        mirror.retries,
        user_agent,
        &platforms,
    ) {
        eprintln!("Downloading rustup init files failed: {:?}", e);
        eprintln!("You will need to sync again to finish this download.");
    }

    let mut failures = false;

    // Mirror stable
    step += 1;
    if rustup.keep_latest_stables != Some(0) {
        let prefix = padded_prefix_message(step, num_steps, "Syncing latest stable");
        if let Err(e) = sync_rustup_channel(
            path,
            &rustup.source,
            rustup.download_threads,
            prefix,
            "stable",
            mirror.retries,
            user_agent,
            download_dev,
            &platforms,
        ) {
            failures = true;
            eprintln!("Downloading stable release failed: {:?}", e);
            eprintln!("You will need to sync again to finish this download.");
        }
    } else {
        eprintln!(
            "{} Skipping syncing stable.",
            current_step_prefix(step, num_steps)
        );
    }

    // Mirror beta
    step += 1;
    if rustup.keep_latest_betas != Some(0) {
        let prefix = padded_prefix_message(step, num_steps, "Syncing latest beta");
        if let Err(e) = sync_rustup_channel(
            path,
            &rustup.source,
            rustup.download_threads,
            prefix,
            "beta",
            mirror.retries,
            user_agent,
            download_dev,
            &platforms,
        ) {
            failures = true;
            eprintln!("Downloading beta release failed: {:?}", e);
            eprintln!("You will need to sync again to finish this download.");
        }
    } else {
        eprintln!(
            "{} Skipping syncing beta.",
            current_step_prefix(step, num_steps)
        );
    }

    // Mirror nightly
    step += 1;
    if rustup.keep_latest_nightlies != Some(0) {
        let prefix = padded_prefix_message(step, num_steps, "Syncing latest nightly");
        if let Err(e) = sync_rustup_channel(
            path,
            &rustup.source,
            rustup.download_threads,
            prefix,
            "nightly",
            mirror.retries,
            user_agent,
            download_dev,
            &platforms,
        ) {
            failures = true;
            eprintln!("Downloading nightly release failed: {:?}", e);
            eprintln!("You will need to sync again to finish this download.");
        }
    } else {
        eprintln!(
            "{} Skipping syncing nightly.",
            current_step_prefix(step, num_steps)
        );
    }

    // Mirror pinned rust versions
    if let Some(pinned_versions) = &rustup.pinned_rust_versions {
        for version in pinned_versions {
            step += 1;
            let prefix =
                padded_prefix_message(step, num_steps, &format!("Syncing pinned rust {}", version));
            if let Err(e) = sync_rustup_channel(
                path,
                &rustup.source,
                rustup.download_threads,
                prefix,
                &version,
                mirror.retries,
                user_agent,
                download_dev,
                &platforms,
            ) {
                failures = true;
                if let SyncError::Download(DownloadError::NotFound { .. }) = e {
                    eprintln!(
                        "{} Pinned rust version {} could not be found.",
                        current_step_prefix(step, num_steps),
                        version
                    );
                    return Err(MirrorError::Config(format!(
                        "Pinned rust version {} could not be found",
                        version
                    )));
                } else {
                    eprintln!("Downloading pinned rust {} failed: {:?}", version, e);
                    eprintln!("You will need to sync again to finish this download.");
                }
            }
        }
    }

    // If all succeeds, clean files
    step += 1;
    if rustup.keep_latest_stables == None
        && rustup.keep_latest_betas == None
        && rustup.keep_latest_nightlies == None
    {
        eprintln!(
            "{} Skipping cleaning files.",
            current_step_prefix(step, num_steps)
        );
    } else if failures {
        eprintln!(
            "{} Skipping cleaning files due to download failures.",
            current_step_prefix(step, num_steps)
        );
    } else {
        let prefix = padded_prefix_message(step, num_steps, "Cleaning old files");
        if let Err(e) = clean_old_files(
            path,
            rustup.keep_latest_stables,
            rustup.keep_latest_betas,
            rustup.keep_latest_nightlies,
            rustup.pinned_rust_versions.as_ref(),
            prefix,
        ) {
            eprintln!("Cleaning old files failed: {:?}", e);
            eprintln!("You may need to sync again to clean these files.");
        }
    }

    eprintln!("{}", style("Syncing Rustup repositories complete!").bold());

    Ok(())
}
