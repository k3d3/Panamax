use serde::Serialize;
use std::{io, num::TryFromIntError, path::Path};

use git2::{
    build::{CheckoutBuilder, RepoBuilder},
    FetchOptions, RemoteCallbacks, Repository, Signature,
};
use thiserror::Error;

use crate::mirror::ConfigCrates;
use crate::progress_bar::{padded_prefix_message, progress_bar, ProgressBarMessage};

#[derive(Error, Debug)]
pub enum IndexSyncError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("JSON serialization error: {0}")]
    SerializeError(#[from] serde_json::Error),
    #[error("Git error: {0}")]
    GitError(#[from] git2::Error),
    #[error("Number conversion error: {0}")]
    IntegerConversionError(#[from] TryFromIntError),
}

#[derive(Debug, Serialize)]
struct ConfigJson {
    dl: String,
    api: String,
}

/// Synchronize the crates.io-index repository.
///
/// `mirror_path`: Root path to the mirror directory.
///
/// `crates`: The crates section of the `mirror.toml` config file.
pub fn sync_crates_repo(mirror_path: &Path, crates: &ConfigCrates) -> Result<(), IndexSyncError> {
    let repo_path = mirror_path.join("crates.io-index");

    // Set up progress bar piping.
    let prefix = padded_prefix_message(1, 3, "Fetching crates.io-index");
    let (pb_thread, sender) = progress_bar(None, prefix);

    // Libgit2 has callbacks that allow us to update the progress bar
    // as the git download progresses.
    let mut remote_callbacks = RemoteCallbacks::new();
    remote_callbacks.transfer_progress(|p| {
        sender
            .send(ProgressBarMessage::SetProgress(
                p.indexed_objects(),
                p.total_objects(),
            ))
            .expect("Channel send should not fail");
        true
    });
    let mut fetch_opts = FetchOptions::new();
    fetch_opts.remote_callbacks(remote_callbacks);

    if !repo_path.join(".git").exists() {
        clone_repository(fetch_opts, &crates.source_index, &repo_path)?
    } else {
        // Get (fetch) the branch's latest remote "master" commit
        let repo = Repository::open(&repo_path)?;
        let mut remote = repo.find_remote("origin")?;
        remote.fetch(&["master"], Some(&mut fetch_opts), None)?;

        // Set master to origin/master.
        //
        // Note that this means config.json changes will have to be rewritten on every sync.
        fast_forward(&repo_path)?;
    }

    sender
        .send(ProgressBarMessage::Done)
        .expect("Channel send should not fail");
    pb_thread.join().expect("Thread join should not fail");

    Ok(())
}

/// Update the config.json file within crates-io.index.
pub fn update_crates_config(
    mirror_path: &Path,
    crates: &ConfigCrates,
) -> Result<(), IndexSyncError> {
    let repo_path = mirror_path.join("crates.io-index");

    if let Some(base_url) = &crates.base_url {
        rewrite_config_json(&repo_path, base_url)?;
    }

    Ok(())
}

/// Perform a git fast-forward on the repository. This will destroy any local changes that have
/// been made to the repo, and will make the local master identical to the remote master.
fn fast_forward(repo_path: &Path) -> Result<(), IndexSyncError> {
    let repo = Repository::open(repo_path)?;

    let fetch_head = repo.find_reference("refs/remotes/origin/master")?;
    let fetch_commit = repo.reference_to_annotated_commit(&fetch_head)?;

    // Force fast-forward on master
    let refname = "refs/heads/master";
    match repo.find_reference(refname) {
        Ok(mut r) => {
            r.set_target(fetch_commit.id(), "Performing fast-forward")?;
        }
        Err(_) => {
            // Remote branch doesn't exist, so use commit directly
            repo.reference(refname, fetch_commit.id(), true, "Performing fast-forward")?;
        }
    }

    // Set the "HEAD" reference to our new master commit.
    repo.set_head(refname)?;

    // Checkout the repo directory (so the files are actually created on disk).
    repo.checkout_head(Some(
        CheckoutBuilder::default().allow_conflicts(true).force(),
    ))?;

    Ok(())
}

/// Clone a repository from scratch. This assumes the path does not exist.
fn clone_repository(
    fetch_opts: FetchOptions,
    source_index: &str,
    repo_path: &Path,
) -> Result<(), IndexSyncError> {
    let mut repo_builder = RepoBuilder::new();
    repo_builder.fetch_options(fetch_opts);
    repo_builder.clone(source_index, repo_path)?;
    Ok(())
}

/// Fast-forward master, then rewrite the crates.io-index config.json.
pub fn rewrite_config_json(repo_path: &Path, base_url: &str) -> Result<(), IndexSyncError> {
    let repo = Repository::open(repo_path)?;
    let refname = "refs/heads/master";
    let signature = Signature::now("Panamax", "panamax@panamax")?;

    eprintln!("{}", padded_prefix_message(3, 3, "Syncing config"));

    let mut index = repo.index()?;

    // Create the new config.json.
    let config_json = ConfigJson {
        dl: base_url.to_string(),
        api: base_url.to_string(),
    };
    let contents = serde_json::to_vec_pretty(&config_json)?;
    std::fs::write(repo_path.join("config.json"), contents)?;

    // Add config.json into the working index.
    // (a.k.a. "git add")
    index.add_path(Path::new("config.json"))?;
    let oid = index.write_tree()?;
    index.write()?;

    // Get the master commit's tree.
    let master = repo.find_reference(refname)?;
    let parent_commit = master.peel_to_commit()?;
    let tree = repo.find_tree(oid)?;

    // Commit this change to the repository.
    repo.commit(
        Some(refname),
        &signature,
        &signature,
        "Rewrite config.json",
        &tree,
        &[&parent_commit],
    )?;

    Ok(())
}
