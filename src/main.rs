//! wtui is a desktop manager for Git worktrees.
//!
//! All filesystem, Git, GitHub CLI, and configuration work runs through
//! `Task::perform` plus Tokio's blocking worker pool. The UI only updates in
//! `update` and renders in `view`.

use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};

use iced::widget::{
    button, center, column, container, mouse_area, opaque, rich_text, row, scrollable, span, stack,
    text, text_input,
};
use iced::{
    Alignment, Border, Color, Element, Length, Size, Subscription, Task, color, theme::Palette,
    time, window,
};
use iced_themer::{ThemeConfig, Themed};
use serde::{Deserialize, Serialize};
use wait_timeout::ChildExt;

const THEME_TOML: &str = include_str!("../theme.toml");
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

const SIZE_TITLE: f32 = 30.0;
const SIZE_HEADING: f32 = 18.0;
const SIZE_BODY: f32 = 16.0;
const SIZE_SMALL: f32 = 14.0;
const SIZE_CAPTION: f32 = 13.0;

fn main() -> iced::Result {
    let config = Arc::new(
        THEME_TOML
            .parse::<ThemeConfig>()
            .expect("embedded theme.toml must parse"),
    );
    let font = config.font();
    let boot_theme = Arc::clone(&config);
    let app_theme = Arc::clone(&config);

    let app = iced::application(move || boot(Arc::clone(&boot_theme)), update, view)
        .title("wtui")
        .theme(move |_: &State| app_theme.theme())
        .subscription(subscription)
        .window(window_settings());

    match font {
        Some(font) => app.default_font(font).run(),
        None => app.run(),
    }
}

fn window_settings() -> window::Settings {
    window::Settings {
        size: Size::new(1180.0, 760.0),
        min_size: Some(Size::new(760.0, 520.0)),
        platform_specific: window::settings::PlatformSpecific {
            application_id: "wtui".to_owned(),
            ..Default::default()
        },
        ..window::Settings::default()
    }
}

// ── Application state ─────────────────────────────────────────────────────

struct State {
    theme: Arc<ThemeConfig>,
    repositories: Vec<Repository>,
    selected_root: Option<String>,
    selected_worktree: Option<WorktreeKey>,
    pull_request: PullRequestStatus,
    modal: Option<Modal>,
    banner: Option<Banner>,
    config_saving: bool,
    config_dirty: bool,
}

#[derive(Debug, Clone)]
struct Repository {
    id: String,
    path: PathBuf,
    worktrees: Vec<Worktree>,
    refreshing: bool,
    changing: bool,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct Worktree {
    path: PathBuf,
    branch: Option<String>,
    is_main: bool,
    locked: bool,
    dirty_files: usize,
    ignored_files: usize,
    upstream: Option<String>,
    ahead: Option<u32>,
    behind: Option<u32>,
    has_remote: bool,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeKey {
    root_id: String,
    path: PathBuf,
}

#[derive(Debug, Clone)]
enum PullRequestStatus {
    Idle,
    Loading,
    Found(PullRequest),
    NotFound,
    Error(String),
}

#[derive(Debug, Clone, Deserialize)]
struct PullRequest {
    number: u64,
    url: String,
    state: String,
    #[serde(rename = "headRefName")]
    head_branch: String,
    #[serde(rename = "mergedAt")]
    merged_at: Option<String>,
}

impl PullRequest {
    fn is_merged(&self) -> bool {
        self.state == "MERGED" || self.merged_at.is_some()
    }
}

#[derive(Debug, Clone)]
enum Modal {
    AddWorktree {
        root_id: String,
        branch: String,
    },
    RemoveWorktree {
        root_id: String,
        path: PathBuf,
        dirty_files: usize,
        ignored_files: usize,
        delete_branch: Option<String>,
    },
}

struct Banner {
    level: BannerLevel,
    message: String,
}

#[derive(Clone, Copy)]
enum BannerLevel {
    Success,
    Error,
    Info,
}

#[derive(Debug, Clone)]
enum Msg {
    ConfigLoaded(Result<LoadedRepositories, String>),
    ConfigSaved(Result<(), String>),
    ChooseRepository,
    RepositoryPicked(Option<PathBuf>),
    RepositoryAdded(Result<NewRepository, String>),
    SelectRoot(String),
    RefreshAll,
    Poll,
    RepositoryRefreshed {
        root_id: String,
        result: Result<Refresh, String>,
    },
    ShowAddWorktree,
    BranchChanged(String),
    ConfirmAddWorktree,
    WorktreeAdded {
        root_id: String,
        result: Result<PathBuf, String>,
    },
    WorktreeSelected(WorktreeKey),
    PullRequestLoaded {
        key: WorktreeKey,
        result: Result<Option<PullRequest>, String>,
    },
    OpenPullRequestUrl(String),
    PullRequestUrlOpened(Result<(), String>),
    AskRemoveWorktree(WorktreeKey),
    AskRemoveMergedWorktree(WorktreeKey),
    ConfirmRemoveWorktree,
    WorktreeRemoved {
        root_id: String,
        path: PathBuf,
        result: Result<RemovalOutcome, String>,
    },
    DismissModal,
}

fn boot(theme: Arc<ThemeConfig>) -> (State, Task<Msg>) {
    let state = State {
        theme,
        repositories: Vec::new(),
        selected_root: None,
        selected_worktree: None,
        pull_request: PullRequestStatus::Idle,
        modal: None,
        banner: None,
        config_saving: false,
        config_dirty: false,
    };

    // `load_configured_repositories` reads files and validates Git roots. It is
    // deliberately run as a background task, even during app startup.
    (
        state,
        Task::perform(background(load_configured_repositories), Msg::ConfigLoaded),
    )
}

fn subscription(_state: &State) -> Subscription<Msg> {
    time::every(POLL_INTERVAL).map(|_| Msg::Poll)
}

/// Run synchronous system work on Tokio's blocking worker pool. Keep every
/// filesystem, Git, GitHub CLI, and native-dialog call behind this boundary.
async fn background<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(work)
        .await
        .expect("a wtui background worker must not panic")
}

// ── Update ─────────────────────────────────────────────────────────────────

fn update(state: &mut State, msg: Msg) -> Task<Msg> {
    match msg {
        Msg::ConfigLoaded(Ok(loaded)) => {
            state.repositories = loaded.repositories;
            state.selected_root = state
                .repositories
                .first()
                .map(|repository| repository.id.clone());
            if !loaded.warnings.is_empty() {
                state.banner = Some(Banner {
                    level: BannerLevel::Info,
                    message: loaded.warnings.join(" "),
                });
            }
            refresh_all(state)
        }
        Msg::ConfigLoaded(Err(error)) => {
            state.banner = Some(Banner {
                level: BannerLevel::Error,
                message: format!("Could not load saved repositories: {error}"),
            });
            Task::none()
        }
        Msg::ConfigSaved(result) => {
            state.config_saving = false;
            if let Err(error) = result {
                state.banner = Some(Banner {
                    level: BannerLevel::Error,
                    message: format!("Could not save repositories: {error}"),
                });
            }
            if state.config_dirty {
                request_config_save(state)
            } else {
                Task::none()
            }
        }
        Msg::ChooseRepository => Task::perform(
            background(|| {
                rfd::FileDialog::new()
                    .set_title("Select Git repository")
                    .pick_folder()
            }),
            Msg::RepositoryPicked,
        ),
        Msg::RepositoryPicked(None) => Task::none(),
        Msg::RepositoryPicked(Some(path)) => Task::perform(
            background(move || validate_repository(path)),
            Msg::RepositoryAdded,
        ),
        Msg::RepositoryAdded(Ok(new)) => {
            if state
                .repositories
                .iter()
                .any(|repository| repository.id == new.id)
            {
                state.banner = Some(Banner {
                    level: BannerLevel::Info,
                    message: "That repository is already listed.".to_owned(),
                });
                return Task::none();
            }

            let root_id = new.id.clone();
            state.repositories.push(Repository {
                id: new.id,
                path: new.path,
                worktrees: Vec::new(),
                refreshing: false,
                changing: false,
                error: None,
            });
            state.selected_root = Some(root_id.clone());
            state.banner = None;
            let save = request_config_save(state);
            let refresh = request_refresh(state, &root_id);
            Task::batch([save, refresh])
        }
        Msg::RepositoryAdded(Err(error)) => {
            state.banner = Some(Banner {
                level: BannerLevel::Error,
                message: format!("Not a Git repository: {error}"),
            });
            Task::none()
        }
        Msg::SelectRoot(root_id) => {
            if state
                .repositories
                .iter()
                .any(|repository| repository.id == root_id)
            {
                state.selected_root = Some(root_id);
                state.selected_worktree = None;
                state.pull_request = PullRequestStatus::Idle;
            }
            Task::none()
        }
        Msg::RefreshAll | Msg::Poll => refresh_all(state),
        Msg::RepositoryRefreshed { root_id, result } => {
            let selected_path = state
                .selected_worktree
                .as_ref()
                .filter(|selection| selection.root_id == root_id)
                .map(|selection| selection.path.clone());
            let mut removed_selection = false;
            match result {
                Ok(refresh) => {
                    if let Some(repository) = repository_mut(state, &root_id) {
                        repository.worktrees = refresh.worktrees;
                        repository.refreshing = false;
                        repository.error = None;
                        removed_selection = selected_path.is_some_and(|path| {
                            !repository
                                .worktrees
                                .iter()
                                .any(|worktree| worktree.path == path)
                        });
                    }
                }
                Err(error) => {
                    if let Some(repository) = repository_mut(state, &root_id) {
                        repository.refreshing = false;
                        repository.error = Some(error);
                    }
                }
            }
            if removed_selection {
                state.selected_worktree = None;
                state.pull_request = PullRequestStatus::Idle;
            }
            Task::none()
        }
        Msg::ShowAddWorktree => {
            let Some(root_id) = state.selected_root.clone() else {
                state.banner = Some(Banner {
                    level: BannerLevel::Info,
                    message: "Select a repository first.".to_owned(),
                });
                return Task::none();
            };
            if repository(state, &root_id).is_some_and(|repository| repository.changing) {
                return Task::none();
            }
            state.modal = Some(Modal::AddWorktree {
                root_id,
                branch: String::new(),
            });
            Task::none()
        }
        Msg::BranchChanged(branch) => {
            if let Some(Modal::AddWorktree { branch: value, .. }) = &mut state.modal {
                *value = branch;
            }
            Task::none()
        }
        Msg::ConfirmAddWorktree => {
            let Some(Modal::AddWorktree { root_id, branch }) = state.modal.clone() else {
                return Task::none();
            };
            if branch.trim().is_empty() {
                state.banner = Some(Banner {
                    level: BannerLevel::Error,
                    message: "Enter a branch name.".to_owned(),
                });
                return Task::none();
            }
            let Some(repository) = repository_mut(state, &root_id) else {
                return Task::none();
            };
            if repository.changing {
                return Task::none();
            }
            repository.changing = true;
            let root = repository.path.clone();
            state.modal = None;
            state.banner = None;
            Task::perform(
                background(move || add_worktree(root, branch)),
                move |result| Msg::WorktreeAdded { root_id, result },
            )
        }
        Msg::WorktreeAdded { root_id, result } => {
            if let Some(repository) = repository_mut(state, &root_id) {
                repository.changing = false;
            }
            match result {
                Ok(path) => {
                    state.banner = Some(Banner {
                        level: BannerLevel::Success,
                        message: format!("Created worktree at {}.", path.display()),
                    });
                    request_refresh(state, &root_id)
                }
                Err(error) => {
                    state.banner = Some(Banner {
                        level: BannerLevel::Error,
                        message: format!("Could not add worktree: {error}"),
                    });
                    Task::none()
                }
            }
        }
        Msg::WorktreeSelected(key) => {
            let path = key.path.clone();
            state.selected_worktree = Some(key.clone());
            state.pull_request = PullRequestStatus::Loading;
            Task::perform(background(move || load_pull_request(path)), move |result| {
                Msg::PullRequestLoaded { key, result }
            })
        }
        Msg::PullRequestLoaded { key, result } => {
            if state.selected_worktree.as_ref() != Some(&key) {
                return Task::none();
            }
            state.pull_request = match result {
                Ok(Some(pull_request)) => PullRequestStatus::Found(pull_request),
                Ok(None) => PullRequestStatus::NotFound,
                Err(message) => PullRequestStatus::Error(message),
            };
            Task::none()
        }
        Msg::OpenPullRequestUrl(url) => Task::perform(
            background(move || open::that(&url).map_err(|error| error.to_string())),
            Msg::PullRequestUrlOpened,
        ),
        Msg::PullRequestUrlOpened(Ok(())) => Task::none(),
        Msg::PullRequestUrlOpened(Err(error)) => {
            state.banner = Some(Banner {
                level: BannerLevel::Error,
                message: format!("Could not open the pull request URL: {error}"),
            });
            Task::none()
        }
        Msg::AskRemoveWorktree(key) => {
            let Some(worktree) = find_worktree(state, &key).cloned() else {
                return Task::none();
            };
            if worktree.is_main {
                state.banner = Some(Banner {
                    level: BannerLevel::Info,
                    message: "The root worktree cannot be removed.".to_owned(),
                });
            } else {
                state.modal = Some(Modal::RemoveWorktree {
                    root_id: key.root_id,
                    path: key.path,
                    dirty_files: worktree.dirty_files,
                    ignored_files: worktree.ignored_files,
                    delete_branch: None,
                });
            }
            Task::none()
        }
        Msg::AskRemoveMergedWorktree(key) => {
            let Some(worktree) = find_worktree(state, &key).cloned() else {
                return Task::none();
            };
            let can_remove_branch = !worktree.is_main
                && worktree.branch.is_some()
                && matches!(&state.pull_request, PullRequestStatus::Found(pr) if pr.is_merged());
            if !can_remove_branch {
                return Task::none();
            }
            state.modal = Some(Modal::RemoveWorktree {
                root_id: key.root_id,
                path: key.path,
                dirty_files: worktree.dirty_files,
                ignored_files: worktree.ignored_files,
                delete_branch: worktree.branch,
            });
            Task::none()
        }
        Msg::ConfirmRemoveWorktree => {
            let Some(Modal::RemoveWorktree {
                root_id,
                path,
                dirty_files,
                ignored_files,
                delete_branch,
            }) = state.modal.clone()
            else {
                return Task::none();
            };
            let Some(repository) = repository_mut(state, &root_id) else {
                return Task::none();
            };
            if repository.changing {
                return Task::none();
            }
            repository.changing = true;
            let root = repository.path.clone();
            state.modal = None;
            state.banner = None;
            let path_for_worker = path.clone();
            let force = dirty_files > 0 || ignored_files > 0;
            Task::perform(
                background(move || match delete_branch {
                    Some(branch) => {
                        remove_merged_worktree_and_branch(root, path_for_worker, branch, force)
                    }
                    None => remove_worktree(root, path_for_worker, force, None),
                }),
                move |result| Msg::WorktreeRemoved {
                    root_id,
                    path,
                    result,
                },
            )
        }
        Msg::WorktreeRemoved {
            root_id,
            path,
            result,
        } => {
            if let Some(repository) = repository_mut(state, &root_id) {
                repository.changing = false;
            }
            match result {
                Ok(outcome) => {
                    if state.selected_worktree.as_ref().is_some_and(|selection| {
                        selection.root_id == root_id && selection.path == path
                    }) {
                        state.selected_worktree = None;
                        state.pull_request = PullRequestStatus::Idle;
                    }
                    state.banner = Some(match outcome.branch_deletion_error {
                        Some(error) => Banner {
                            level: BannerLevel::Info,
                            message: format!(
                                "Removed worktree at {}, but could not delete its local branch: {error}",
                                path.display()
                            ),
                        },
                        None => Banner {
                            level: BannerLevel::Success,
                            message: match outcome.deleted_branch {
                                Some(branch) => format!(
                                    "Removed worktree at {} and local branch {branch}.",
                                    path.display()
                                ),
                                None => format!("Removed worktree at {}.", path.display()),
                            },
                        },
                    });
                    request_refresh(state, &root_id)
                }
                Err(error) => {
                    state.banner = Some(Banner {
                        level: BannerLevel::Error,
                        message: format!("Could not remove worktree: {error}"),
                    });
                    Task::none()
                }
            }
        }
        Msg::DismissModal => {
            state.modal = None;
            Task::none()
        }
    }
}

fn repository<'a>(state: &'a State, root_id: &str) -> Option<&'a Repository> {
    state
        .repositories
        .iter()
        .find(|repository| repository.id == root_id)
}

fn repository_mut<'a>(state: &'a mut State, root_id: &str) -> Option<&'a mut Repository> {
    state
        .repositories
        .iter_mut()
        .find(|repository| repository.id == root_id)
}

fn find_worktree<'a>(state: &'a State, key: &WorktreeKey) -> Option<&'a Worktree> {
    repository(state, &key.root_id)?
        .worktrees
        .iter()
        .find(|worktree| worktree.path == key.path)
}

fn request_refresh(state: &mut State, root_id: &str) -> Task<Msg> {
    let Some(repository) = repository_mut(state, root_id) else {
        return Task::none();
    };
    if repository.refreshing || repository.changing {
        return Task::none();
    }
    repository.refreshing = true;
    let root_id = repository.id.clone();
    let path = repository.path.clone();
    Task::perform(
        background(move || refresh_repository(path)),
        move |result| Msg::RepositoryRefreshed { root_id, result },
    )
}

fn refresh_all(state: &mut State) -> Task<Msg> {
    let ids: Vec<_> = state
        .repositories
        .iter()
        .map(|repository| repository.id.clone())
        .collect();
    Task::batch(ids.iter().map(|root_id| request_refresh(state, root_id)))
}

// ── Background Git and configuration operations ────────────────────────────

#[derive(Debug, Clone)]
struct NewRepository {
    id: String,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct LoadedRepositories {
    repositories: Vec<Repository>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct Refresh {
    worktrees: Vec<Worktree>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct SavedConfig {
    #[serde(default)]
    roots: Vec<String>,
}

fn load_configured_repositories() -> Result<LoadedRepositories, String> {
    let config = load_config()?;
    let mut repositories = Vec::new();
    let mut warnings = Vec::new();
    let mut seen = HashSet::new();

    for root in config.roots {
        match validate_repository(PathBuf::from(&root)) {
            Ok(repository) if seen.insert(repository.id.clone()) => repositories.push(Repository {
                id: repository.id,
                path: repository.path,
                worktrees: Vec::new(),
                refreshing: false,
                changing: false,
                error: None,
            }),
            Ok(_) => {}
            Err(error) => warnings.push(format!("Skipped saved repository {root:?}: {error}.")),
        }
    }

    Ok(LoadedRepositories {
        repositories,
        warnings,
    })
}

fn config_path() -> Result<PathBuf, String> {
    dirs::config_dir()
        .map(|directory| directory.join("wtui").join("repositories.json"))
        .ok_or_else(|| "the platform configuration directory is unavailable".to_owned())
}

fn load_config() -> Result<SavedConfig, String> {
    let path = config_path()?;
    match fs::read_to_string(&path) {
        Ok(contents) => {
            serde_json::from_str(&contents).map_err(|error| format!("{}: {error}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(SavedConfig::default()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Queue a configuration snapshot. At most one write runs at once, so a stale
/// snapshot cannot finish after a newer one and discard a recently added root.
fn request_config_save(state: &mut State) -> Task<Msg> {
    state.config_dirty = true;
    if state.config_saving {
        return Task::none();
    }
    state.config_saving = true;
    state.config_dirty = false;
    save_repositories_task(&state.repositories)
}

fn save_repositories_task(repositories: &[Repository]) -> Task<Msg> {
    let roots = repositories
        .iter()
        .map(|repository| repository.path.to_string_lossy().into_owned())
        .collect();
    Task::perform(
        background(move || save_config(SavedConfig { roots })),
        Msg::ConfigSaved,
    )
}

fn save_config(config: SavedConfig) -> Result<(), String> {
    let path = config_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| "configuration path has no parent directory".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;

    let contents = serde_json::to_vec_pretty(&config).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, contents).map_err(|error| format!("{}: {error}", temporary.display()))?;
    fs::rename(&temporary, &path).map_err(|error| format!("{}: {error}", path.display()))
}

fn validate_repository(path: PathBuf) -> Result<NewRepository, String> {
    let top_level = git(&path, &["rev-parse", "--show-toplevel"])?;
    let path = fs::canonicalize(top_level.trim())
        .map_err(|error| format!("{}: {error}", top_level.trim()))?;
    let id = path.to_string_lossy().into_owned();
    Ok(NewRepository { id, path })
}

fn refresh_repository(root: PathBuf) -> Result<Refresh, String> {
    let raw_worktrees = parse_worktree_list(&git(&root, &["worktree", "list", "--porcelain"])?)?;
    let has_remote = !git(&root, &["remote"])?.trim().is_empty();

    let worktrees = raw_worktrees
        .into_iter()
        .map(|raw| match status_summary(&raw.path) {
            Ok(status) => Worktree {
                is_main: raw.path == root,
                path: raw.path,
                branch: raw.branch,
                locked: raw.locked,
                dirty_files: status.dirty_files,
                ignored_files: status.ignored_files,
                upstream: status.upstream,
                ahead: status.ahead,
                behind: status.behind,
                has_remote,
                error: None,
            },
            Err(error) => Worktree {
                is_main: raw.path == root,
                path: raw.path,
                branch: raw.branch,
                locked: raw.locked,
                dirty_files: 0,
                ignored_files: 0,
                upstream: None,
                ahead: None,
                behind: None,
                has_remote,
                error: Some(error),
            },
        })
        .collect();

    Ok(Refresh { worktrees })
}

fn add_worktree(root: PathBuf, branch: String) -> Result<PathBuf, String> {
    let branch = branch.trim().to_owned();
    git(&root, &["check-ref-format", "--branch", &branch])?;

    let root_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "the root repository name is not valid Unicode".to_owned())?;
    let parent = root
        .parent()
        .ok_or_else(|| "the root repository has no parent directory".to_owned())?;
    let target = parent.join(format!("{root_name}-{}", slugify(&branch)));

    if target.exists() {
        return Err(format!("{} already exists", target.display()));
    }

    git_with_args(
        &root,
        [
            "worktree".to_owned(),
            "add".to_owned(),
            "-b".to_owned(),
            branch,
            target.to_string_lossy().into_owned(),
        ],
    )?;
    Ok(target)
}

#[derive(Debug, Clone)]
struct RemovalOutcome {
    deleted_branch: Option<String>,
    branch_deletion_error: Option<String>,
}

fn remove_worktree(
    root: PathBuf,
    target: PathBuf,
    force: bool,
    expected_branch: Option<&str>,
) -> Result<RemovalOutcome, String> {
    let worktrees = parse_worktree_list(&git(&root, &["worktree", "list", "--porcelain"])?)?;
    let target =
        fs::canonicalize(&target).map_err(|error| format!("{}: {error}", target.display()))?;
    let Some(worktree) = worktrees
        .into_iter()
        .find(|worktree| worktree.path == target)
    else {
        return Err("the worktree no longer belongs to this repository".to_owned());
    };
    if worktree.path == root {
        return Err("the root worktree cannot be removed".to_owned());
    }
    if let Some(expected_branch) = expected_branch
        && worktree.branch.as_deref() != Some(expected_branch)
    {
        return Err("the worktree branch changed before removal".to_owned());
    }
    let status = status_summary(&worktree.path)?;
    if (status.dirty_files > 0 || status.ignored_files > 0) && !force {
        return Err(
            "the worktree has uncommitted or ignored files; confirm forced removal".to_owned(),
        );
    }

    let mut arguments = vec!["worktree".to_owned(), "remove".to_owned()];
    if force {
        arguments.push("--force".to_owned());
    }
    arguments.push(worktree.path.to_string_lossy().into_owned());
    git_with_args(&root, arguments)?;
    Ok(RemovalOutcome {
        deleted_branch: None,
        branch_deletion_error: None,
    })
}

/// Remove a worktree and its local branch only after GitHub confirms the PR is
/// still merged. `git branch -D` is safe here because the GitHub check is made
/// immediately before the destructive action; it also works before the root
/// worktree has fetched the merged commit.
fn remove_merged_worktree_and_branch(
    root: PathBuf,
    target: PathBuf,
    branch: String,
    force: bool,
) -> Result<RemovalOutcome, String> {
    match load_pull_request(target.clone())? {
        Some(pull_request) if pull_request.is_merged() && pull_request.head_branch == branch => {}
        Some(pull_request) if !pull_request.is_merged() => {
            return Err("the pull request is no longer merged".to_owned());
        }
        Some(_) => return Err("the merged pull request belongs to another branch".to_owned()),
        None => return Err("no GitHub pull request is available to verify as merged".to_owned()),
    }

    remove_worktree(root.clone(), target, force, Some(&branch))?;
    let branch_deletion_error = git_with_args(
        &root,
        [
            "branch".to_owned(),
            "-D".to_owned(),
            "--".to_owned(),
            branch.clone(),
        ],
    )
    .err();
    Ok(RemovalOutcome {
        deleted_branch: branch_deletion_error.is_none().then_some(branch),
        branch_deletion_error,
    })
}

/// Read the PR only when a worktree is selected. The five-second refresh never
/// calls GitHub, so it stays cheap and works offline.
fn load_pull_request(path: PathBuf) -> Result<Option<PullRequest>, String> {
    let output = match run_command(
        "gh",
        &path,
        [
            "pr".to_owned(),
            "view".to_owned(),
            "--json".to_owned(),
            "number,url,state,mergedAt,headRefName".to_owned(),
        ],
    ) {
        Ok(output) => output,
        Err(error) if no_pull_request_error(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    serde_json::from_str(&output)
        .map(Some)
        .map_err(|error| format!("could not parse GitHub CLI output: {error}"))
}

fn no_pull_request_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("no pull request found") || error.contains("no pull requests found")
}

fn git(path: &Path, arguments: &[&str]) -> Result<String, String> {
    git_with_args(
        path,
        arguments.iter().map(|argument| (*argument).to_owned()),
    )
}

fn git_with_args(
    path: &Path,
    arguments: impl IntoIterator<Item = String>,
) -> Result<String, String> {
    run_command("git", path, arguments)
}

/// Run a command with bounded wall time while draining both output streams.
/// Draining in separate threads prevents a command with large output from
/// filling a pipe and blocking before the timeout can be checked.
fn run_command(
    program: &str,
    path: &Path,
    arguments: impl IntoIterator<Item = String>,
) -> Result<String, String> {
    let mut child = Command::new(program)
        .current_dir(path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start {program} in {}: {error}", path.display()))?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let stdout_reader = std::thread::spawn(move || read_stream(stdout));
    let stderr_reader = std::thread::spawn(move || read_stream(stderr));

    let status = match child.wait_timeout(COMMAND_TIMEOUT) {
        Ok(Some(status)) => status,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            // Do not wait for readers here. A child process can leave a
            // descendant holding the pipe open after the parent is killed.
            // Dropping these handles detaches the readers and lets the UI
            // receive this timeout result immediately.
            return Err(format!(
                "{program} timed out after {} seconds",
                COMMAND_TIMEOUT.as_secs()
            ));
        }
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("could not wait for {program}: {error}"));
        }
    };
    let stdout = collect_stream(stdout_reader, "stdout")?;
    let stderr = collect_stream(stderr_reader, "stderr")?;

    if status.success() {
        return Ok(String::from_utf8_lossy(&stdout).into_owned());
    }

    let error = String::from_utf8_lossy(&stderr).trim().to_owned();
    let fallback = String::from_utf8_lossy(&stdout).trim().to_owned();
    Err(if error.is_empty() { fallback } else { error })
}

fn read_stream(mut stream: impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn collect_stream(
    reader: std::thread::JoinHandle<std::io::Result<Vec<u8>>>,
    name: &str,
) -> Result<Vec<u8>, String> {
    match reader.join() {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(error)) => Err(format!("could not read command {name}: {error}")),
        Err(_) => Err(format!("command {name} reader panicked")),
    }
}

#[derive(Debug)]
struct RawWorktree {
    path: PathBuf,
    branch: Option<String>,
    locked: bool,
}

fn parse_worktree_list(output: &str) -> Result<Vec<RawWorktree>, String> {
    let mut worktrees = Vec::new();
    for block in output
        .split("\n\n")
        .filter(|block| !block.trim().is_empty())
    {
        let mut path = None;
        let mut branch = None;
        let mut locked = false;
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(value));
            } else if let Some(value) = line.strip_prefix("branch refs/heads/") {
                branch = Some(value.to_owned());
            } else if line == "locked" || line.starts_with("locked ") {
                locked = true;
            }
        }
        let path = path.ok_or_else(|| "Git returned a worktree entry without a path".to_owned())?;
        worktrees.push(RawWorktree {
            path,
            branch,
            locked,
        });
    }
    Ok(worktrees)
}

#[derive(Debug)]
struct StatusSummary {
    dirty_files: usize,
    ignored_files: usize,
    upstream: Option<String>,
    ahead: Option<u32>,
    behind: Option<u32>,
}

fn status_summary(path: &Path) -> Result<StatusSummary, String> {
    let output = git(
        path,
        &["status", "--porcelain=v2", "--branch", "--ignored", "-z"],
    )?;
    Ok(parse_status_summary(&output))
}

fn parse_status_summary(output: &str) -> StatusSummary {
    let mut dirty_files = 0;
    let mut ignored_files = 0;
    let mut skip_rename_source = false;
    let mut upstream = None;
    let mut ahead = None;
    let mut behind = None;

    for record in output.split('\0').filter(|record| !record.is_empty()) {
        if skip_rename_source {
            // Porcelain v2 type `2` has a second NUL-delimited source path.
            skip_rename_source = false;
            continue;
        }
        if let Some(value) = record.strip_prefix("# branch.upstream ") {
            upstream = Some(value.to_owned());
        } else if let Some(value) = record.strip_prefix("# branch.ab ") {
            let mut values = value.split_whitespace();
            ahead = values
                .next()
                .and_then(|value| value.strip_prefix('+'))
                .and_then(|value| value.parse().ok());
            behind = values
                .next()
                .and_then(|value| value.strip_prefix('-'))
                .and_then(|value| value.parse().ok());
        } else if record.starts_with("2 ") {
            dirty_files += 1;
            skip_rename_source = true;
        } else if matches!(record.as_bytes().first(), Some(b'1' | b'u' | b'?')) {
            dirty_files += 1;
        } else if record.starts_with("! ") {
            ignored_files += 1;
        }
    }

    StatusSummary {
        dirty_files,
        ignored_files,
        upstream,
        ahead,
        behind,
    }
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_separator = true;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            last_was_separator = false;
        } else if !last_was_separator {
            slug.push('-');
            last_was_separator = true;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "worktree".to_owned()
    } else {
        slug.to_owned()
    }
}

// ── View ───────────────────────────────────────────────────────────────────

fn view(state: &State) -> Element<'_, Msg> {
    let palette = state.theme.theme().palette();
    let mut content = column![header(state, &palette)].spacing(12);
    if let Some(banner) = &state.banner {
        content = content.push(banner_view(banner, &palette));
    }
    content = content.push(main_content(state, &palette));

    let base: Element<_> = container(content)
        .padding(16)
        .width(Length::Fill)
        .height(Length::Fill)
        .into();

    match &state.modal {
        Some(modal) => stack![base, modal_view(modal, &palette)].into(),
        None => base,
    }
}

fn header<'a>(state: &State, palette: &Palette) -> Element<'a, Msg> {
    let title = column![
        text("wtui").size(SIZE_TITLE).color(palette.text),
        text("Manage Git worktrees without blocking your desktop.")
            .size(SIZE_SMALL)
            .color(dim(palette.text, 0.55)),
    ]
    .spacing(2)
    .width(Length::Fill);

    let selected_is_idle = state
        .selected_root
        .as_deref()
        .and_then(|root_id| repository(state, root_id))
        .is_some_and(|repository| !repository.changing);

    row![
        title,
        action_button(
            "Add repository",
            palette.primary,
            Some(Msg::ChooseRepository)
        ),
        action_button("Refresh", palette.primary, Some(Msg::RefreshAll)),
        action_button(
            "Add worktree",
            palette.success,
            selected_is_idle.then_some(Msg::ShowAddWorktree)
        ),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

fn banner_view<'a>(banner: &Banner, palette: &Palette) -> Element<'a, Msg> {
    let accent = match banner.level {
        BannerLevel::Success => palette.success,
        BannerLevel::Error => palette.danger,
        BannerLevel::Info => palette.primary,
    };
    container(
        text(banner.message.clone())
            .size(SIZE_SMALL)
            .color(palette.text),
    )
    .padding([8, 12])
    .width(Length::Fill)
    .style(move |_| container::Style {
        background: Some(dim(accent, 0.15).into()),
        border: Border {
            radius: 8.0.into(),
            width: 1.0,
            color: dim(accent, 0.4),
        },
        ..container::Style::default()
    })
    .into()
}

fn main_content<'a>(state: &'a State, palette: &Palette) -> Element<'a, Msg> {
    if state.repositories.is_empty() {
        return container(
            column![
                text("No repositories yet.")
                    .size(SIZE_HEADING)
                    .color(palette.text),
                text("Select a checked-out Git repository to list and manage its worktrees.")
                    .size(SIZE_BODY)
                    .color(dim(palette.text, 0.6)),
            ]
            .spacing(8),
        )
        .padding(24)
        .width(Length::Fill)
        .height(Length::Fill)
        .themed(state.theme.container())
        .into();
    }

    let mut tabs = row![].spacing(8).width(Length::Fill);
    for repository in &state.repositories {
        let name = repository
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repository");
        let selected = state.selected_root.as_deref() == Some(&repository.id);
        tabs = tabs.push(tab_button(name, selected, repository.id.clone(), palette));
    }

    let body: Element<_> = match state
        .selected_root
        .as_deref()
        .and_then(|id| repository(state, id))
    {
        Some(repository) => worktree_overview(state, repository, palette),
        None => text("Select a repository.").size(SIZE_BODY).into(),
    };

    column![tabs, body].spacing(12).height(Length::Fill).into()
}

fn tab_button<'a>(
    label: &'a str,
    selected: bool,
    id: String,
    palette: &Palette,
) -> Element<'a, Msg> {
    let text_color = palette.text;
    let accent = if selected {
        palette.primary
    } else {
        dim(text_color, 0.22)
    };
    button(text(label).size(SIZE_SMALL).color(if selected {
        color!(0x14161f)
    } else {
        text_color
    }))
    .padding([7, 11])
    .style(move |_, status| button::Style {
        background: Some(
            match status {
                button::Status::Hovered => scale(accent, 1.12),
                button::Status::Pressed => scale(accent, 0.85),
                button::Status::Disabled | button::Status::Active => accent,
            }
            .into(),
        ),
        text_color: if selected {
            color!(0x14161f)
        } else {
            text_color
        },
        border: Border {
            radius: 7.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    })
    .on_press(Msg::SelectRoot(id))
    .into()
}

fn worktree_overview<'a>(
    state: &'a State,
    repository: &'a Repository,
    palette: &Palette,
) -> Element<'a, Msg> {
    let activity = if repository.changing {
        "Working…"
    } else if repository.refreshing {
        "Refreshing…"
    } else {
        ""
    };
    let list_header = row![
        text(format!("Worktrees ({})", repository.worktrees.len()))
            .size(SIZE_HEADING)
            .color(palette.text)
            .width(Length::Fill),
        text(activity).size(SIZE_SMALL).color(palette.primary),
    ]
    .align_y(Alignment::Center);
    let mut list = column![list_header].spacing(8);

    if let Some(error) = &repository.error {
        list = list.push(text(error.clone()).size(SIZE_SMALL).color(palette.danger));
    }
    if repository.worktrees.is_empty() && !repository.refreshing && repository.error.is_none() {
        list = list.push(
            text("No worktrees found.")
                .size(SIZE_SMALL)
                .color(dim(palette.text, 0.6)),
        );
    }
    for worktree in &repository.worktrees {
        list = list.push(worktree_card(state, repository, worktree, palette));
    }

    let worktrees = container(scrollable(list).height(Length::Fill))
        .padding(14)
        .width(Length::FillPortion(5))
        .height(Length::Fill)
        .themed(state.theme.container());
    let details = worktree_details_panel(state, palette);

    row![worktrees, details]
        .spacing(12)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn worktree_card<'a>(
    state: &'a State,
    repository: &'a Repository,
    worktree: &'a Worktree,
    palette: &Palette,
) -> Element<'a, Msg> {
    let key = WorktreeKey {
        root_id: repository.id.clone(),
        path: worktree.path.clone(),
    };
    let selected = state.selected_worktree.as_ref() == Some(&key);
    let text_color = palette.text;
    let primary = palette.primary;
    let danger = palette.danger;
    let name = worktree.branch.as_deref().unwrap_or("(detached HEAD)");
    let path = worktree.path.to_string_lossy().into_owned();
    let mut details = vec![worktree_status(worktree)];
    if worktree.locked {
        details.push("locked".to_owned());
    }
    if let Some(error) = &worktree.error {
        details.push(error.clone());
    }

    let accent = if selected {
        dim(primary, 0.25)
    } else {
        dim(text_color, 0.08)
    };
    let select = button(
        column![
            text(name).size(SIZE_BODY).color(text_color),
            text(path).size(SIZE_CAPTION).color(dim(text_color, 0.55)),
            text(details.join(" · "))
                .size(SIZE_CAPTION)
                .color(if worktree.error.is_some() {
                    danger
                } else {
                    dim(text_color, 0.7)
                }),
        ]
        .spacing(3),
    )
    .width(Length::Fill)
    .padding(10)
    .style(move |_, status| button::Style {
        background: Some(
            match status {
                button::Status::Hovered => scale(accent, 1.35),
                button::Status::Pressed => scale(accent, 0.85),
                button::Status::Disabled | button::Status::Active => accent,
            }
            .into(),
        ),
        border: Border {
            radius: 8.0.into(),
            width: if selected { 1.0 } else { 0.0 },
            color: primary,
        },
        ..button::Style::default()
    })
    .on_press(Msg::WorktreeSelected(key.clone()));

    let remove: Element<_> = if worktree.is_main {
        text("root")
            .size(SIZE_CAPTION)
            .color(dim(text_color, 0.55))
            .into()
    } else {
        action_button(
            "Remove",
            danger,
            (!repository.changing).then_some(Msg::AskRemoveWorktree(key)),
        )
    };

    row![select, remove]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
}

fn worktree_status(worktree: &Worktree) -> String {
    let mut status = if worktree.dirty_files == 0 {
        "clean".to_owned()
    } else {
        format!("{} uncommitted", worktree.dirty_files)
    };
    if worktree.ignored_files > 0 {
        status.push_str(&format!(" · {} ignored", worktree.ignored_files));
    }

    match &worktree.upstream {
        Some(upstream) => {
            status.push_str(&format!(" · {upstream}"));
            match (worktree.ahead, worktree.behind) {
                (Some(ahead), Some(behind)) if ahead > 0 || behind > 0 => {
                    status.push_str(&format!(" · ↑{ahead} ↓{behind}"));
                }
                (Some(_), Some(_)) => status.push_str(" · pushed"),
                _ => status.push_str(" · upstream status unavailable"),
            }
        }
        None if worktree.has_remote => status.push_str(" · no upstream branch"),
        None => status.push_str(" · no remote"),
    }
    status
}

fn worktree_details_panel<'a>(state: &'a State, palette: &Palette) -> Element<'a, Msg> {
    let text_color = palette.text;
    let primary = palette.primary;
    let danger = palette.danger;
    let success = palette.success;
    let body: Element<_> = match state
        .selected_worktree
        .as_ref()
        .and_then(|key| find_worktree(state, key).map(|worktree| (key, worktree)))
    {
        None => column![
            text("Worktree details")
                .size(SIZE_HEADING)
                .color(text_color),
            text("Select a worktree to see its status and pull request.")
                .size(SIZE_SMALL)
                .color(dim(text_color, 0.6)),
        ]
        .spacing(8)
        .into(),
        Some((key, worktree)) => {
            let branch = worktree.branch.as_deref().unwrap_or("(detached HEAD)");
            let changes = if worktree.dirty_files == 0 {
                "clean".to_owned()
            } else {
                format!("{} uncommitted file(s)", worktree.dirty_files)
            };
            let ignored = if worktree.ignored_files == 0 {
                "none".to_owned()
            } else {
                format!(
                    "{} ignored entr{}",
                    worktree.ignored_files,
                    if worktree.ignored_files == 1 {
                        "y"
                    } else {
                        "ies"
                    }
                )
            };
            let upstream = worktree
                .upstream
                .as_deref()
                .unwrap_or(if worktree.has_remote {
                    "no upstream branch"
                } else {
                    "no remote"
                });
            let sync = match (worktree.ahead, worktree.behind) {
                (Some(ahead), Some(behind)) => format!("↑{ahead}  ↓{behind}"),
                _ if worktree.upstream.is_some() => "unavailable".to_owned(),
                _ => "not applicable".to_owned(),
            };
            let pull_request: Element<_> = match &state.pull_request {
                PullRequestStatus::Idle => text("Pull request: not checked")
                    .size(SIZE_SMALL)
                    .color(dim(text_color, 0.6))
                    .into(),
                PullRequestStatus::Loading => text("Pull request: checking GitHub…")
                    .size(SIZE_SMALL)
                    .color(primary)
                    .into(),
                PullRequestStatus::NotFound => text("Pull request: none found")
                    .size(SIZE_SMALL)
                    .color(dim(text_color, 0.6))
                    .into(),
                PullRequestStatus::Error(error) => {
                    text(format!("Pull request check failed: {error}"))
                        .size(SIZE_SMALL)
                        .color(danger)
                        .into()
                }
                PullRequestStatus::Found(pull_request) => {
                    let state_name = if pull_request.is_merged() {
                        "merged"
                    } else {
                        &pull_request.state
                    };
                    let mut content = column![
                        text(format!(
                            "Pull request #{}: {state_name}",
                            pull_request.number
                        ))
                        .size(SIZE_SMALL)
                        .color(if pull_request.is_merged() {
                            success
                        } else {
                            text_color
                        }),
                        rich_text([span(pull_request.url.clone())
                            .color(primary)
                            .underline(true)
                            .link(pull_request.url.clone())])
                        .size(SIZE_CAPTION)
                        .on_link_click(Msg::OpenPullRequestUrl),
                    ]
                    .spacing(3);
                    if pull_request.is_merged() && !worktree.is_main && worktree.branch.is_some() {
                        content = content.push(action_button(
                            "PR has been merged, remove the worktree and branch?",
                            danger,
                            Some(Msg::AskRemoveMergedWorktree(key.clone())),
                        ));
                    }
                    content.into()
                }
            };

            column![
                text("Worktree details")
                    .size(SIZE_HEADING)
                    .color(text_color),
                text(branch).size(SIZE_BODY).color(text_color),
                text(worktree.path.to_string_lossy().into_owned())
                    .size(SIZE_CAPTION)
                    .color(dim(text_color, 0.6)),
                text(format!("Changes: {changes}"))
                    .size(SIZE_SMALL)
                    .color(text_color),
                text(format!("Ignored entries: {ignored}"))
                    .size(SIZE_SMALL)
                    .color(text_color),
                text(format!("Upstream: {upstream}"))
                    .size(SIZE_SMALL)
                    .color(text_color),
                text(format!("Sync: {sync}"))
                    .size(SIZE_SMALL)
                    .color(text_color),
                pull_request,
            ]
            .spacing(8)
            .into()
        }
    };

    container(body)
        .padding(14)
        .width(Length::FillPortion(7))
        .height(Length::Fill)
        .themed(state.theme.container())
        .into()
}

fn modal_view<'a>(modal: &'a Modal, palette: &Palette) -> Element<'a, Msg> {
    let dialog: Element<_> = match modal {
        Modal::AddWorktree { branch, .. } => {
            let submit = (!branch.trim().is_empty()).then_some(Msg::ConfirmAddWorktree);
            container(
                column![
                    text("Add worktree").size(SIZE_HEADING).color(palette.text),
                    text("The new directory is a sibling named <root>-<branch-slug>.")
                        .size(SIZE_SMALL)
                        .color(dim(palette.text, 0.65)),
                    text_input("Branch name", branch)
                        .on_input(Msg::BranchChanged)
                        .padding(10),
                    row![
                        action_button("Cancel", palette.primary, Some(Msg::DismissModal)),
                        action_button("Create", palette.success, submit),
                    ]
                    .spacing(10),
                ]
                .spacing(14),
            )
            .padding(18)
            .width(Length::Fixed(480.0))
            .themed(None)
            .into()
        }
        Modal::RemoveWorktree {
            path,
            dirty_files,
            ignored_files,
            delete_branch,
            ..
        } => {
            let guarded = *dirty_files > 0 || *ignored_files > 0;
            let branch_note = match delete_branch {
                Some(branch) => format!(" The local branch {branch:?} will also be deleted."),
                None => " The branch stays available.".to_owned(),
            };
            let warning = if guarded {
                format!(
                    "This worktree has {dirty_files} uncommitted and {ignored_files} ignored entr{}. Remove anyway? This runs Git with --force.{branch_note}",
                    if *ignored_files == 1 { "y" } else { "ies" }
                )
            } else {
                format!("This removes the worktree through Git.{branch_note}")
            };
            container(
                column![
                    text(if delete_branch.is_some() {
                        "Remove worktree and branch"
                    } else {
                        "Remove worktree"
                    })
                    .size(SIZE_HEADING)
                    .color(palette.text),
                    text(path.to_string_lossy().into_owned())
                        .size(SIZE_SMALL)
                        .color(dim(palette.text, 0.65)),
                    text(warning).size(SIZE_BODY).color(palette.text),
                    row![
                        action_button("Cancel", palette.primary, Some(Msg::DismissModal)),
                        action_button(
                            if delete_branch.is_some() {
                                if guarded {
                                    "Remove anyway and delete branch"
                                } else {
                                    "Remove and delete branch"
                                }
                            } else if guarded {
                                "Remove anyway"
                            } else {
                                "Remove"
                            },
                            palette.danger,
                            Some(Msg::ConfirmRemoveWorktree),
                        ),
                    ]
                    .spacing(10),
                ]
                .spacing(14),
            )
            .padding(18)
            .width(Length::Fixed(520.0))
            .themed(None)
            .into()
        }
    };

    // The two opaque layers stop clicks from reaching the underlying app or the
    // backdrop when a user clicks inside the dialog.
    let backdrop = palette.background;
    container(opaque(
        mouse_area(
            center(opaque(dialog))
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .on_press(Msg::DismissModal),
    ))
    .width(Length::Fill)
    .height(Length::Fill)
    .style(move |_| container::Style {
        background: Some(dim(backdrop, 0.82).into()),
        ..container::Style::default()
    })
    .into()
}

fn action_button<'a>(label: &str, accent: Color, on_press: Option<Msg>) -> Element<'a, Msg> {
    let mut action = button(text(label.to_owned()).size(SIZE_SMALL))
        .padding([8, 12])
        .style(move |_, status| {
            let background = match status {
                button::Status::Hovered => scale(accent, 1.12),
                button::Status::Pressed => scale(accent, 0.85),
                button::Status::Disabled => dim(accent, 0.25),
                button::Status::Active => accent,
            };
            button::Style {
                background: Some(background.into()),
                text_color: match status {
                    button::Status::Disabled => dim(accent, 0.5),
                    _ => color!(0x14161f),
                },
                border: Border {
                    radius: 8.0.into(),
                    ..Border::default()
                },
                ..button::Style::default()
            }
        });
    if let Some(message) = on_press {
        action = action.on_press(message);
    }
    action.into()
}

fn dim(color: Color, alpha: f32) -> Color {
    Color { a: alpha, ..color }
}

fn scale(color: Color, factor: f32) -> Color {
    Color {
        r: (color.r * factor).clamp(0.0, 1.0),
        g: (color.g * factor).clamp(0.0, 1.0),
        b: (color.b * factor).clamp(0.0, 1.0),
        a: color.a,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugifies_branch_names_for_sibling_paths() {
        assert_eq!(slugify("my-new-feature"), "my-new-feature");
        assert_eq!(slugify("Feature/Some thing"), "feature-some-thing");
        assert_eq!(slugify("---"), "worktree");
    }

    #[test]
    fn parses_worktree_porcelain() {
        let worktrees = parse_worktree_list(
            "worktree /repos/botanic\nHEAD abc123\nbranch refs/heads/main\n\nworktree /repos/botanic-feature\nHEAD def456\nbranch refs/heads/feature\nlocked reason\n\n",
        )
        .unwrap();
        assert_eq!(worktrees.len(), 2);
        assert_eq!(worktrees[0].branch.as_deref(), Some("main"));
        assert!(worktrees[1].locked);
    }

    #[test]
    fn parses_status_counts_and_tracking() {
        let status = parse_status_summary(
            "# branch.oid abc\0# branch.head feature\0# branch.upstream origin/feature\0# branch.ab +2 -1\x001 M. N... 100644 100644 100644 abc def file\0? new-file\0! ignored-file\0",
        );
        assert_eq!(status.dirty_files, 2);
        assert_eq!(status.ignored_files, 1);
        assert_eq!(status.upstream.as_deref(), Some("origin/feature"));
        assert_eq!(status.ahead, Some(2));
        assert_eq!(status.behind, Some(1));
    }

    #[test]
    fn counts_a_rename_once_when_its_source_path_looks_like_a_record() {
        let status = parse_status_summary(
            "2 R. N... 100644 100644 100644 abc def R100 renamed\x002-source-name\0",
        );
        assert_eq!(status.dirty_files, 1);
    }

    #[test]
    fn recognizes_a_merged_github_pull_request() {
        let pull_request: PullRequest = serde_json::from_str(
            r#"{"number":42,"url":"https://github.com/org/repo/pull/42","state":"MERGED","headRefName":"feature/merged","mergedAt":"2026-01-02T03:04:05Z"}"#,
        )
        .unwrap();
        assert!(pull_request.is_merged());

        let open_pull_request: PullRequest = serde_json::from_str(
            r#"{"number":43,"url":"https://github.com/org/repo/pull/43","state":"OPEN","headRefName":"feature/open","mergedAt":null}"#,
        )
        .unwrap();
        assert!(!open_pull_request.is_merged());
    }

    #[test]
    fn creates_refreshes_and_guardedly_removes_a_real_worktree() {
        let root = std::env::temp_dir().join(format!(
            "wtui-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init"]).unwrap();
        git(&root, &["config", "user.name", "wtui test"]).unwrap();
        git(&root, &["config", "user.email", "wtui@example.test"]).unwrap();
        fs::write(root.join("README.md"), "test\n").unwrap();
        git(&root, &["add", "README.md"]).unwrap();
        git(&root, &["commit", "-m", "initial"]).unwrap();

        let worktree = add_worktree(root.clone(), "Feature/Test".to_owned()).unwrap();
        assert_eq!(worktree.parent(), root.parent());
        assert!(
            worktree
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("-feature-test"))
        );

        let refresh = refresh_repository(root.clone()).unwrap();
        let created = refresh
            .worktrees
            .iter()
            .find(|entry| entry.path == worktree)
            .unwrap();
        assert_eq!(created.branch.as_deref(), Some("Feature/Test"));
        assert_eq!(created.dirty_files, 0);

        fs::write(worktree.join("uncommitted.txt"), "change\n").unwrap();
        assert!(remove_worktree(root.clone(), worktree.clone(), false, None).is_err());
        remove_worktree(root.clone(), worktree.clone(), true, None).unwrap();
        assert!(!worktree.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
