use std::collections::BTreeMap;
use std::fmt::Display;
use std::fs;
use std::fs::{DirEntry, FileType, Metadata};
#[cfg(target_family = "unix")]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

use crossbeam_channel::Sender;
use fun_time::fun_time;
use log::debug;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::common::{check_if_stop_received, prepare_thread_handler_common, send_info_and_wait_for_ending_all_threads};
use crate::common_directory::Directories;
use crate::common_extensions::Extensions;
use crate::common_items::ExcludedItems;
use crate::common_tool::CommonToolData;
use crate::common_traits::ResultEntry;
use crate::flc;
use crate::progress_data::{CurrentStage, ProgressData};

#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub enum ToolType {
    Duplicate,
    EmptyFolders,
    EmptyFiles,
    InvalidSymlinks,
    BrokenFiles,
    BadExtensions,
    BigFile,
    SameMusic,
    SimilarImages,
    SimilarVideos,
    TemporaryFiles,
    #[default]
    None,
}

#[derive(PartialEq, Eq, Clone, Debug, Copy, Default, Deserialize, Serialize)]
pub enum CheckingMethod {
    #[default]
    None,
    Name,
    SizeName,
    Size,
    Hash,
    AudioTags,
    AudioContent,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    pub modified_date: u64,
}

impl ResultEntry for FileEntry {
    fn get_path(&self) -> &Path {
        &self.path
    }
    fn get_modified_date(&self) -> u64 {
        self.modified_date
    }
    fn get_size(&self) -> u64 {
        self.size
    }
}

// Symlinks

#[derive(Clone, Debug, PartialEq, Eq, Copy, Deserialize, Serialize)]
pub enum ErrorType {
    InfiniteRecursion,
    NonExistentFile,
}

impl Display for ErrorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InfiniteRecursion => write!(f, "Infinite recursion"),
            Self::NonExistentFile => write!(f, "Non existent file"),
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum Collect {
    InvalidSymlinks,
    Files,
}

#[derive(Eq, PartialEq, Copy, Clone, Debug)]
enum EntryType {
    File,
    Dir,
    Symlink,
    Other,
}

pub struct DirTraversalBuilder<'a, 'b, F> {
    group_by: Option<F>,
    root_dirs: Vec<PathBuf>,
    stop_flag: Option<&'a Arc<AtomicBool>>,
    progress_sender: Option<&'b Sender<ProgressData>>,
    minimal_file_size: Option<u64>,
    maximal_file_size: Option<u64>,
    checking_method: CheckingMethod,
    collect: Collect,
    recursive_search: bool,
    directories: Option<Directories>,
    excluded_items: Option<ExcludedItems>,
    extensions: Option<Extensions>,
    tool_type: ToolType,
}

pub struct DirTraversal<'a, 'b, F> {
    group_by: F,
    root_dirs: Vec<PathBuf>,
    stop_flag: Option<&'a Arc<AtomicBool>>,
    progress_sender: Option<&'b Sender<ProgressData>>,
    recursive_search: bool,
    directories: Directories,
    excluded_items: ExcludedItems,
    extensions: Extensions,
    minimal_file_size: u64,
    maximal_file_size: u64,
    checking_method: CheckingMethod,
    tool_type: ToolType,
    collect: Collect,
}

impl Default for DirTraversalBuilder<'_, '_, ()> {
    fn default() -> Self {
        Self::new()
    }
}

impl DirTraversalBuilder<'_, '_, ()> {
    pub fn new() -> Self {
        DirTraversalBuilder {
            group_by: None,
            root_dirs: vec![],
            stop_flag: None,
            progress_sender: None,
            checking_method: CheckingMethod::None,
            minimal_file_size: None,
            maximal_file_size: None,
            collect: Collect::Files,
            recursive_search: false,
            directories: None,
            extensions: None,
            excluded_items: None,
            tool_type: ToolType::None,
        }
    }
}

impl<'a, 'b, F> DirTraversalBuilder<'a, 'b, F> {
    pub(crate) fn common_data(mut self, common_tool_data: &CommonToolData) -> Self {
        self.root_dirs = common_tool_data.directories.included_directories.clone();
        self.extensions = Some(common_tool_data.extensions.clone());
        self.excluded_items = Some(common_tool_data.excluded_items.clone());
        self.recursive_search = common_tool_data.recursive_search;
        self.minimal_file_size = Some(common_tool_data.minimal_file_size);
        self.maximal_file_size = Some(common_tool_data.maximal_file_size);
        self.tool_type = common_tool_data.tool_type;
        self.directories = Some(common_tool_data.directories.clone());
        self
    }

    pub(crate) fn stop_flag(mut self, stop_flag: &'a Arc<AtomicBool>) -> Self {
        self.stop_flag = Some(stop_flag);
        self
    }

    pub(crate) fn progress_sender(mut self, progress_sender: Option<&'b Sender<ProgressData>>) -> Self {
        self.progress_sender = progress_sender;
        self
    }

    pub(crate) fn checking_method(mut self, checking_method: CheckingMethod) -> Self {
        self.checking_method = checking_method;
        self
    }

    pub(crate) fn minimal_file_size(mut self, minimal_file_size: u64) -> Self {
        self.minimal_file_size = Some(minimal_file_size);
        self
    }

    pub(crate) fn maximal_file_size(mut self, maximal_file_size: u64) -> Self {
        self.maximal_file_size = Some(maximal_file_size);
        self
    }

    pub(crate) fn collect(mut self, collect: Collect) -> Self {
        self.collect = collect;
        self
    }

    pub(crate) fn group_by<G, T>(self, group_by: G) -> DirTraversalBuilder<'a, 'b, G>
    where
        G: Fn(&FileEntry) -> T,
    {
        DirTraversalBuilder {
            group_by: Some(group_by),
            root_dirs: self.root_dirs,
            stop_flag: self.stop_flag,
            progress_sender: self.progress_sender,
            directories: self.directories,
            extensions: self.extensions,
            excluded_items: self.excluded_items,
            recursive_search: self.recursive_search,
            maximal_file_size: self.maximal_file_size,
            minimal_file_size: self.minimal_file_size,
            collect: self.collect,
            checking_method: self.checking_method,
            tool_type: self.tool_type,
        }
    }

    pub(crate) fn build(self) -> DirTraversal<'a, 'b, F> {
        DirTraversal {
            group_by: self.group_by.expect("could not build"),
            root_dirs: self.root_dirs,
            stop_flag: self.stop_flag,
            progress_sender: self.progress_sender,
            checking_method: self.checking_method,
            minimal_file_size: self.minimal_file_size.unwrap_or(0),
            maximal_file_size: self.maximal_file_size.unwrap_or(u64::MAX),
            collect: self.collect,
            directories: self.directories.expect("could not build"),
            excluded_items: self.excluded_items.expect("could not build"),
            extensions: self.extensions.unwrap_or_default(),
            recursive_search: self.recursive_search,
            tool_type: self.tool_type,
        }
    }
}

pub enum DirTraversalResult<T: Ord + PartialOrd> {
    SuccessFiles {
        warnings: Vec<String>,
        grouped_file_entries: BTreeMap<T, Vec<FileEntry>>,
    },
    Stopped,
}

fn entry_type(file_type: FileType) -> EntryType {
    if file_type.is_dir() {
        EntryType::Dir
    } else if file_type.is_symlink() {
        EntryType::Symlink
    } else if file_type.is_file() {
        EntryType::File
    } else {
        EntryType::Other
    }
}

impl<F, T> DirTraversal<'_, '_, F>
where
    F: Fn(&FileEntry) -> T,
    T: Ord + PartialOrd,
{
    #[fun_time(message = "run(collecting files/dirs)", level = "debug")]
    pub(crate) fn run(self) -> DirTraversalResult<T> {
        assert_ne!(self.tool_type, ToolType::None, "Tool type cannot be None");

        let mut all_warnings = vec![];
        let mut grouped_file_entries: BTreeMap<T, Vec<FileEntry>> = BTreeMap::new();

        // Add root folders for finding
        let mut folders_to_check: Vec<PathBuf> = self.root_dirs.clone();

        let (progress_thread_handle, progress_thread_run, items_counter, _check_was_stopped, _size_counter) =
            prepare_thread_handler_common(self.progress_sender, CurrentStage::CollectingFiles, 0, (self.tool_type, self.checking_method), 0);

        let DirTraversal {
            collect,
            directories,
            excluded_items,
            extensions,
            recursive_search,
            minimal_file_size,
            maximal_file_size,
            stop_flag,
            ..
        } = self;
        let stop_flag = stop_flag.expect("Stop flag must be always initialized");

        while !folders_to_check.is_empty() {
            if check_if_stop_received(stop_flag) {
                send_info_and_wait_for_ending_all_threads(&progress_thread_run, progress_thread_handle);
                return DirTraversalResult::Stopped;
            }

            let segments: Vec<_> = folders_to_check
                .into_par_iter()
                .with_max_len(2) // Avoiding checking too much folders in batch
                .map(|current_folder| {
                    let mut dir_result = vec![];
                    let mut warnings = vec![];
                    let mut fe_result = vec![];

                    let Some(read_dir) = common_read_dir(&current_folder, &mut warnings) else {
                        return Some((dir_result, warnings, fe_result));
                    };

                    let mut counter = 0;
                    // Check every sub folder/file/link etc.
                    for entry in read_dir {
                        if check_if_stop_received(stop_flag) {
                            return None;
                        }

                        let Some(entry_data) = common_get_entry_data(&entry, &mut warnings, &current_folder) else {
                            continue;
                        };
                        let Ok(file_type) = entry_data.file_type() else { continue };

                        match (entry_type(file_type), collect) {
                            (EntryType::Dir, Collect::Files | Collect::InvalidSymlinks) => {
                                process_dir_in_file_symlink_mode(recursive_search, entry_data, &directories, &mut dir_result, &mut warnings, &excluded_items);
                            }
                            (EntryType::File, Collect::Files) => {
                                counter += 1;
                                process_file_in_file_mode(
                                    entry_data,
                                    &mut warnings,
                                    &mut fe_result,
                                    &extensions,
                                    &directories,
                                    &excluded_items,
                                    minimal_file_size,
                                    maximal_file_size,
                                );
                            }
                            (EntryType::File, Collect::InvalidSymlinks) => {
                                counter += 1;
                            }
                            (EntryType::Symlink, Collect::InvalidSymlinks) => {
                                counter += 1;
                                process_symlink_in_symlink_mode(entry_data, &mut warnings, &mut fe_result, &extensions, &directories, &excluded_items);
                            }
                            (EntryType::Symlink, Collect::Files) | (EntryType::Other, _) => {
                                // nothing to do
                            }
                        }
                    }
                    if counter > 0 {
                        // Increase counter in batch, because usually it may be slow to add multiple times atomic value
                        items_counter.fetch_add(counter, Ordering::Relaxed);
                    }
                    Some((dir_result, warnings, fe_result))
                })
                .while_some()
                .collect();

            let required_size = segments.iter().map(|(segment, _, _)| segment.len()).sum::<usize>();
            folders_to_check = Vec::with_capacity(required_size);

            // Process collected data
            for (segment, warnings, mut fe_result) in segments {
                folders_to_check.extend(segment);
                all_warnings.extend(warnings);
                fe_result.sort_by_cached_key(|fe| fe.path.to_string_lossy().to_string());
                for fe in fe_result {
                    let key = (self.group_by)(&fe);
                    grouped_file_entries.entry(key).or_default().push(fe);
                }
            }
        }

        send_info_and_wait_for_ending_all_threads(&progress_thread_run, progress_thread_handle);

        debug!("Collected {} files", grouped_file_entries.values().map(Vec::len).sum::<usize>());

        match collect {
            Collect::Files | Collect::InvalidSymlinks => DirTraversalResult::SuccessFiles {
                grouped_file_entries,
                warnings: all_warnings,
            },
        }
    }
}

fn process_file_in_file_mode(
    entry_data: &DirEntry,
    warnings: &mut Vec<String>,
    fe_result: &mut Vec<FileEntry>,
    extensions: &Extensions,
    directories: &Directories,
    excluded_items: &ExcludedItems,
    minimal_file_size: u64,
    maximal_file_size: u64,
) {
    if !extensions.check_if_entry_have_valid_extension(entry_data) {
        return;
    }

    let current_file_name = entry_data.path();
    if excluded_items.is_excluded(&current_file_name) {
        return;
    }

    #[cfg(target_family = "unix")]
    if directories.exclude_other_filesystems() {
        match directories.is_on_other_filesystems(&current_file_name) {
            Ok(true) => return,
            Err(e) => warnings.push(e),
            _ => (),
        }
    }

    let Some(metadata) = common_get_metadata_dir(entry_data, warnings, &current_file_name) else {
        return;
    };

    if (minimal_file_size..=maximal_file_size).contains(&metadata.len()) {
        // Creating new file entry
        let fe: FileEntry = FileEntry {
            size: metadata.len(),
            modified_date: get_modified_time(&metadata, warnings, &current_file_name, false),
            path: current_file_name,
        };

        fe_result.push(fe);
    }
}

fn process_dir_in_file_symlink_mode(
    recursive_search: bool,
    entry_data: &DirEntry,
    directories: &Directories,
    dir_result: &mut Vec<PathBuf>,
    warnings: &mut Vec<String>,
    excluded_items: &ExcludedItems,
) {
    if !recursive_search {
        return;
    }

    let dir_path = entry_data.path();
    if directories.is_excluded(&dir_path) {
        return;
    }

    if excluded_items.is_excluded(&dir_path) {
        return;
    }

    #[cfg(target_family = "unix")]
    if directories.exclude_other_filesystems() {
        match directories.is_on_other_filesystems(&dir_path) {
            Ok(true) => return,
            Err(e) => warnings.push(e),
            _ => (),
        }
    }

    dir_result.push(dir_path);
}

fn process_symlink_in_symlink_mode(
    entry_data: &DirEntry,
    warnings: &mut Vec<String>,
    fe_result: &mut Vec<FileEntry>,
    extensions: &Extensions,
    directories: &Directories,
    excluded_items: &ExcludedItems,
) {
    if !extensions.check_if_entry_have_valid_extension(entry_data) {
        return;
    }

    let current_file_name = entry_data.path();
    if excluded_items.is_excluded(&current_file_name) {
        return;
    }

    #[cfg(target_family = "unix")]
    if directories.exclude_other_filesystems() {
        match directories.is_on_other_filesystems(&current_file_name) {
            Ok(true) => return,
            Err(e) => warnings.push(e),
            _ => (),
        }
    }

    let Some(metadata) = common_get_metadata_dir(entry_data, warnings, &current_file_name) else {
        return;
    };

    // Creating new file entry
    let fe: FileEntry = FileEntry {
        size: metadata.len(),
        modified_date: get_modified_time(&metadata, warnings, &current_file_name, false),
        path: current_file_name,
    };

    fe_result.push(fe);
}

pub(crate) fn common_read_dir(current_folder: &Path, warnings: &mut Vec<String>) -> Option<Vec<Result<DirEntry, std::io::Error>>> {
    match fs::read_dir(current_folder) {
        Ok(t) => Some(t.collect()),
        Err(e) => {
            warnings.push(flc!("core_cannot_open_dir", dir = current_folder.to_string_lossy().to_string(), reason = e.to_string()));
            None
        }
    }
}
pub(crate) fn common_get_entry_data<'a>(entry: &'a Result<DirEntry, std::io::Error>, warnings: &mut Vec<String>, current_folder: &Path) -> Option<&'a DirEntry> {
    let entry_data = match entry {
        Ok(t) => t,
        Err(e) => {
            warnings.push(flc!(
                "core_cannot_read_entry_dir",
                dir = current_folder.to_string_lossy().to_string(),
                reason = e.to_string()
            ));
            return None;
        }
    };
    Some(entry_data)
}
pub(crate) fn common_get_metadata_dir(entry_data: &DirEntry, warnings: &mut Vec<String>, current_folder: &Path) -> Option<Metadata> {
    let metadata: Metadata = match entry_data.metadata() {
        Ok(t) => t,
        Err(e) => {
            warnings.push(flc!(
                "core_cannot_read_metadata_dir",
                dir = current_folder.to_string_lossy().to_string(),
                reason = e.to_string()
            ));
            return None;
        }
    };
    Some(metadata)
}

pub(crate) fn get_modified_time(metadata: &Metadata, warnings: &mut Vec<String>, current_file_name: &Path, is_folder: bool) -> u64 {
    match metadata.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs(),
            Err(_inspected) => {
                if is_folder {
                    warnings.push(flc!("core_folder_modified_before_epoch", name = current_file_name.to_string_lossy().to_string()));
                } else {
                    warnings.push(flc!("core_file_modified_before_epoch", name = current_file_name.to_string_lossy().to_string()));
                }
                0
            }
        },
        Err(e) => {
            if is_folder {
                warnings.push(flc!(
                    "core_folder_no_modification_date",
                    name = current_file_name.to_string_lossy().to_string(),
                    reason = e.to_string()
                ));
            } else {
                warnings.push(flc!(
                    "core_file_no_modification_date",
                    name = current_file_name.to_string_lossy().to_string(),
                    reason = e.to_string()
                ));
            }
            0
        }
    }
}

#[cfg(target_family = "windows")]
pub(crate) fn inode(_fe: &FileEntry) -> Option<u64> {
    None
}

#[cfg(target_family = "unix")]
pub(crate) fn inode(fe: &FileEntry) -> Option<u64> {
    if let Ok(meta) = fs::metadata(&fe.path) { Some(meta.ino()) } else { None }
}

pub(crate) fn take_1_per_inode((k, mut v): (Option<u64>, Vec<FileEntry>)) -> Vec<FileEntry> {
    if k.is_some() {
        v.drain(1..);
    }
    v
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs::File;
    use std::io::prelude::*;
    use std::time::{Duration, SystemTime};
    use std::{fs, io};

    use once_cell::sync::Lazy;
    use tempfile::TempDir;

    use super::*;
    use crate::common_tool::*;

    impl CommonData for CommonToolData {
        fn get_cd(&self) -> &CommonToolData {
            self
        }
        fn get_cd_mut(&mut self) -> &mut CommonToolData {
            self
        }
        fn found_any_broken_files(&self) -> bool {
            false
        }
    }

    static NOW: Lazy<SystemTime> = Lazy::new(|| SystemTime::UNIX_EPOCH + Duration::new(100, 0));
    const CONTENT: &[u8; 1] = b"a";

    fn create_files(dir: &TempDir) -> io::Result<(PathBuf, PathBuf, PathBuf)> {
        let (src, hard, other) = (dir.path().join("a"), dir.path().join("b"), dir.path().join("c"));

        let mut file = File::create(&src)?;
        file.write_all(CONTENT)?;
        fs::hard_link(&src, &hard)?;
        file.set_modified(*NOW)?;

        let mut file = File::create(&other)?;
        file.write_all(CONTENT)?;
        file.set_modified(*NOW)?;
        Ok((src, hard, other))
    }

    #[test]
    fn test_traversal() -> io::Result<()> {
        let dir = tempfile::Builder::new().tempdir()?;
        let (src, hard, other) = create_files(&dir)?;
        let secs = NOW.duration_since(SystemTime::UNIX_EPOCH).expect("Cannot fail calculating duration since epoch").as_secs();

        let mut common_data = CommonToolData::new(ToolType::SimilarImages);
        common_data.directories.set_included_directory([dir.path().to_owned()].to_vec());
        common_data.set_minimal_file_size(0);

        match DirTraversalBuilder::new()
            .group_by(|_fe| ())
            .stop_flag(&Arc::default())
            .common_data(&common_data)
            .build()
            .run()
        {
            DirTraversalResult::SuccessFiles {
                warnings: _,
                grouped_file_entries,
            } => {
                let actual: HashSet<_> = grouped_file_entries.into_values().flatten().collect();
                assert_eq!(
                    HashSet::from([
                        FileEntry {
                            path: src,
                            size: 1,
                            modified_date: secs,
                        },
                        FileEntry {
                            path: hard,
                            size: 1,
                            modified_date: secs,
                        },
                        FileEntry {
                            path: other,
                            size: 1,
                            modified_date: secs,
                        },
                    ]),
                    actual
                );
            }
            DirTraversalResult::Stopped => {
                panic!("Expect SuccessFiles.");
            }
        };
        Ok(())
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn test_traversal_group_by_inode() -> io::Result<()> {
        let dir = tempfile::Builder::new().tempdir()?;
        let (src, _, other) = create_files(&dir)?;
        let secs = NOW.duration_since(SystemTime::UNIX_EPOCH).expect("Cannot fail calculating duration since epoch").as_secs();

        let mut common_data = CommonToolData::new(ToolType::SimilarImages);
        common_data.directories.set_included_directory([dir.path().to_owned()].to_vec());
        common_data.set_minimal_file_size(0);

        match DirTraversalBuilder::new()
            .group_by(inode)
            .stop_flag(&Arc::default())
            .common_data(&common_data)
            .build()
            .run()
        {
            DirTraversalResult::SuccessFiles {
                warnings: _,
                grouped_file_entries,
            } => {
                let actual: HashSet<_> = grouped_file_entries.into_iter().flat_map(take_1_per_inode).collect();
                assert_eq!(
                    HashSet::from([
                        FileEntry {
                            path: src,
                            size: 1,
                            modified_date: secs,
                        },
                        FileEntry {
                            path: other,
                            size: 1,
                            modified_date: secs,
                        },
                    ]),
                    actual
                );
            }
            DirTraversalResult::Stopped => {
                panic!("Expect SuccessFiles.");
            }
        };
        Ok(())
    }

    #[cfg(target_family = "windows")]
    #[test]
    fn test_traversal_group_by_inode() -> io::Result<()> {
        let dir = tempfile::Builder::new().tempdir()?;
        let (src, hard, other) = create_files(&dir)?;
        let secs = NOW.duration_since(SystemTime::UNIX_EPOCH).expect("Cannot fail duration from epoch").as_secs();

        let mut common_data = CommonToolData::new(ToolType::SimilarImages);
        common_data.directories.set_included_directory([dir.path().to_owned()].to_vec());
        common_data.set_minimal_file_size(0);

        match DirTraversalBuilder::new()
            .group_by(inode)
            .stop_flag(&Arc::default())
            .common_data(&common_data)
            .build()
            .run()
        {
            DirTraversalResult::SuccessFiles {
                warnings: _,
                grouped_file_entries,
            } => {
                let actual: HashSet<_> = grouped_file_entries.into_iter().flat_map(take_1_per_inode).collect();
                assert_eq!(
                    HashSet::from([
                        FileEntry {
                            path: src,
                            size: 1,
                            modified_date: secs,
                        },
                        FileEntry {
                            path: hard,
                            size: 1,
                            modified_date: secs,
                        },
                        FileEntry {
                            path: other,
                            size: 1,
                            modified_date: secs,
                        },
                    ]),
                    actual
                );
            }
            _ => {
                panic!("Expect SuccessFiles.");
            }
        };
        Ok(())
    }
}
