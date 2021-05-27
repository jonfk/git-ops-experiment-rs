use error::GitDataStoreError;
use git2::{
    Commit, DiffOptions, FileMode, Index, IndexEntry, IndexTime, Oid, Reference, Repository,
    Signature,
};
use history::HistoryIterator;
use parking_lot::Mutex;
use serde::Serialize;
use std::path::Path;

pub mod clone;
pub mod commit;
pub mod commit_to_branch;
pub mod error;
pub mod history;
pub mod route;

pub struct GitDataStore {
    repo_path: String,
    primary_branch: String,
    mutex: Mutex<()>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct GitEntry {
    pub data: GitData,
    pub commit_id: String,
}
#[derive(Debug, Serialize, PartialEq, Eq)]
pub enum GitData {
    Dir { entries: Vec<String> },
    File { data: String },
}

impl GitData {
    pub fn is_dir(&self) -> bool {
        if let GitData::Dir { .. } = self {
            true
        } else {
            false
        }
    }

    pub fn is_file(&self) -> bool {
        if let GitData::File { .. } = self {
            true
        } else {
            false
        }
    }

    pub fn file(&self) -> Option<&str> {
        if let GitData::File { data } = self {
            Some(data)
        } else {
            None
        }
    }
}

impl GitDataStore {
    pub fn new(repo_path: &str, primary_branch: &str) -> Self {
        GitDataStore {
            repo_path: repo_path.to_string(),
            primary_branch: primary_branch.to_string(),
            mutex: Mutex::new(()),
        }
    }

    pub fn read_latest(&self, path: &str) -> Result<Option<GitEntry>, GitDataStoreError> {
        let repo = Repository::open(&self.repo_path)?;
        let main_ref = repo.find_reference(&format!("refs/heads/{}", self.primary_branch))?;
        let commit = main_ref.peel_to_commit()?;

        read_entry_from_tree(&repo, &commit, path)
    }

    pub fn read(&self, commit_id: &str, path: &str) -> Result<Option<GitEntry>, GitDataStoreError> {
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
        overwrite: bool,
    ) -> Result<String, GitDataStoreError> {
        // get last commit from primary branch and parent commit
        // if they are the same or the overwrite flag is set, create new commit with that as parent and update primary branch
        // if they are not the same, diff between the 2 commits and check that path hasn't been updated since parent commit
        // if it has been updated, create conflict error
        let repo = Repository::open(&self.repo_path)?;

        let parent_rev = repo.revparse_single(parent_rev_id)?;
        let parent_commit = parent_rev.peel_to_commit()?;

        // lock mutex
        let _mutex = self.mutex.lock();
        let main_ref = repo.find_reference(&format!("refs/heads/{}", self.primary_branch))?;

        let head_commit = main_ref.peel_to_commit()?;

        if head_commit.id() != parent_commit.id()
            && !overwrite
            && has_conflict(&repo, path, &parent_commit, &head_commit)?
        {
            return Err(GitDataStoreError::ConflictOnWrite {
                path: path.to_string(),
                parent_commit_id: parent_commit.id().to_string(),
            });
        }

        let tree_oid = self.create_tree(&repo, path, data, &head_commit)?;
        let tree = repo.find_tree(tree_oid)?;

        let author_commiter = signature();

        let commit_id = repo.commit(
            Some(&format!("refs/heads/{}", self.primary_branch)),
            &author_commiter,
            &author_commiter,
            &format!("Update {}", path),
            &tree,
            &[&head_commit],
        )?;
        Ok(commit_id.to_string())
    }

    pub fn put_latest(&self, path: &str, data: &str) -> Result<String, GitDataStoreError> {
        let repo = Repository::open(&self.repo_path)?;

        let _mutex = self.mutex.lock();
        let main_ref = repo.find_reference(&format!("refs/heads/{}", self.primary_branch))?;

        let head_commit = main_ref.peel_to_commit()?;

        let tree_oid = self.create_tree(&repo, path, data, &head_commit)?;

        let tree = repo.find_tree(tree_oid)?;
        let author_commiter = signature();

        let commit_id = repo.commit(
            Some(&format!("refs/heads/{}", self.primary_branch)),
            &author_commiter,
            &author_commiter,
            "Update latest",
            &tree,
            &[&head_commit],
        )?;
        Ok(commit_id.to_string())
    }

    pub fn history(&self) -> Result<HistoryIterator, GitDataStoreError> {
        let repo = Repository::open(&self.repo_path)?;
        history::git_log(repo)
    }

    pub fn delete(
        &self,
        parent_rev_id: &str,
        path: &str,
        overwrite: bool,
    ) -> Result<String, GitDataStoreError> {
        let repo = Repository::open(&self.repo_path)?;

        let parent_rev = repo.revparse_single(parent_rev_id)?;
        let parent_commit = parent_rev.peel_to_commit()?;

        let _mutex = self.mutex.lock();
        let main_ref = repo.find_reference(&format!("refs/heads/{}", self.primary_branch))?;

        let head_commit = main_ref.peel_to_commit()?;

        if head_commit.id() != parent_commit.id()
            && !overwrite
            && has_conflict(&repo, path, &parent_commit, &head_commit)?
        {
            return Err(GitDataStoreError::ConflictOnWrite {
                path: path.to_string(),
                parent_commit_id: parent_commit.id().to_string(),
            });
        }

        let mut index = Index::new()?;
        index.read_tree(&head_commit.tree()?)?;
        repo.set_index(&mut index)?;

        // https://libgit2.org/libgit2/#HEAD/type/git_index_stage_t
        index.remove(&Path::new(path), -1)?;

        let tree_oid = index.write_tree_to(&repo)?;

        let tree = repo.find_tree(tree_oid)?;
        let author_commiter = signature();

        let commit_id = repo.commit(
            Some(&format!("refs/heads/{}", self.primary_branch)),
            &author_commiter,
            &author_commiter,
            "delete",
            &tree,
            &[&head_commit],
        )?;
        Ok(commit_id.to_string())
    }

    pub fn delete_latest(&self, path: &str) -> Result<String, GitDataStoreError> {
        let repo = Repository::open(&self.repo_path)?;

        let _mutex = self.mutex.lock();
        let main_ref = repo.find_reference(&format!("refs/heads/{}", self.primary_branch))?;

        let head_commit = main_ref.peel_to_commit()?;

        let mut index = Index::new()?;
        index.read_tree(&head_commit.tree()?)?;
        repo.set_index(&mut index)?;

        // https://libgit2.org/libgit2/#HEAD/type/git_index_stage_t
        index.remove(&Path::new(path), -1)?;

        let tree_oid = index.write_tree_to(&repo)?;

        let tree = repo.find_tree(tree_oid)?;
        let author_commiter = signature();

        let commit_id = repo.commit(
            Some(&format!("refs/heads/{}", self.primary_branch)),
            &author_commiter,
            &author_commiter,
            "Delete latest",
            &tree,
            &[&head_commit],
        )?;
        Ok(commit_id.to_string())
    }

    fn create_tree(
        &self,
        repo: &Repository,
        path: &str,
        data: &str,
        head_commit: &Commit,
    ) -> Result<Oid, GitDataStoreError> {
        let mut index = Index::new()?;
        index.read_tree(&head_commit.tree()?)?;
        repo.set_index(&mut index)?;
        index.add_frombuffer(&make_index_entry(&path), data.as_bytes())?;

        let tree_oid = index.write_tree_to(&repo)?;
        Ok(tree_oid)
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
) -> Result<Option<GitEntry>, GitDataStoreError> {
    let tree = commit.tree()?;

    let entry = match tree.get_path(Path::new(path)) {
        Ok(entry) => Some(entry),
        Err(err) => {
            println!("{:?}", err);
            None
        },
    };

    entry
        .map(|entry| {
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
        })
        .transpose()
}

fn has_conflict(
    repo: &Repository,
    path: &str,
    parent_commit: &Commit,
    head_commit: &Commit,
) -> Result<bool, GitDataStoreError> {
    let mut diff_options = DiffOptions::new();
    diff_options.pathspec(path);
    let diff = repo.diff_tree_to_tree(
        Some(&parent_commit.tree()?),
        Some(&head_commit.tree()?),
        Some(&mut diff_options),
    )?;

    Ok(diff.deltas().len() != 0)
}
