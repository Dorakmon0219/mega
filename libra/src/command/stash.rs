use clap::{Parser, Subcommand};

use mercury::hash::SHA1;
use mercury::internal::object::commit::Commit;
use mercury::internal::object::tree::Tree;
use mercury::internal::index::Index;

use crate::command::{self, save_object, status};
use crate::internal::head::Head;
use crate::utils::{path, util};
use crate::command::restore::RestoreArgs;
use crate::command::add::AddArgs;
use crate::utils::object_ext::{CommitExt, TreeExt};

#[derive(Debug, Clone)]
struct StashEntry {
    stash_commit: SHA1,
    index_commit: Option<SHA1>,
    message: String,
}

#[derive(Parser, Debug)]
pub struct StashArgs {
    #[command(subcommand)]
    command: Option<StashCommands>,
}

#[derive(Subcommand, Debug)]
enum StashCommands {
    #[command(name = "pop")]
    Pop,
}

const STASH_MESSAGE_PREFIX: &str = "WIP on ";

pub async fn execute(args: StashArgs) {
    if !util::check_repo_exist() {
        return;
    }

    match args.command {
        Some(StashCommands::Pop) => {
            pop_stash().await;
        }
        None => {
            // Default command: create a new stash
            create_stash().await;
        }
    }
}

/// Read the stored stash
async fn get_stash() -> Option<StashEntry> {
    let stash_file = util::storage_path().join("stash");
    if !stash_file.exists() {
        return None;
    }

    match std::fs::read_to_string(&stash_file) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            if lines.is_empty() {
                return None;
            }
            
            let parts: Vec<&str> = lines[0].split(':').collect();
            if parts.len() >= 2 {
                let stash_commit = SHA1::from_str(parts[0]).ok()?;
                let index_commit = if parts[1].is_empty() {
                    None
                } else {
                    Some(SHA1::from_str(parts[1]).ok()?)
                };
                let message = parts.get(2).unwrap_or(&"").to_string();
                Some(StashEntry {
                    stash_commit,
                    index_commit,
                    message,
                })
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

/// Save a stash entry to the stash file
async fn save_stash_entry(entry: &StashEntry) {
    let stash_file = util::storage_path().join("stash");
    let content = format!(
        "{}:{}:{}",
        entry.stash_commit,
        entry.index_commit.unwrap_or_default(),
        entry.message
    );
    
    std::fs::write(stash_file, content).unwrap();
}

/// Remove the stash entry
async fn remove_stash_entry() {
    let stash_file = util::storage_path().join("stash");
    if stash_file.exists() {
        std::fs::remove_file(stash_file).unwrap();
    }
}

/// Create a new stash
async fn create_stash() {
    // Check if there are any changes to stash
    let unstaged = status::changes_to_be_staged();
    let staged = status::changes_to_be_committed().await;
    
    if unstaged.is_empty() && staged.is_empty() {
        println!("No local changes to save");
        return;
    }

    // Get current HEAD
    let head_commit_id = match Head::current_commit().await {
        Some(id) => id,
        None => {
            println!("Cannot stash changes: no commits in the repository");
            return;
        }
    };
    
    let _head_commit = Commit::load(&head_commit_id);
    
    // Generate message based on current branch
    let message = match Head::current().await {
        Head::Branch(branch) => format!("{}{}", STASH_MESSAGE_PREFIX, branch),
        Head::Detached(_) => format!("{}detached HEAD", STASH_MESSAGE_PREFIX),
    };

    // commit the index changes
    let index_commit_id = if !staged.is_empty() {
        
        // Save current index before creating a temporary commit
        let backup_index_path = util::storage_path().join("index.bak");
        std::fs::copy(path::index(), &backup_index_path).unwrap();
        
        // Create a temporary commit with the current index
        let commit_args = command::commit::CommitArgs {
            message: format!("index on {}", message),
            allow_empty: true,
            conventional: false,
            amend: false,
        };
        command::commit::execute(commit_args).await;
        
        // Get the commit ID of the temporary commit
        let temp_commit_id = Head::current_commit().await.unwrap();
        
        // Restore the previous HEAD
        let head_type = Head::current().await;
        match head_type {
            Head::Branch(branch_name) => {
                crate::internal::branch::Branch::update_branch(&branch_name, &head_commit_id.to_string(), None).await;
            },
            Head::Detached(_) => {
                Head::update(Head::Detached(head_commit_id), None).await;
            }
        }
        
        // Restore the original index
        std::fs::copy(&backup_index_path, path::index()).unwrap();
        std::fs::remove_file(backup_index_path).unwrap();
        
        Some(temp_commit_id)
    } else {
        None
    };

    // create a stash with all changes
    let backup_index = Index::load(path::index()).unwrap();
    
    // Stage all unstaged files
    command::add::execute(AddArgs {
        pathspec: vec![],
        all: true,
        update: false,
        verbose: false,
    }).await;
    
    // Save the current HEAD commit ID
    let original_head_commit = Head::current_commit().await.unwrap();
    
    // Create a temporary commit with all changes
    let commit_args = command::commit::CommitArgs {
        message: message.clone(),
        allow_empty: true,
        conventional: false,
        amend: false,
    };
    command::commit::execute(commit_args).await;
    
    // Get the commit ID and tree ID
    let stash_commit_temp = Head::current_commit().await.unwrap();
    let stash_commit_obj = Commit::load(&stash_commit_temp);
    let stash_tree_id = stash_commit_obj.tree_id;
    
    // Create the stash commit with proper parents
    let parents = match index_commit_id {
        Some(id) => vec![head_commit_id, id],
        None => vec![head_commit_id],
    };
    
    let stash_commit = Commit::from_tree_id(
        stash_tree_id,
        parents,
        &message
    );
    save_object(&stash_commit, &stash_commit.id).unwrap();
    
    // Restore the original HEAD
    let head_type = Head::current().await;
    match head_type {
        Head::Branch(branch_name) => {
            crate::internal::branch::Branch::update_branch(&branch_name, &original_head_commit.to_string(), None).await;
        },
        Head::Detached(_) => {
            Head::update(Head::Detached(original_head_commit), None).await;
        }
    }

    // Save stash entry
    let stash_entry = StashEntry {
        stash_commit: stash_commit.id,
        index_commit: index_commit_id,
        message,
    };
    save_stash_entry(&stash_entry).await;

    // Restore index and working tree to HEAD
    let index_file = path::index();
    backup_index.save(&index_file).unwrap();
    
    // Restore working directory to HEAD
    command::restore::execute(RestoreArgs {
        pathspec: vec![util::working_dir_string()],
        source: Some("HEAD".to_string()),
        worktree: true,
        staged: true,
    }).await;

    println!("Saved working directory successfully");
}

/// Apply the stash and remove it
async fn pop_stash() {
    let stash = get_stash().await;
    
    if stash.is_none() {
        println!("No stash entry found");
        return;
    }
    
    let stash = stash.unwrap();
    
    // Check if there are conflicts with current changes
    let unstaged = status::changes_to_be_staged();
    let staged = status::changes_to_be_committed().await;
    
    if !unstaged.is_empty() || !staged.is_empty() {
        println!("Cannot apply stash: local changes would be overwritten");
        println!("Please commit or stash your changes before applying the stash");
        return;
    }
    
    let stash_commit = Commit::load(&stash.stash_commit);
    let stash_tree = Tree::load(&stash_commit.tree_id);
    
    // Create a temporary index with stash contents
    let mut index = Index::load(path::index()).unwrap();
    
    // Add all files from the stash tree to the index
    let stash_files = stash_tree.get_plain_items();
    let storage = util::objects_storage();
    
    for (file_path, file_hash) in stash_files {
        let file_str = file_path.to_str().unwrap().to_string();
        // Get the blob data directly from storage
        let blob_data = storage.get(&file_hash).unwrap();
        
        index.add(mercury::internal::index::IndexEntry::new_from_blob(
            file_str,
            file_hash,
            blob_data.len() as u32
        ));
    }
    
    // Save the updated index
    index.save(&path::index()).unwrap();
    
    // Update the working directory
    command::restore::execute(RestoreArgs {
        pathspec: vec![util::working_dir_string()],
        source: None, // Apply from the index
        worktree: true,
        staged: false,
    }).await;
    
    // Remove the stash entry
    remove_stash_entry().await;
    
    println!("Applied stash: {}", stash.message);
    println!("Dropped stash");
}

// convert string to SHA1
trait FromStr {
    fn from_str(s: &str) -> Result<Self, String> where Self: Sized;
}

impl FromStr for SHA1 {
    fn from_str(s: &str) -> Result<Self, String> {
        if s.len() != 40 && s != "" {
            return Err(format!("Invalid SHA1 hash: {}", s));
        }
        
        if s == "" {
            return Ok(SHA1::default());
        }
        
        let bytes = (0..s.len())
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(&s[i..i + 2], 16)
                    .map_err(|_| format!("Invalid hex character in SHA1: {}", s))
            })
            .collect::<Result<Vec<u8>, _>>()?;
        
        let mut hash = [0u8; 20];
        hash.copy_from_slice(&bytes);
        Ok(SHA1::new(&hash))
    }
}