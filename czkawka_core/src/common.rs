use std::cmp::Ordering;
use std::ffi::OsString;
use std::fs::{DirEntry, File, OpenOptions};
use std::io::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
use std::sync::{Arc, atomic};
use std::thread::{JoinHandle, sleep};
use std::time::{Duration, Instant};
use std::{env, fs, io, thread};

use crossbeam_channel::Sender;
use directories_next::ProjectDirs;
use file_rotate::compression::Compression;
use file_rotate::suffix::{AppendTimestamp, FileLimit};
use file_rotate::{ContentLimit, FileRotate};
use fun_time::fun_time;
use handsome_logger::{ColorChoice, CombinedLogger, ConfigBuilder, FormatText, SharedLogger, TermLogger, TerminalMode, TimeFormat, WriteLogger};
use log::{LevelFilter, Record, debug, info, warn};
use once_cell::sync::OnceCell;

use crate::CZKAWKA_VERSION;
// #[cfg(feature = "heif")]
// use libheif_rs::LibHeif;
use crate::common_dir_traversal::{CheckingMethod, ToolType};
use crate::common_directory::Directories;
use crate::common_items::{ExcludedItems, SingleExcludedItem};
use crate::common_traits::ResultEntry;
use crate::progress_data::{CurrentStage, ProgressData};

static NUMBER_OF_THREADS: state::InitCell<usize> = state::InitCell::new();
static ALL_AVAILABLE_THREADS: state::InitCell<usize> = state::InitCell::new();
static CONFIG_CACHE_PATH: OnceCell<Option<ConfigCachePath>> = OnceCell::new();

pub const DEFAULT_THREAD_SIZE: usize = 8 * 1024 * 1024; // 8 MB
pub const DEFAULT_WORKER_THREAD_SIZE: usize = 4 * 1024 * 1024; // 4 MB

const TEMP_HARDLINK_FILE: &str = "rzeczek.rxrxrxl";

#[derive(Debug, PartialEq)]
pub enum WorkContinueStatus {
    Continue,
    Stop,
}

#[derive(Debug, Clone)]
pub struct ConfigCachePath {
    pub config_folder: PathBuf,
    pub cache_folder: PathBuf,
}

pub fn get_config_cache_path() -> Option<ConfigCachePath> {
    CONFIG_CACHE_PATH.get().expect("Cannot fail if set_config_cache_path was called before").clone()
}

pub fn print_infos_and_warnings(infos: Vec<String>, warnings: Vec<String>) {
    for info in infos {
        info!("{info}");
    }
    for warning in warnings {
        warn!("{warning}");
    }
}

pub fn set_config_cache_path(cache_name: &'static str, config_name: &'static str) -> (Vec<String>, Vec<String>) {
    // By default, such folders are used:
    // Lin: /home/username/.config/czkawka
    // Win: C:\Users\Username\AppData\Roaming\Qarmin\Czkawka\config
    // Mac: /Users/Username/Library/Application Support/pl.Qarmin.Czkawka

    let mut infos = vec![];
    let mut warnings = vec![];

    let config_folder_env = std::env::var("CZKAWKA_CONFIG_PATH").unwrap_or_default().trim().to_string();
    let cache_folder_env = std::env::var("CZKAWKA_CACHE_PATH").unwrap_or_default().trim().to_string();

    let default_cache_folder = ProjectDirs::from("pl", "Qarmin", cache_name).map(|proj_dirs| proj_dirs.cache_dir().to_path_buf());
    let default_config_folder = ProjectDirs::from("pl", "Qarmin", config_name).map(|proj_dirs| proj_dirs.config_dir().to_path_buf());

    let mut resolve_folder = |env_var: &str, default_folder: Option<PathBuf>, name: &'static str| {
        let default_folder_str = default_folder.as_ref().map_or("<not available>".to_string(), |t| t.to_string_lossy().to_string());

        if env_var.is_empty() {
            default_folder
        } else {
            let folder_path = PathBuf::from(env_var);
            let _ = fs::create_dir_all(&folder_path);
            if !folder_path.exists() {
                warnings.push(format!(
                    "{name} folder \"{}\" does not exist, using default folder \"{}\"",
                    folder_path.to_string_lossy(),
                    default_folder_str
                ));
                return default_folder;
            };
            if !folder_path.is_dir() {
                warnings.push(format!(
                    "{name} folder \"{}\" is not a directory, using default folder \"{}\"",
                    folder_path.to_string_lossy(),
                    default_folder_str
                ));
                return default_folder;
            }

            match dunce::canonicalize(folder_path) {
                Ok(t) => Some(t),
                Err(_e) => {
                    warnings.push(format!(
                        "Cannot canonicalize {} folder \"{}\", using default folder \"{}\"",
                        name.to_ascii_lowercase(),
                        env_var,
                        default_folder_str
                    ));
                    default_folder
                }
            }
        }
    };

    let config_folder = resolve_folder(&config_folder_env, default_config_folder, "Config");
    let cache_folder = resolve_folder(&cache_folder_env, default_cache_folder, "Cache");

    let config_cache_path = if let (Some(config_folder), Some(cache_folder)) = (config_folder, cache_folder) {
        infos.push(format!(
            "Config folder set to \"{}\" and cache folder set to \"{}\"",
            config_folder.to_string_lossy(),
            cache_folder.to_string_lossy()
        ));
        if !config_folder.exists() {
            if let Err(e) = fs::create_dir_all(&config_folder) {
                warnings.push(format!("Cannot create config folder \"{}\", reason {e}", config_folder.to_string_lossy()));
            }
        }
        if !cache_folder.exists() {
            if let Err(e) = fs::create_dir_all(&cache_folder) {
                warnings.push(format!("Cannot create cache folder \"{}\", reason {e}", cache_folder.to_string_lossy()));
            }
        }
        Some(ConfigCachePath { config_folder, cache_folder })
    } else {
        warnings.push("Cannot set config/cache path - config and cache will not be used.".to_string());
        None
    };

    CONFIG_CACHE_PATH.set(config_cache_path).expect("Cannot set config/cache path twice");

    (infos, warnings)
}

pub fn get_number_of_threads() -> usize {
    let data = NUMBER_OF_THREADS.get();
    if *data >= 1 { *data } else { get_all_available_threads() }
}

fn filtering_messages(record: &Record) -> bool {
    if let Some(module_path) = record.module_path() {
        // Printing not supported modules
        // if !["krokiet", "czkawka", "log_panics", "smithay_client_toolkit", "sctk_adwaita"]
        //     .iter()
        //     .any(|t| module_path.starts_with(t))
        // {
        //     println!("{:?}", module_path);
        //     return true;
        // } else {
        //     return false;
        // }

        ["krokiet", "czkawka", "log_panics"].iter().any(|t| module_path.starts_with(t))
    } else {
        true
    }
}

#[allow(clippy::print_stdout)]
pub fn setup_logger(disabled_terminal_printing: bool, app_name: &str) {
    log_panics::init();

    let terminal_log_level = if disabled_terminal_printing && ![Ok("1"), Ok("true")].contains(&env::var("ENABLE_TERMINAL_LOGS_IN_CLI").as_deref()) {
        LevelFilter::Off
    } else {
        LevelFilter::Info
    };
    let file_log_level = LevelFilter::Debug;

    let term_config = ConfigBuilder::default()
        .set_level(terminal_log_level)
        .set_message_filtering(Some(filtering_messages))
        .build();
    let file_config = ConfigBuilder::default()
        .set_level(file_log_level)
        .set_write_once(true)
        .set_message_filtering(Some(filtering_messages))
        .set_time_format(TimeFormat::DateTimeWithMicro, None)
        .set_format_text(FormatText::DefaultWithThreadFile.get(), None)
        .build();

    let combined_logger = (|| {
        let Some(config_cache_path) = get_config_cache_path() else {
            // println!("No config cache path configured, using default config folder");
            return None;
        };

        let cache_logs_path = config_cache_path.cache_folder.join(format!("{app_name}.log"));

        let write_rotater = FileRotate::new(
            &cache_logs_path,
            AppendTimestamp::default(FileLimit::MaxFiles(3)),
            ContentLimit::BytesSurpassed(100 * 1024 * 1024),
            Compression::None,
            None,
        );

        let combined_logs: Vec<Box<dyn SharedLogger>> = if [Ok("1"), Ok("true")].contains(&env::var("DISABLE_FILE_LOGGING").as_deref()) {
            vec![TermLogger::new_from_config(term_config.clone())]
        } else {
            vec![TermLogger::new_from_config(term_config.clone()), WriteLogger::new(file_config, write_rotater)]
        };

        CombinedLogger::init(combined_logs).ok().inspect(|()| {
            info!("Logging to file \"{}\" and terminal", cache_logs_path.to_string_lossy());
        })
    })();

    if combined_logger.is_none() {
        TermLogger::init(term_config, TerminalMode::Mixed, ColorChoice::Always).expect("Cannot initialize logger");
        info!("Logging to terminal only, file logging is disabled");
    }
}

pub fn get_all_available_threads() -> usize {
    *ALL_AVAILABLE_THREADS.get_or_init(|| {
        let available_threads = thread::available_parallelism().map(std::num::NonZeroUsize::get).unwrap_or(1);
        ALL_AVAILABLE_THREADS.set(available_threads);
        available_threads
    })
}

#[allow(clippy::vec_init_then_push)]
#[allow(unused_mut)]
pub fn print_version_mode(app: &str) {
    let rust_version = env!("RUST_VERSION_INTERNAL");
    let debug_release = if cfg!(debug_assertions) { "debug" } else { "release" };

    let processors = get_all_available_threads();

    let info = os_info::get();

    let mut features: Vec<&str> = vec![];
    #[cfg(feature = "heif")]
    features.push("heif");
    #[cfg(feature = "libavif")]
    features.push("libavif");
    #[cfg(feature = "libraw")]
    features.push("libraw");
    #[cfg(feature = "fast_image_resize")]
    features.push("fast_image_resize");

    let mut app_cpu_version = "Baseline";
    let mut os_cpu_version = "Baseline";
    if cfg!(target_feature = "sse2") {
        app_cpu_version = "x86-64-v1 (SSE2)";
    }
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    if is_x86_feature_detected!("sse2") {
        os_cpu_version = "x86-64-v1 (SSE2)";
    }

    if cfg!(target_feature = "popcnt") {
        app_cpu_version = "x86-64-v2 (SSE4.2 + POPCNT)";
    }
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    if is_x86_feature_detected!("popcnt") {
        os_cpu_version = "x86-64-v2 (SSE4.2 + POPCNT)";
    }

    if cfg!(target_feature = "avx2") {
        app_cpu_version = "x86-64-v3 (AVX2) or x86-64-v4 (AVX-512)";
    }
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    if is_x86_feature_detected!("avx2") {
        os_cpu_version = "x86-64-v3 (AVX2)";
    }

    // TODO - https://github.com/rust-lang/rust/issues/44839 - remove "or" from above when fixed
    // Currently this is always false, because cfg!(target_feature = "avx512f") is not working
    // What is strange, because is_x86_feature_detected!("avx512f") is working
    if cfg!(target_feature = "avx512f") {
        app_cpu_version = "x86-64-v4 (AVX-512)";
    }
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    if is_x86_feature_detected!("avx512f") {
        os_cpu_version = "x86-64-v4 (AVX-512)";
    }

    // TODO - probably needs to add arm and other architectures, need help, because I don't have access to them

    info!(
        "{app} version: {CZKAWKA_VERSION}, {debug_release} mode, rust {rust_version}, os {} {} ({} {}), {processors} cpu/threads, features({}): [{}], app cpu version: {}, os cpu version: {}",
        info.os_type(),
        info.version(),
        std::env::consts::ARCH,
        info.bitness(),
        features.len(),
        features.join(", "),
        app_cpu_version,
        os_cpu_version,
    );
    if cfg!(debug_assertions) {
        warn!("You are running debug version of app which is a lot of slower than release version.");
    }

    if option_env!("USING_CRANELIFT").is_some() {
        warn!("You are running app with cranelift which is intended only for fast compilation, not runtime performance.");
    }

    if cfg!(panic = "abort") {
        warn!("You are running app compiled with panic='abort', which may cause panics when processing untrusted data.");
    }
}

pub fn set_number_of_threads(thread_number: usize) {
    NUMBER_OF_THREADS.set(thread_number);

    let additional_message = if thread_number == 0 {
        format!(
            " (0 - means that all available threads will be used({}))",
            thread::available_parallelism().map(std::num::NonZeroUsize::get).unwrap_or(1)
        )
    } else {
        "".to_string()
    };
    debug!("Number of threads set to {thread_number}{additional_message}");

    rayon::ThreadPoolBuilder::new()
        .num_threads(get_number_of_threads())
        .stack_size(DEFAULT_WORKER_THREAD_SIZE)
        .build_global()
        .expect("Cannot set number of threads");
}

pub const RAW_IMAGE_EXTENSIONS: &[&str] = &[
    "ari", "cr3", "cr2", "crw", "erf", "raf", "3fr", "kdc", "dcs", "dcr", "iiq", "mos", "mef", "mrw", "nef", "nrw", "orf", "rw2", "pef", "srw", "arw", "srf", "sr2",
];

pub const JXL_IMAGE_EXTENSIONS: &[&str] = &["jxl"];

#[cfg(feature = "libavif")]
pub const IMAGE_RS_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "bmp", "tiff", "tif", "tga", "ff", "jif", "jfi", "webp", "gif", "ico", "exr", "qoi", "avif",
];
#[cfg(not(feature = "libavif"))]
pub const IMAGE_RS_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "bmp", "tiff", "tif", "tga", "ff", "jif", "jfi", "webp", "gif", "ico", "exr", "qoi"];

#[cfg(feature = "libavif")]
pub const IMAGE_RS_SIMILAR_IMAGES_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "tiff", "tif", "tga", "ff", "jif", "jfi", "bmp", "webp", "exr", "qoi", "avif"];
#[cfg(not(feature = "libavif"))]
pub const IMAGE_RS_SIMILAR_IMAGES_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "tiff", "tif", "tga", "ff", "jif", "jfi", "bmp", "webp", "exr", "qoi"];

#[cfg(feature = "libavif")]
pub const IMAGE_RS_BROKEN_FILES_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "tiff", "tif", "tga", "ff", "jif", "jfi", "gif", "bmp", "ico", "jfif", "jpe", "pnz", "dib", "webp", "exr", "avif",
];
#[cfg(not(feature = "libavif"))]
pub const IMAGE_RS_BROKEN_FILES_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "tiff", "tif", "tga", "ff", "jif", "jfi", "gif", "bmp", "ico", "jfif", "jpe", "pnz", "dib", "webp", "exr",
];

pub const HEIC_EXTENSIONS: &[&str] = &["heif", "heifs", "heic", "heics", "avci", "avcs"];

pub const ZIP_FILES_EXTENSIONS: &[&str] = &["zip", "jar"];

pub const PDF_FILES_EXTENSIONS: &[&str] = &["pdf"];

pub const AUDIO_FILES_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "wav", "ogg", "m4a", "aac", "aiff", "pcm", "aif", "aiff", "aifc", "m3a", "mp2", "mp4a", "mp2a", "mpga", "wave", "weba", "wma", "oga",
];

pub const VIDEO_FILES_EXTENSIONS: &[&str] = &[
    "mp4", "mpv", "flv", "mp4a", "webm", "mpg", "mp2", "mpeg", "m4p", "m4v", "avi", "wmv", "qt", "mov", "swf", "mkv",
];

pub const LOOP_DURATION: u32 = 20; //ms
pub const SEND_PROGRESS_DATA_TIME_BETWEEN: u32 = 200; //ms

pub fn check_if_folder_contains_only_empty_folders(path: impl AsRef<Path>) -> Result<(), String> {
    let path = path.as_ref();
    if !path.is_dir() {
        return Err(format!("Trying to remove folder \"{}\" which is not a directory", path.to_string_lossy()));
    }

    let mut entries_to_check = Vec::new();
    let Ok(initial_entry) = path.read_dir() else {
        return Err(format!("Cannot read directory \"{}\"", path.to_string_lossy()));
    };
    for entry in initial_entry {
        if let Ok(entry) = entry {
            entries_to_check.push(entry);
        } else {
            return Err(format!("Cannot read entry from directory \"{}\"", path.to_string_lossy()));
        }
    }
    loop {
        let Some(entry) = entries_to_check.pop() else {
            break;
        };
        let Some(file_type) = entry.file_type().ok() else {
            return Err(format!(
                "Folder contains file with unknown type \"{}\" inside \"{}\"",
                entry.path().to_string_lossy(),
                path.to_string_lossy()
            ));
        };

        if !file_type.is_dir() {
            return Err(format!("Folder contains file \"{}\" inside \"{}\"", entry.path().to_string_lossy(), path.to_string_lossy()));
        }
        let Ok(internal_read_dir) = entry.path().read_dir() else {
            return Err(format!(
                "Cannot read directory \"{}\" inside \"{}\"",
                entry.path().to_string_lossy(),
                path.to_string_lossy()
            ));
        };
        for internal_elements in internal_read_dir {
            if let Ok(internal_element) = internal_elements {
                entries_to_check.push(internal_element);
            } else {
                return Err(format!(
                    "Cannot read entry from directory \"{}\" inside \"{}\"",
                    entry.path().to_string_lossy(),
                    path.to_string_lossy()
                ));
            }
        }
    }

    Ok(())
}

pub fn remove_folder_if_contains_only_empty_folders(path: impl AsRef<Path>, remove_to_trash: bool) -> Result<(), String> {
    check_if_folder_contains_only_empty_folders(&path)?;

    let path = path.as_ref();

    if remove_to_trash {
        trash::delete(path).map_err(|e| format!("Cannot move folder \"{}\" to trash, reason {e}", path.to_string_lossy()))
    } else {
        fs::remove_dir_all(path).map_err(|e| format!("Cannot remove directory \"{}\", reason {e}", path.to_string_lossy()))
    }
}

pub(crate) fn open_cache_folder(
    cache_file_name: &str,
    save_to_cache: bool,
    use_json: bool,
    warnings: &mut Vec<String>,
) -> Option<((Option<File>, PathBuf), (Option<File>, PathBuf))> {
    let cache_dir = get_config_cache_path()?.cache_folder;
    let cache_file = cache_dir.join(cache_file_name);
    let cache_file_json = cache_dir.join(cache_file_name.replace(".bin", ".json"));

    let mut file_handler_default = None;
    let mut file_handler_json = None;

    if save_to_cache {
        file_handler_default = Some(match OpenOptions::new().truncate(true).write(true).create(true).open(&cache_file) {
            Ok(t) => t,
            Err(e) => {
                warnings.push(format!("Cannot create or open cache file \"{}\", reason {e}", cache_file.to_string_lossy()));
                return None;
            }
        });
        if use_json {
            file_handler_json = Some(match OpenOptions::new().truncate(true).write(true).create(true).open(&cache_file_json) {
                Ok(t) => t,
                Err(e) => {
                    warnings.push(format!("Cannot create or open cache file \"{}\", reason {e}", cache_file_json.to_string_lossy()));
                    return None;
                }
            });
        }
    } else {
        if let Ok(t) = OpenOptions::new().read(true).open(&cache_file) {
            file_handler_default = Some(t);
        } else {
            if use_json {
                file_handler_json = Some(OpenOptions::new().read(true).open(&cache_file_json).ok()?);
            } else {
                // messages.push(format!("Cannot find or open cache file {cache_file:?}")); // No error or warning
                return None;
            }
        }
    };
    Some(((file_handler_default, cache_file), (file_handler_json, cache_file_json)))
}

pub fn split_path(path: &Path) -> (String, String) {
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(file)) => (dir.to_string_lossy().to_string(), file.to_string_lossy().into_owned()),
        (Some(dir), None) => (dir.to_string_lossy().to_string(), String::new()),
        (None, _) => (String::new(), String::new()),
    }
}

pub fn split_path_compare(path_a: &Path, path_b: &Path) -> Ordering {
    match path_a.parent().cmp(&path_b.parent()) {
        Ordering::Equal => path_a.file_name().cmp(&path_b.file_name()),
        other => other,
    }
}

pub(crate) fn create_crash_message(library_name: &str, file_path: &str, home_library_url: &str) -> String {
    format!(
        "{library_name} library crashed when opening \"{file_path}\", please check if this is fixed with the latest version of {library_name} and if it is not fixed, please report bug here - {home_library_url}"
    )
}

#[allow(clippy::string_slice)]
pub fn regex_check(expression_item: &SingleExcludedItem, directory_name: &str) -> bool {
    if expression_item.expression_splits.is_empty() {
        return true;
    }

    // Early checking if directory contains all parts needed by expression
    for split in &expression_item.unique_extensions_splits {
        if !directory_name.contains(split) {
            return false;
        }
    }

    // `git*` shouldn't be true for `/gitsfafasfs`
    if !expression_item.expression.starts_with('*')
        && directory_name
            .find(&expression_item.expression_splits[0])
            .expect("Cannot fail, because split must exists in directory_name")
            > 0
    {
        return false;
    }
    // `*home` shouldn't be true for `/homeowner`
    if !expression_item.expression.ends_with('*')
        && !directory_name.ends_with(expression_item.expression_splits.last().expect("Cannot fail, because at least one item is available"))
    {
        return false;
    }

    // At the end we check if parts between * are correctly positioned
    let mut last_split_point = directory_name.find(&expression_item.expression_splits[0]).expect("Cannot fail, because is checked earlier");
    let mut current_index: usize = 0;
    let mut found_index: usize;
    for spl in &expression_item.expression_splits[1..] {
        found_index = match directory_name[current_index..].find(spl) {
            Some(t) => t,
            None => return false,
        };
        current_index = last_split_point + spl.len();
        last_split_point = found_index + current_index;
    }
    true
}

#[allow(clippy::string_slice)] // Is in char boundary
pub fn normalize_windows_path(path_to_change: impl AsRef<Path>) -> PathBuf {
    let path = path_to_change.as_ref();

    // Don't do anything, because network path may be case intensive
    if path.to_string_lossy().starts_with('\\') {
        return path.to_path_buf();
    }

    match path.to_str() {
        Some(path) if path.is_char_boundary(1) => {
            let replaced = path.replace('/', "\\");
            let mut new_path = OsString::new();
            if replaced[1..].starts_with(':') {
                new_path.push(replaced[..1].to_ascii_uppercase());
                new_path.push(replaced[1..].to_ascii_lowercase());
            } else {
                new_path.push(replaced.to_ascii_lowercase());
            }
            PathBuf::from(new_path)
        }
        _ => path.to_path_buf(),
    }
}

pub(crate) fn check_folder_children(
    dir_result: &mut Vec<PathBuf>,
    warnings: &mut Vec<String>,
    entry_data: &DirEntry,
    recursive_search: bool,
    directories: &Directories,
    excluded_items: &ExcludedItems,
) {
    if !recursive_search {
        return;
    }

    let next_item = entry_data.path();
    if directories.is_excluded(&next_item) {
        return;
    }

    if excluded_items.is_excluded(&next_item) {
        return;
    }

    #[cfg(target_family = "unix")]
    if directories.exclude_other_filesystems() {
        match directories.is_on_other_filesystems(&next_item) {
            Ok(true) => return,
            Err(e) => warnings.push(e),
            _ => (),
        }
    }

    dir_result.push(next_item);
}

pub(crate) fn filter_reference_folders_generic<T>(entries_to_check: Vec<Vec<T>>, directories: &Directories) -> Vec<(T, Vec<T>)>
where
    T: ResultEntry,
{
    entries_to_check
        .into_iter()
        .filter_map(|vec_file_entry| {
            let (mut files_from_referenced_folders, normal_files): (Vec<_>, Vec<_>) =
                vec_file_entry.into_iter().partition(|e| directories.is_in_referenced_directory(e.get_path()));

            if normal_files.is_empty() {
                None
            } else {
                files_from_referenced_folders.pop().map(|file| (file, normal_files))
            }
        })
        .collect::<Vec<(T, Vec<T>)>>()
}

pub(crate) fn prepare_thread_handler_common(
    progress_sender: Option<&Sender<ProgressData>>,
    sstage: CurrentStage,
    max_items: usize,
    test_type: (ToolType, CheckingMethod),
    max_size: u64,
) -> (JoinHandle<()>, Arc<AtomicBool>, Arc<AtomicUsize>, AtomicBool, Arc<AtomicU64>) {
    let (tool_type, checking_method) = test_type;
    assert_ne!(tool_type, ToolType::None, "Cannot send progress data for ToolType::None");
    let progress_thread_run = Arc::new(AtomicBool::new(true));
    let items_counter = Arc::new(AtomicUsize::new(0));
    let size_counter = Arc::new(AtomicU64::new(0));
    let check_was_stopped = AtomicBool::new(false);
    let progress_thread_sender = if let Some(progress_sender) = progress_sender {
        let progress_send = progress_sender.clone();
        let progress_thread_run = progress_thread_run.clone();
        let items_counter = items_counter.clone();
        let size_counter = size_counter.clone();
        thread::spawn(move || {
            // Use earlier time, to send immediately first message
            let mut time_since_last_send = Instant::now().checked_sub(Duration::from_secs(10u64)).unwrap_or_else(Instant::now);

            loop {
                if time_since_last_send.elapsed().as_millis() > SEND_PROGRESS_DATA_TIME_BETWEEN as u128 {
                    let progress_data = ProgressData {
                        sstage,
                        checking_method,
                        current_stage_idx: sstage.get_current_stage(),
                        max_stage_idx: tool_type.get_max_stage(checking_method),
                        entries_checked: items_counter.load(atomic::Ordering::Relaxed),
                        entries_to_check: max_items,
                        bytes_checked: size_counter.load(atomic::Ordering::Relaxed),
                        bytes_to_check: max_size,
                        tool_type,
                    };

                    progress_data.validate();

                    progress_send.send(progress_data).expect("Cannot send progress data");
                    time_since_last_send = Instant::now();
                }
                if !progress_thread_run.load(atomic::Ordering::Relaxed) {
                    break;
                }
                sleep(Duration::from_millis(LOOP_DURATION as u64));
            }
        })
    } else {
        thread::spawn(|| {})
    };
    (progress_thread_sender, progress_thread_run, items_counter, check_was_stopped, size_counter)
}

#[inline]
pub(crate) fn check_if_stop_received(stop_flag: &Arc<AtomicBool>) -> bool {
    stop_flag.load(atomic::Ordering::Relaxed)
}

pub fn make_hard_link(src: &Path, dst: &Path) -> io::Result<()> {
    let dst_dir = dst.parent().ok_or_else(|| Error::other("No parent"))?;
    let temp = dst_dir.join(TEMP_HARDLINK_FILE);
    fs::rename(dst, temp.as_path())?;
    let result = fs::hard_link(src, dst);
    if result.is_err() {
        fs::rename(temp.as_path(), dst)?;
    }
    fs::remove_file(temp)?;
    result
}

#[fun_time(message = "send_info_and_wait_for_ending_all_threads", level = "debug")]
pub(crate) fn send_info_and_wait_for_ending_all_threads(progress_thread_run: &Arc<AtomicBool>, progress_thread_handle: JoinHandle<()>) {
    progress_thread_run.store(false, atomic::Ordering::Relaxed);
    progress_thread_handle.join().expect("Cannot join progress thread - quite fatal error, but happens rarely");
}

#[cfg(test)]
mod test {
    use std::fs::{File, Metadata, read_dir};
    use std::io::Write;
    #[cfg(target_family = "windows")]
    use std::os::fs::MetadataExt;
    #[cfg(target_family = "unix")]
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::{fs, io};

    use tempfile::tempdir;

    use crate::common::{make_hard_link, normalize_windows_path, regex_check, remove_folder_if_contains_only_empty_folders};
    use crate::common_items::new_excluded_item;

    #[cfg(target_family = "unix")]
    fn assert_inode(before: &Metadata, after: &Metadata) {
        assert_eq!(before.ino(), after.ino());
    }

    #[cfg(target_family = "windows")]
    fn assert_inode(_: &Metadata, _: &Metadata) {}

    #[test]
    fn test_make_hard_link() -> io::Result<()> {
        let dir = tempfile::Builder::new().tempdir()?;
        let (src, dst) = (dir.path().join("a"), dir.path().join("b"));
        File::create(&src)?;
        let metadata = fs::metadata(&src)?;
        File::create(&dst)?;

        make_hard_link(&src, &dst)?;

        assert_inode(&metadata, &fs::metadata(&dst)?);
        assert_eq!(metadata.permissions(), fs::metadata(&dst)?.permissions());
        assert_eq!(metadata.modified()?, fs::metadata(&dst)?.modified()?);
        assert_inode(&metadata, &fs::metadata(&src)?);
        assert_eq!(metadata.permissions(), fs::metadata(&src)?.permissions());
        assert_eq!(metadata.modified()?, fs::metadata(&src)?.modified()?);

        let mut actual = read_dir(&dir)?.flatten().map(|e| e.path()).collect::<Vec<PathBuf>>();
        actual.sort_unstable();
        assert_eq!(vec![src, dst], actual);
        Ok(())
    }
    #[test]
    fn test_make_hard_link_fails() -> io::Result<()> {
        let dir = tempfile::Builder::new().tempdir()?;
        let (src, dst) = (dir.path().join("a"), dir.path().join("b"));
        File::create(&dst)?;
        let metadata = fs::metadata(&dst)?;

        assert!(make_hard_link(&src, &dst).is_err());

        assert_inode(&metadata, &fs::metadata(&dst)?);
        assert_eq!(metadata.permissions(), fs::metadata(&dst)?.permissions());
        assert_eq!(metadata.modified()?, fs::metadata(&dst)?.modified()?);

        assert_eq!(vec![dst], read_dir(&dir)?.flatten().map(|e| e.path()).collect::<Vec<PathBuf>>());
        Ok(())
    }

    #[test]
    fn test_remove_folder_if_contains_only_empty_folders() {
        let dir = tempdir().expect("Cannot create temporary directory");
        let sub_dir = dir.path().join("sub_dir");
        fs::create_dir(&sub_dir).expect("Cannot create directory");

        // Test with empty directory
        assert!(remove_folder_if_contains_only_empty_folders(&sub_dir, false).is_ok());
        assert!(!Path::new(&sub_dir).exists());

        // Test with directory containing an empty directory
        fs::create_dir(&sub_dir).expect("Cannot create directory");
        fs::create_dir(sub_dir.join("empty_sub_dir")).expect("Cannot create directory");
        assert!(remove_folder_if_contains_only_empty_folders(&sub_dir, false).is_ok());
        assert!(!Path::new(&sub_dir).exists());

        // Test with directory containing a file
        fs::create_dir(&sub_dir).expect("Cannot create directory");
        let mut file = File::create(sub_dir.join("file.txt")).expect("Cannot create file");
        writeln!(file, "Hello, world!").expect("Cannot write to file");
        assert!(remove_folder_if_contains_only_empty_folders(&sub_dir, false).is_err());
        assert!(Path::new(&sub_dir).exists());
    }

    #[test]
    fn test_regex() {
        assert!(regex_check(&new_excluded_item("*"), "/home/rafal"));
        assert!(regex_check(&new_excluded_item("*home*"), "/home/rafal"));
        assert!(regex_check(&new_excluded_item("*home"), "/home"));
        assert!(regex_check(&new_excluded_item("*home/"), "/home/"));
        assert!(regex_check(&new_excluded_item("*home/*"), "/home/"));
        assert!(regex_check(&new_excluded_item("*.git*"), "/home/.git"));
        assert!(regex_check(&new_excluded_item("*/home/rafal*rafal*rafal*rafal*"), "/home/rafal/rafalrafalrafal"));
        assert!(regex_check(&new_excluded_item("AAA"), "AAA"));
        assert!(regex_check(&new_excluded_item("AAA*"), "AAABDGG/QQPW*"));
        assert!(!regex_check(&new_excluded_item("*home"), "/home/"));
        assert!(!regex_check(&new_excluded_item("*home"), "/homefasfasfasfasf/"));
        assert!(!regex_check(&new_excluded_item("*home"), "/homefasfasfasfasf"));
        assert!(!regex_check(&new_excluded_item("rafal*afal*fal"), "rafal"));
        assert!(!regex_check(&new_excluded_item("rafal*a"), "rafal"));
        assert!(!regex_check(&new_excluded_item("AAAAAAAA****"), "/AAAAAAAAAAAAAAAAA"));
        assert!(!regex_check(&new_excluded_item("*.git/*"), "/home/.git"));
        assert!(!regex_check(&new_excluded_item("*home/*koc"), "/koc/home/"));
        assert!(!regex_check(&new_excluded_item("*home/"), "/home"));
        assert!(!regex_check(&new_excluded_item("*TTT"), "/GGG"));
        assert!(regex_check(
            &new_excluded_item("*/home/*/.local/share/containers"),
            "/var/home/roman/.local/share/containers"
        ));

        if cfg!(target_family = "windows") {
            assert!(regex_check(&new_excluded_item("*\\home"), "C:\\home"));
            assert!(regex_check(&new_excluded_item("*/home"), "C:\\home"));
        }
    }

    #[test]
    fn test_windows_path() {
        assert_eq!(PathBuf::from("C:\\path.txt"), normalize_windows_path("c:/PATH.tXt"));
        assert_eq!(PathBuf::from("H:\\reka\\weza\\roman.txt"), normalize_windows_path("h:/RekA/Weza\\roMan.Txt"));
        assert_eq!(PathBuf::from("T:\\a"), normalize_windows_path("T:\\A"));
        assert_eq!(PathBuf::from("\\\\aBBa"), normalize_windows_path("\\\\aBBa"));
        assert_eq!(PathBuf::from("a"), normalize_windows_path("a"));
        assert_eq!(PathBuf::from(""), normalize_windows_path(""));
    }
}
