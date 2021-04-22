use error::GitDataStoreError;
use git2::{FileMode, IndexEntry, IndexTime, MergeOptions, Oid, Reference, Repository, Signature};
use parking_lot::Mutex;
use serde::Serialize;
use std::path::Path;

pub mod clone;
pub mod commit;
pub mod commit_to_branch;
pub mod error;
pub mod log;
pub mod route;

pub struct GitDataStore {
    repo_path: String,
    primary_branch: String,
    mutex: Mutex<()>,
}

#[derive(Serialize)]
pub struct GitEntry {
    data: GitData,
    commit_id: String,
}
#[derive(Serialize)]
pub enum GitData {
    Dir { entries: Vec<String> },
    File { data: String },
}

impl GitDataStore {
    pub fn new(repo_path: &str, primary_branch: &str) -> Self {
        GitDataStore {
            repo_path: repo_path.to_string(),
            primary_branch: primary_branch.to_string(),
            mutex: Mutex::new(()),
        }
    }

    pub fn read_latest(&self, path: &str) -> Result<GitEntry, GitDataStoreError> {
        let repo = Repository::open(&self.repo_path)?;
        let main_ref = repo.find_reference(&format!("refs/heads/{}", self.primary_branch))?;
        let commit = main_ref.peel_to_commit()?;

        read_entry_from_tree(&repo, &commit, path)
    }

    pub fn read(&self, commit_id: &str, path: &str) -> Result<GitEntry, GitDataStoreError> {
        let repo = Repository::open(&self.repo_path)?;

        // TODO allow rev spec
        let commit = repo
            .find_commit(
                Oid::from_str(commit_id)
                    .map_err(|_e| GitDataStoreError::RevNotFound(commit_id.to_string()))?,
            )
            .map_err(|_e| GitDataStoreError::RevNotFound(commit_id.to_string()))?;

        read_entry_from_tree(&repo, &commit, path)
    }

    pub fn put(
        &self,
        parent_rev_id: &str,
        path: &str,
        data: &str,
        resolve_conflicts_in_our_favor: bool,
    ) -> Result<String, GitDataStoreError> {
        // get index, create commit from parent commit
        // merge commit with current commit of primary branch
        // update primary branch
        let repo = Repository::open(&self.repo_path)?;

        let parent_rev = repo.revparse_single(parent_rev_id)?;

        let parent_commit = match parent_rev.kind().expect("no kind on parent rev") {
            git2::ObjectType::Commit => parent_rev
                .as_commit()
                .expect("parent_rev commit is not commit")
                .to_owned(),
            git2::ObjectType::Tag => parent_rev
                .as_tag()
                .expect("parent_rev tag is not tag")
                .target()?
                .as_commit()
                .expect("parent_rev tag target not commit")
                .to_owned(),
            _ => panic!("Unexpected parent_rev type"),
        };

        let mut index = repo.index()?;
        index.add_frombuffer(&make_index_entry(&path), data.as_bytes())?;

        let tree_oid = index.write_tree()?;
        let tree = repo.find_tree(tree_oid)?;

        let author_commiter = signature();

        let commit_id = repo.commit(
            None,
            &author_commiter,
            &author_commiter,
            "Update",
            &tree,
            &[&parent_commit],
        )?;

        // lock mutex
        let _mutex = self.mutex.lock();
        let mut main_ref = repo.find_reference(&format!("refs/heads/{}", self.primary_branch))?;

        let head_commit = main_ref.peel_to_commit()?;

        if head_commit.id() == parent_commit.id() {
            // update-ref of main branch
            main_ref.set_target(commit_id, "updated primary branch with new commit")?;
            Ok(commit_id.to_string())
        } else {
            // merge commits
            let our_commit = repo.find_commit(commit_id)?;
            let mut merge_options = MergeOptions::new();

            if resolve_conflicts_in_our_favor {
                merge_options.file_favor(git2::FileFavor::Ours);
            }

            let mut merge_index =
                repo.merge_commits(&our_commit, &head_commit, Some(&merge_options))?;

            if merge_index.has_conflicts() {
                // TODO add error
                panic!("index has conflicts");
            }

            let merge_tree_id = merge_index.write_tree_to(&repo)?;
            let merge_tree = repo.find_tree(merge_tree_id)?;

            let merge_commit_id = repo.commit(
                Some(&format!("refs/heads/{}", self.primary_branch)),
                &author_commiter,
                &author_commiter,
                "Merge Commit",
                &merge_tree,
                &[&head_commit, &our_commit],
            )?;

            Ok(merge_commit_id.to_string())
        }
    }

    pub fn put_latest(&self, path: &str, data: &str) -> Result<String, GitDataStoreError> {
        let repo = Repository::open(&self.repo_path).expect("open repo");

        let _mutex = self.mutex.lock();
        let main_ref = repo.find_reference(&format!("refs/heads/{}", self.primary_branch))?;

        let head_commit = main_ref.peel_to_commit()?;

        let mut index = repo.index()?;
        index.add_frombuffer(&make_index_entry(&path), data.as_bytes())?;

        let tree_oid = index.write_tree()?;
        let tree = repo.find_tree(tree_oid)?;

        let author_commiter = signature();

        let commit_id = repo.commit(
            Some(&format!("refs/head/{}", self.primary_branch)),
            &author_commiter,
            &author_commiter,
            "Update latest",
            &tree,
            &[&head_commit],
        )?;
        Ok(commit_id.to_string())
    }
}

pub fn make_index_entry(path: &str) -> IndexEntry {
    IndexEntry {
        ctime: IndexTime::new(0, 0),
        mtime: IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode: FileMode::Blob.into(),
        uid: 0,
        gid: 0,
        file_size: 0,
        id: Oid::from_bytes(&[0; 20]).unwrap(),
        flags: 0,
        flags_extended: 0,
        path: path.into(),
    }
}

fn signature() -> Signature<'static> {
    Signature::now("GitDataStore", "gitdatastore@email.com").expect("Failed creating Signature")
}

pub fn create_branch<'repo>(
    repo: &'repo Repository,
    branch_name: &str,
    oid: Oid,
) -> Result<Reference<'repo>, GitDataStoreError> {
    Ok(repo.reference(
        &format!("refs/heads/{}", branch_name),
        oid,
        false,
        "creating branch",
    )?)
}

fn read_entry_from_tree(
    repo: &Repository,
    commit: &git2::Commit,
    path: &str,
) -> Result<GitEntry, GitDataStoreError> {
    let tree = commit.tree()?;
    let entry = tree
        .get_path(Path::new(path))
        .map_err(|e| GitDataStoreError::PathNotFound(path.to_string(), e))?;

    let git_data = match entry.kind().expect("tree entry does not have kind") {
        git2::ObjectType::Tree => {
            let obj = entry.to_object(&repo)?;
            let tree = obj.as_tree().expect("tree is not a tree");
            GitData::Dir {
                entries: tree
                    .into_iter()
                    .filter_map(|e| e.name().map(|name| name.to_string()))
                    .collect(),
            }
        }
        git2::ObjectType::Blob => {
            let obj = entry.to_object(&repo)?;
            let blob = obj.as_blob().expect("blob is not blob");

            // Should non-utf8 data be returned as base-64 encoded?
            GitData::File {
                data: String::from_utf8(blob.content().to_owned()).map_err(|_e| {
                    GitDataStoreError::NonUtf8Blob {
                        commit_id: commit.id().to_string(),
                        path: path.to_string(),
                    }
                })?,
            }
        }
        _ => {
            unreachable!("Impossible entry.kind() {:?}", entry.kind())
        }
    };

    Ok(GitEntry {
        data: git_data,
        commit_id: commit.id().to_string(),
    })
}
