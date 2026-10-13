mod persistence;
#[cfg(target_os = "windows")]
mod resumable_cli;
#[cfg(target_os = "windows")]
pub use resumable_cli::run_hook_helper as run_resumable_cli_hook_helper;
pub mod terminal_element;
pub mod terminal_panel;
mod terminal_path_like_target;
pub mod terminal_scrollbar;

use anyhow::Context as _;
use editor::{
    Editor, EditorSettings, actions::SelectAll, blink_manager::BlinkManager,
    ui_scrollbar_settings_from_raw,
};
use futures::{FutureExt as _, StreamExt as _, future::Shared};
use gpui::{
    Action, AnyElement, App, ClipboardEntry, Corners, DismissEvent, Entity, EventEmitter,
    ExternalPaths, FocusHandle, Focusable, Font, KeyContext, KeyDownEvent, Keystroke, MouseButton,
    MouseDownEvent, Pixels, Point as GpuiPoint, Rems, Render, ScrollWheelEvent, Styled,
    Subscription, Task, TaskExt, WeakEntity, actions, anchored, deferred, div,
};
use menu;
use persistence::{PersistentTerminalId, ProjectWorktreeAssociation, TerminalDb};
use project::{Project, ProjectEntryId, ProjectPath, search::SearchQuery};
use schemars::JsonSchema;
use serde::Deserialize;
use settings::{
    SeedQuerySetting, Settings, SettingsStore, TerminalBell, TerminalBlink, WorkingDirectory,
};
use std::{
    any::Any,
    cmp,
    ops::Range as StdRange,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use task::TaskId;
use terminal::{
    Clear, Copy, Event, HoveredWord, MaybeNavigationTarget, Modes, MouseInputMode, Paste,
    PasteText, Point, Range, ScrollLineDown, ScrollLineUp, ScrollPageDown, ScrollPageUp,
    ScrollToBottom, ScrollToTop, Search, ShowCharacterPalette, TaskState, TaskStatus, Terminal,
    TerminalBounds, ToggleViMode, is_known_terminal_agent_command,
    terminal_settings::{CursorShape, TerminalSettings},
};
use terminal_element::TerminalElement;
use terminal_panel::TerminalPanel;
use terminal_path_like_target::{hover_path_like_target, open_path_like_target};
use terminal_scrollbar::TerminalScrollHandle;
use ui::{
    ContextMenu, Divider, Indicator, ScrollAxes, Scrollbars, Tooltip, WithScrollbar,
    prelude::*,
    scrollbars::{self, ScrollbarVisibility},
};
use util::ResultExt;
use workspace::{
    CloseActiveItem, DraggedSelection, DraggedTab, NewCenterTerminal, NewTerminal, Pane,
    PersistentItemId, ToolbarItemLocation, Workspace, WorkspaceId, delete_unloaded_items,
    item::{
        HighlightedText, Item, ItemEvent, SerializableItem, TabContentParams, TabTooltipContent,
    },
    register_serializable_item,
    searchable::{
        Direction, SearchEvent, SearchOptions, SearchToken, SearchableItem, SearchableItemHandle,
    },
};
use zed_actions::{agent::AddSelectionToThread, assistant::InlineAssist};

struct ImeState {
    marked_text: String,
}

fn viewport_line_for_point(point: Point, display_offset: usize) -> Option<usize> {
    let display_offset = i32::try_from(display_offset).unwrap_or(i32::MAX);
    let line = point.line.saturating_add(display_offset);
    if line < 0 {
        None
    } else {
        usize::try_from(line).ok()
    }
}

const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// Event to transmit the scroll from the element to the view
#[derive(Clone, Debug, PartialEq)]
pub struct ScrollTerminal(pub i32);

/// Sends the specified text directly to the terminal.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = terminal)]
pub struct SendText(String);

/// Sends a keystroke sequence to the terminal.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = terminal)]
pub struct SendKeystroke(String);

actions!(
    terminal,
    [
        /// Reruns the last executed task in the terminal.
        RerunTask,
        /// Resumes the exact AI CLI session associated with this terminal.
        ResumeAiSession,
        /// Starts a new AI CLI session while retaining the old association until capture succeeds.
        StartNewAiSession,
        /// Removes the AI CLI association without deleting provider-owned history.
        ForgetAiSession,
    ]
);

/// Opens a center terminal whose Codex conversation can be resumed after restart.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = terminal)]
pub struct NewResumableCodexTerminal;

/// Opens a center terminal whose Claude conversation can be resumed after restart.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = terminal)]
pub struct NewResumableClaudeTerminal;

/// Opens a center terminal whose Copilot conversation can be resumed after restart.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = terminal)]
pub struct NewResumableCopilotTerminal;

/// Opens a center terminal whose Hermes conversation can be resumed after restart.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = terminal)]
pub struct NewResumableHermesTerminal;

/// Renames the terminal tab.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = terminal)]
pub struct RenameTerminal;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ResumableCliProvider {
    Codex,
    Claude,
    Copilot,
    Hermes,
}

impl ResumableCliProvider {
    fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Copilot => "copilot",
            Self::Hermes => "hermes",
        }
    }

    fn from_persisted(value: &str) -> anyhow::Result<Self> {
        match value {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            "copilot" => Ok(Self::Copilot),
            "hermes" => Ok(Self::Hermes),
            _ => anyhow::bail!("unknown resumable CLI provider: {value}"),
        }
    }

    fn validate_resume_id(self, resume_id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!resume_id.is_empty(), "resume ID cannot be empty");
        anyhow::ensure!(
            !resume_id.chars().any(char::is_control),
            "resume ID cannot contain control characters"
        );
        match self {
            Self::Codex => {
                uuid::Uuid::parse_str(resume_id)
                    .map_err(|error| anyhow::anyhow!("invalid Codex resume ID: {error}"))?;
            }
            Self::Claude => {
                uuid::Uuid::parse_str(resume_id)
                    .map_err(|error| anyhow::anyhow!("invalid Claude resume ID: {error}"))?;
            }
            Self::Copilot => {
                uuid::Uuid::parse_str(resume_id)
                    .map_err(|error| anyhow::anyhow!("invalid Copilot resume ID: {error}"))?;
            }
            Self::Hermes => {
                anyhow::ensure!(
                    regex::Regex::new(r"^\d{8}_\d{6}_[0-9a-fA-F]{6}$")?.is_match(resume_id),
                    "invalid Hermes resume ID"
                );
            }
        }
        Ok(())
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Copilot => "Copilot",
            Self::Hermes => "Hermes",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumableCliEvidence {
    ProviderEvent,
    ProviderWorkspace,
    CallerSelected,
    ExplicitAttachment,
}

impl ResumableCliEvidence {
    fn as_str(self) -> &'static str {
        match self {
            Self::ProviderEvent => "provider_event",
            Self::ProviderWorkspace => "provider_workspace",
            Self::CallerSelected => "caller_selected",
            Self::ExplicitAttachment => "explicit_attachment",
        }
    }

    fn from_persisted(value: &str) -> anyhow::Result<Self> {
        match value {
            "provider_event" => Ok(Self::ProviderEvent),
            "provider_workspace" => Ok(Self::ProviderWorkspace),
            "caller_selected" => Ok(Self::CallerSelected),
            "explicit_attachment" => Ok(Self::ExplicitAttachment),
            _ => anyhow::bail!("unknown resumable CLI evidence kind: {value}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResumableCliSession {
    provider: ResumableCliProvider,
    exact_resume_id: String,
    resume_cwd: PathBuf,
    evidence: ResumableCliEvidence,
    provider_home: Option<PathBuf>,
    provider_profile: Option<String>,
}

impl ResumableCliSession {
    pub fn new(
        provider: ResumableCliProvider,
        exact_resume_id: String,
        resume_cwd: PathBuf,
        evidence: ResumableCliEvidence,
    ) -> anyhow::Result<Self> {
        provider.validate_resume_id(&exact_resume_id)?;
        anyhow::ensure!(
            resume_cwd.is_absolute(),
            "resume working directory must be absolute"
        );
        Ok(Self {
            provider,
            exact_resume_id,
            resume_cwd,
            evidence,
            provider_home: None,
            provider_profile: None,
        })
    }

    pub(crate) fn with_provider_context(
        mut self,
        provider_home: PathBuf,
        provider_profile: String,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            self.provider == ResumableCliProvider::Hermes,
            "provider context is only supported for Hermes sessions"
        );
        anyhow::ensure!(
            provider_home.is_absolute(),
            "provider home must be absolute"
        );
        anyhow::ensure!(
            !provider_profile.is_empty(),
            "provider profile must not be empty"
        );
        self.provider_home = Some(provider_home);
        self.provider_profile = Some(provider_profile);
        Ok(self)
    }

    fn from_persisted(
        provider: ResumableCliProvider,
        exact_resume_id: String,
        resume_cwd: PathBuf,
        evidence: ResumableCliEvidence,
        provider_home: Option<PathBuf>,
        provider_profile: Option<String>,
    ) -> anyhow::Result<Self> {
        match (&provider_home, &provider_profile) {
            (None, None) => {}
            (Some(home), Some(profile)) => {
                anyhow::ensure!(
                    provider == ResumableCliProvider::Hermes,
                    "provider context is only supported for Hermes sessions"
                );
                anyhow::ensure!(home.is_absolute(), "provider home must be absolute");
                anyhow::ensure!(!profile.is_empty(), "provider profile must not be empty");
            }
            _ => anyhow::bail!("resumable CLI session has incomplete provider context"),
        }
        Ok(Self {
            provider,
            exact_resume_id,
            resume_cwd,
            evidence,
            provider_home,
            provider_profile,
        })
    }

    pub fn provider(&self) -> ResumableCliProvider {
        self.provider
    }

    pub fn exact_resume_id(&self) -> &str {
        &self.exact_resume_id
    }

    pub fn resume_cwd(&self) -> &Path {
        &self.resume_cwd
    }

    pub fn evidence(&self) -> ResumableCliEvidence {
        self.evidence
    }

    pub(crate) fn provider_home(&self) -> Option<&Path> {
        self.provider_home.as_deref()
    }

    pub(crate) fn provider_profile(&self) -> Option<&str> {
        self.provider_profile.as_deref()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResumableCliBinding {
    pending_codex_cwd: Option<PathBuf>,
    revision: u64,
    session: Option<ResumableCliSession>,
}

impl ResumableCliBinding {
    pub fn pending_codex_cwd(&self) -> Option<&Path> {
        self.pending_codex_cwd.as_deref()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn session(&self) -> Option<&ResumableCliSession> {
        self.session.as_ref()
    }
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ClaimObservation {
    provider_seen: bool,
    release: bool,
}

#[cfg(target_os = "windows")]
fn observe_claimed_process(
    provider: Option<ResumableCliProvider>,
    provider_seen: bool,
    command: Option<&str>,
) -> ClaimObservation {
    let Some(provider) = provider else {
        return ClaimObservation {
            provider_seen: false,
            release: true,
        };
    };
    if command.is_some_and(|command| command.eq_ignore_ascii_case(provider.as_str())) {
        ClaimObservation {
            provider_seen: true,
            release: false,
        }
    } else {
        ClaimObservation {
            provider_seen: false,
            release: provider_seen,
        }
    }
}

const SUPERSEDED_BINDING_ERROR: &str = "The terminal association was superseded by a newer change";

fn require_binding_revision(revision: Option<u64>) -> Result<u64, Arc<anyhow::Error>> {
    revision.ok_or_else(|| Arc::new(anyhow::anyhow!(SUPERSEDED_BINDING_ERROR)))
}

pub fn init(cx: &mut App) {
    terminal_panel::init(cx);

    register_serializable_item::<TerminalView>(cx);

    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(TerminalView::deploy);
        #[cfg(target_os = "windows")]
        {
            workspace.register_action(TerminalView::deploy_codex);
            workspace.register_action(TerminalView::deploy_claude);
            workspace.register_action(TerminalView::deploy_copilot);
            workspace.register_action(TerminalView::deploy_hermes);
        }
    })
    .detach();
}

pub struct BlockProperties {
    pub height: u8,
    pub render: Box<dyn Send + Fn(&mut BlockContext) -> AnyElement>,
}

pub struct BlockContext<'a, 'b> {
    pub window: &'a mut Window,
    pub context: &'b mut App,
    pub dimensions: TerminalBounds,
}

///A terminal view, maintains the PTY's file handles and communicates with the terminal
pub struct TerminalView {
    terminal: Entity<Terminal>,
    workspace: WeakEntity<Workspace>,
    project: WeakEntity<Project>,
    focus_handle: FocusHandle,
    has_bell: bool,
    context_menu: Option<(Entity<ContextMenu>, GpuiPoint<Pixels>, Subscription)>,
    cursor_shape: CursorShape,
    blink_manager: Entity<BlinkManager>,
    mode: TerminalMode,
    /// Corner radii for the terminal's background, for when it's embedded in a
    /// container with rounded corners.
    ///
    /// GPUI can't clip children to rounded corners, so without this the square
    /// background paints over the container's corners. Only the background is
    /// rounded, terminal content isn't clipped.
    background_corner_radii: Option<Corners<Rems>>,
    read_only: bool,
    // Explicit override for whether workspace-specific context menu actions are shown.
    // When `None`, visibility is derived from `mode` (hidden for embedded terminals).
    show_workspace_actions: Option<bool>,
    blinking_terminal_enabled: bool,
    needs_serialize: bool,
    persistence_generation: u64,
    persistence_task: Option<Shared<Task<Result<(), Arc<anyhow::Error>>>>>,
    custom_title: Option<String>,
    resumable_cli_binding: ResumableCliBinding,
    binding_persistence_task: Option<Shared<Task<Result<(), Arc<anyhow::Error>>>>>,
    #[cfg(target_os = "windows")]
    capture_cancellation: Option<resumable_cli::CaptureCancellation>,
    #[cfg(target_os = "windows")]
    cli_identity_task: Option<Task<()>>,
    #[cfg(target_os = "windows")]
    conceal_resumable_cli_launch: bool,
    #[cfg(target_os = "windows")]
    resumable_cli_launch_generation: u64,
    #[cfg(target_os = "windows")]
    session_claim: Option<resumable_cli::SessionClaim>,
    #[cfg(target_os = "windows")]
    claimed_process_seen: bool,
    #[cfg(target_os = "windows")]
    resume_attempt_generation: u64,
    #[cfg(target_os = "windows")]
    resume_attempt_cancellation: Option<futures::channel::oneshot::Sender<()>>,
    resume_on_first_focus: bool,
    resume_attempted: bool,
    hover: Option<HoverTarget>,
    hover_tooltip_update: Task<()>,
    workspace_id: Option<WorkspaceId>,
    persistent_terminal_id: Option<PersistentTerminalId>,
    project_worktree_association: ProjectWorktreeAssociation,
    show_breadcrumbs: bool,
    block_below_cursor: Option<Rc<BlockProperties>>,
    scroll_top: Pixels,
    scroll_handle: TerminalScrollHandle,
    ime_state: Option<ImeState>,
    self_handle: WeakEntity<Self>,
    rename_editor: Option<Entity<Editor>>,
    rename_editor_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
    _terminal_subscriptions: Vec<Subscription>,
}

#[derive(Default, Clone)]
pub enum TerminalMode {
    #[default]
    Standalone,
    Embedded {
        max_lines_when_unfocused: Option<usize>,
    },
}

#[derive(Clone)]
pub enum ContentMode {
    Scrollable,
    Inline {
        displayed_lines: usize,
        total_lines: usize,
    },
}

impl ContentMode {
    pub fn is_limited(&self) -> bool {
        match self {
            ContentMode::Scrollable => false,
            ContentMode::Inline {
                displayed_lines,
                total_lines,
            } => displayed_lines < total_lines,
        }
    }

    pub fn is_scrollable(&self) -> bool {
        matches!(self, ContentMode::Scrollable)
    }
}

#[derive(Debug)]
#[cfg_attr(test, derive(Clone, Eq, PartialEq))]
struct HoverTarget {
    tooltip: String,
    hovered_word: HoveredWord,
}

pub struct BellAcknowledged;

impl EventEmitter<BellAcknowledged> for TerminalView {}
impl EventEmitter<Event> for TerminalView {}
impl EventEmitter<ItemEvent> for TerminalView {}
impl EventEmitter<SearchEvent> for TerminalView {}

impl Drop for TerminalView {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        {
            if let Some(cancellation) = self.capture_cancellation.take() {
                cancellation.cancel();
            }
            self.invalidate_resume_attempt();
        }
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalView {
    ///Create a new Terminal in the current working directory or the user's home directory
    pub fn deploy(
        workspace: &mut Workspace,
        action: &NewCenterTerminal,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let local = action.local;
        let working_directory = default_working_directory(workspace, cx);
        TerminalPanel::add_center_terminal(workspace, window, cx, move |project, cx| {
            if local {
                project.create_local_terminal(cx)
            } else {
                project.create_terminal_shell(working_directory, cx)
            }
        })
        .detach_and_log_err(cx);
    }

    #[cfg(target_os = "windows")]
    pub fn deploy_codex(
        workspace: &mut Workspace,
        _: &NewResumableCodexTerminal,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(working_directory) = default_working_directory(workspace, cx) else {
            workspace.show_error(
                anyhow::anyhow!("A local working directory is required to launch Codex"),
                cx,
            );
            return;
        };
        let fs = workspace.app_state().fs.clone();

        cx.spawn_in(window, async move |workspace, cx| {
            let mut launched_terminal: Option<WeakEntity<TerminalView>> = None;
            let outcome: anyhow::Result<()> = async {
                let hooks_path = resumable_cli::codex_hooks_path()?;
                let existing = if fs.is_file(&hooks_path).await {
                    Some(fs.load(&hooks_path).await?)
                } else {
                    None
                };
                let hook_merge = resumable_cli::merge_codex_hook(existing.as_deref())?;
                if hook_merge.changed {
                    let detail = format!(
                        "Zed will add this entry to {} and preserve all unrelated hooks:\n\n{}",
                        hooks_path.display(),
                        hook_merge.entry_preview
                    );
                    let answer = workspace
                        .update_in(cx, |_, window, cx| {
                            window.prompt(
                                gpui::PromptLevel::Info,
                                "Allow resumable Codex terminals?",
                                Some(&detail),
                                &["Review and Add Globally", "Manual Instructions", "Cancel"],
                                cx,
                            )
                        })?
                        .await;
                    match answer {
                        Ok(0) => {
                            if let Some(parent) = hooks_path.parent() {
                                fs.create_dir(parent).await?;
                            }
                            fs.atomic_write(hooks_path, hook_merge.content).await?;
                        }
                        Ok(1) => {
                            let manual = format!(
                                "Add the reviewed entry to {} and run this action again:\n\n{}",
                                hooks_path.display(),
                                hook_merge.entry_preview
                            );
                            workspace
                                .update_in(cx, |_, window, cx| {
                                    window.prompt(
                                        gpui::PromptLevel::Info,
                                        "Manual Codex hook installation",
                                        Some(&manual),
                                        &["OK"],
                                        cx,
                                    )
                                })?
                                .await
                                .ok();
                            return Ok(());
                        }
                        _ => return Ok(()),
                    }
                }

                cx.background_executor()
                    .spawn(async { resumable_cli::verify_codex_contract() })
                    .await?;
                let listener =
                    resumable_cli::start_codex_identity_observer(working_directory.clone())?;
                let pending_cwd = working_directory.clone();
                let create = workspace.update_in(cx, |workspace, window, cx| {
                    TerminalPanel::add_center_terminal_with_env(
                        workspace,
                        working_directory,
                        listener.environment.clone(),
                        window,
                        cx,
                    )
                })?;
                let terminal_view = create.await?;
                let weak_view = terminal_view.downgrade();
                launched_terminal = Some(weak_view.clone());
                let (write, generation, terminal_id) = terminal_view.update(cx, |view, cx| {
                    let write =
                        view.replace_resumable_cli_association(None, Some(pending_cwd.clone()), cx);
                    (
                        write,
                        view.resume_attempt_generation,
                        view.terminal.entity_id(),
                    )
                });
                // Keep no strong view handle while saving. Closing the tab must
                // prevent launch even when its initial binding write is pending.
                drop(terminal_view);
                write.await?;
                if weak_view
                    .read_with(cx, |view, _| {
                        view.resume_attempt_generation == generation
                            && view.terminal.entity_id() == terminal_id
                            && view.resumable_cli_binding.pending_codex_cwd.as_ref()
                                == Some(&pending_cwd)
                    })
                    .unwrap_or(false)
                {
                    weak_view.update(cx, |view, cx| {
                        view.observe_cli_identity(listener, cx);
                        view.write_ai_command_after_startup(resumable_cli::codex_new_command(), cx);
                    })?;
                }
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.flush_serialization(window, cx)
                    })?
                    .await;
                Ok(())
            }
            .await;

            if let Err(error) = outcome {
                let message = launched_terminal
                    .as_ref()
                    .and_then(|terminal| {
                        terminal
                            .read_with(cx, |terminal, cx| {
                                terminal.capture_error_message(
                                    ResumableCliProvider::Codex,
                                    &error,
                                    cx,
                                )
                            })
                            .ok()
                    })
                    .unwrap_or_else(|| {
                        resumable_cli::capture_error_message(
                            ResumableCliProvider::Codex,
                            None,
                            None,
                            &error,
                        )
                    });
                workspace.update(cx, |workspace, cx| workspace.show_error(message, cx))?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    #[cfg(target_os = "windows")]
    pub fn deploy_claude(
        workspace: &mut Workspace,
        _: &NewResumableClaudeTerminal,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(working_directory) = default_working_directory(workspace, cx) else {
            workspace.show_error(
                anyhow::anyhow!("A local working directory is required to launch Claude"),
                cx,
            );
            return;
        };

        cx.spawn_in(window, async move |workspace, cx| {
            let mut launched_terminal: Option<WeakEntity<TerminalView>> = None;
            let outcome: anyhow::Result<()> = async {
                cx.background_executor()
                    .spawn(async { resumable_cli::verify_claude_contract() })
                    .await?;
                let listener = resumable_cli::start_claude_capture(working_directory.clone())?;
                let command =
                    resumable_cli::claude_new_command(&listener.session_id, &listener.plugin_path)?;
                let create = workspace.update_in(cx, |workspace, window, cx| {
                    TerminalPanel::add_center_terminal_with_env(
                        workspace,
                        working_directory,
                        listener.observer.environment.clone(),
                        window,
                        cx,
                    )
                })?;
                let terminal_view = create.await?;
                launched_terminal = Some(terminal_view.downgrade());
                terminal_view.update(cx, |terminal_view, cx| {
                    terminal_view.observe_cli_identity(listener.observer, cx);
                    terminal_view.write_ai_command_after_startup(command, cx);
                });
                Ok(())
            }
            .await;

            if let Err(error) = outcome {
                let message = launched_terminal
                    .as_ref()
                    .and_then(|terminal| {
                        terminal
                            .read_with(cx, |terminal, cx| {
                                terminal.capture_error_message(
                                    ResumableCliProvider::Claude,
                                    &error,
                                    cx,
                                )
                            })
                            .ok()
                    })
                    .unwrap_or_else(|| {
                        resumable_cli::capture_error_message(
                            ResumableCliProvider::Claude,
                            None,
                            None,
                            &error,
                        )
                    });
                workspace.update(cx, |workspace, cx| workspace.show_error(message, cx))?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    #[cfg(target_os = "windows")]
    pub fn deploy_copilot(
        workspace: &mut Workspace,
        _: &NewResumableCopilotTerminal,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(working_directory) = default_working_directory(workspace, cx) else {
            workspace.show_error(
                anyhow::anyhow!("A local working directory is required to launch Copilot"),
                cx,
            );
            return;
        };

        cx.spawn_in(window, async move |workspace, cx| {
            let mut launched_terminal: Option<WeakEntity<TerminalView>> = None;
            let outcome: anyhow::Result<()> = async {
                cx.background_executor()
                    .spawn(async { resumable_cli::verify_copilot_contract() })
                    .await?;
                let listener = resumable_cli::start_copilot_capture(working_directory.clone())?;
                let command = resumable_cli::copilot_new_command(&listener.session_id)?;
                let create = workspace.update_in(cx, |workspace, window, cx| {
                    TerminalPanel::add_center_terminal_with_env(
                        workspace,
                        working_directory,
                        listener.environment.clone(),
                        window,
                        cx,
                    )
                })?;
                let terminal_view = create.await?;
                launched_terminal = Some(terminal_view.downgrade());
                let capture_cancellation = listener.cancellation.clone();
                terminal_view.update(cx, |terminal_view, cx| {
                    terminal_view.begin_capture_attempt(capture_cancellation.clone());
                    terminal_view.write_ai_command_after_startup(command, cx);
                });

                let terminal_view = terminal_view.downgrade();
                let capture = listener.result.await;
                if capture_cancellation.is_cancelled() {
                    return Ok(());
                }
                let capture =
                    capture.context("Copilot identity record capture ended before capture")??;
                anyhow::ensure!(
                    terminal_view.update(cx, |terminal_view, _| terminal_view
                        .complete_capture_attempt(&capture_cancellation))?,
                    "Copilot identity arrived after the launch attempt was replaced"
                );
                let claim = resumable_cli::try_claim_target(&capture.session)
                    .context("Copilot reported an AI session already active in another terminal")?;
                terminal_view
                    .update(cx, |terminal_view, cx| {
                        terminal_view.set_resumable_cli_session_with_claim(
                            capture.session,
                            claim,
                            cx,
                        )
                    })?
                    .await?;
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.flush_serialization(window, cx)
                    })?
                    .await;
                Ok(())
            }
            .await;

            if let Err(error) = outcome {
                let message = launched_terminal
                    .as_ref()
                    .and_then(|terminal| {
                        terminal
                            .read_with(cx, |terminal, cx| {
                                terminal.capture_error_message(
                                    ResumableCliProvider::Copilot,
                                    &error,
                                    cx,
                                )
                            })
                            .ok()
                    })
                    .unwrap_or_else(|| {
                        resumable_cli::capture_error_message(
                            ResumableCliProvider::Copilot,
                            None,
                            None,
                            &error,
                        )
                    });
                workspace.update(cx, |workspace, cx| workspace.show_error(message, cx))?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    #[cfg(target_os = "windows")]
    pub fn deploy_hermes(
        workspace: &mut Workspace,
        _: &NewResumableHermesTerminal,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(working_directory) = default_working_directory(workspace, cx) else {
            workspace.show_error(
                anyhow::anyhow!("A local working directory is required to launch Hermes"),
                cx,
            );
            return;
        };
        let fs = workspace.app_state().fs.clone();

        cx.spawn_in(window, async move |workspace, cx| {
            let mut launched_terminal: Option<WeakEntity<TerminalView>> = None;
            let outcome: anyhow::Result<()> = async {
                let hermes_profile = resumable_cli::active_hermes_profile()?;
                let config_path = resumable_cli::hermes_config_path(&hermes_profile);
                let existing = if fs.is_file(&config_path).await {
                    Some(fs.load(&config_path).await?)
                } else {
                    None
                };
                let hook_merge = resumable_cli::merge_hermes_hook(existing.as_deref())?;
                if hook_merge.changed {
                    let detail = format!(
                        "Zed will add this entry to {} and preserve unrelated configuration. Hermes will separately ask you to approve this exact command on first use:\n\n{}",
                        config_path.display(),
                        hook_merge.entry_preview
                    );
                    let answer = workspace
                        .update_in(cx, |_, window, cx| {
                            window.prompt(
                                gpui::PromptLevel::Info,
                                "Allow resumable Hermes terminals?",
                                Some(&detail),
                                &["Review and Add Globally", "Manual Instructions", "Cancel"],
                                cx,
                            )
                        })?
                        .await;
                    match answer {
                        Ok(0) => {
                            if let Some(parent) = config_path.parent() {
                                fs.create_dir(parent).await?;
                            }
                            fs.atomic_write(config_path, hook_merge.content).await?;
                        }
                        Ok(1) => {
                            let manual = format!(
                                "Add the reviewed entry to {} and run this action again:\n\n{}",
                                config_path.display(),
                                hook_merge.entry_preview
                            );
                            workspace
                                .update_in(cx, |_, window, cx| {
                                    window.prompt(
                                        gpui::PromptLevel::Info,
                                        "Manual Hermes hook installation",
                                        Some(&manual),
                                        &["OK"],
                                        cx,
                                    )
                                })?
                                .await
                                .ok();
                            return Ok(());
                        }
                        _ => return Ok(()),
                    }
                }

                cx.background_executor()
                    .spawn(async { resumable_cli::verify_hermes_contract() })
                    .await?;
                let listener = resumable_cli::start_hermes_capture(
                    working_directory.clone(),
                    hermes_profile.clone(),
                )?;
                let create = workspace.update_in(cx, |workspace, window, cx| {
                    TerminalPanel::add_center_terminal_with_env(
                        workspace,
                        working_directory,
                        listener.environment,
                        window,
                        cx,
                    )
                })?;
                let terminal_view = create.await?;
                launched_terminal = Some(terminal_view.downgrade());
                let capture_cancellation = listener.cancellation.clone();
                terminal_view.update(cx, |terminal_view, cx| {
                    terminal_view.begin_capture_attempt(capture_cancellation.clone());
                    terminal_view.write_ai_command_after_startup(
                        resumable_cli::hermes_new_command(&hermes_profile),
                        cx,
                    );
                });

                let terminal_view = terminal_view.downgrade();
                let capture = listener.result.await;
                    if capture_cancellation.is_cancelled() {
                        return Ok(());
                    }
                    let capture = capture.context("Hermes identity callback ended before capture")??;
                anyhow::ensure!(
                    terminal_view.update(cx, |terminal_view, _| terminal_view
                        .complete_capture_attempt(&capture_cancellation))?,
                    "Hermes identity arrived after the launch attempt was replaced"
                );
                let claim = resumable_cli::try_claim_target(&capture.session).context(
                    "Hermes reported an AI session already active in another terminal",
                )?;
                terminal_view
                    .update(cx, |terminal_view, cx| {
                        terminal_view.set_resumable_cli_session_with_claim(
                            capture.session,
                            claim,
                            cx,
                        )
                    })?
                    .await?;
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.flush_serialization(window, cx)
                    })?
                    .await;
                Ok(())
            }
            .await;

            if let Err(error) = outcome {
                let message = launched_terminal
                    .as_ref()
                    .and_then(|terminal| terminal.read_with(cx, |terminal, cx| {
                        terminal.capture_error_message(ResumableCliProvider::Hermes, &error, cx)
                    }).ok())
                    .unwrap_or_else(|| resumable_cli::capture_error_message(
                        ResumableCliProvider::Hermes, None, None, &error,
                    ));
                workspace.update(cx, |workspace, cx| workspace.show_error(message, cx))?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn new(
        terminal: Entity<Terminal>,
        workspace: WeakEntity<Workspace>,
        workspace_id: Option<WorkspaceId>,
        project: WeakEntity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace_handle = workspace.clone();
        let terminal_subscriptions =
            subscribe_for_terminal_events(&terminal, workspace, window, cx);

        let focus_handle = cx.focus_handle();
        let focus_in = cx.on_focus_in(&focus_handle, window, |terminal_view, window, cx| {
            terminal_view.focus_in(window, cx);
        });
        let focus_out = cx.on_focus_out(
            &focus_handle,
            window,
            |terminal_view, _event, window, cx| {
                terminal_view.focus_out(window, cx);
            },
        );
        let window_activation = cx.observe_window_activation(window, |view, window, cx| {
            if window.is_window_active() && view.focus_handle.is_focused(window) {
                view.clear_bell(cx);
            }
        });
        let cursor_shape = TerminalSettings::get_global(cx).cursor_shape;

        let scroll_handle = TerminalScrollHandle::new(terminal.read(cx));

        let blink_manager = cx.new(|cx| {
            BlinkManager::new(
                CURSOR_BLINK_INTERVAL,
                |cx| {
                    !matches!(
                        TerminalSettings::get_global(cx).blinking,
                        TerminalBlink::Off
                    )
                },
                cx,
            )
        });

        let subscriptions = vec![
            focus_in,
            focus_out,
            window_activation,
            cx.observe(&blink_manager, |_, _, cx| cx.notify()),
            cx.observe_global::<SettingsStore>(Self::settings_changed),
        ];

        let project_worktree_association =
            ProjectWorktreeAssociation::Recorded(project.upgrade().and_then(|project| {
                project_worktree_root_for_directory(
                    project.read(cx),
                    terminal.read(cx).initial_working_directory(),
                    cx,
                )
            }));

        Self {
            terminal,
            workspace: workspace_handle,
            project,
            has_bell: false,
            focus_handle,
            context_menu: None,
            cursor_shape,
            blink_manager,
            blinking_terminal_enabled: false,
            hover: None,
            hover_tooltip_update: Task::ready(()),
            mode: TerminalMode::Standalone,
            background_corner_radii: None,
            read_only: false,
            show_workspace_actions: None,
            workspace_id,
            persistent_terminal_id: None,
            project_worktree_association,
            show_breadcrumbs: TerminalSettings::get_global(cx).toolbar.breadcrumbs,
            block_below_cursor: None,
            scroll_top: Pixels::ZERO,
            scroll_handle,
            needs_serialize: true,
            persistence_generation: 1,
            persistence_task: None,
            custom_title: None,
            resumable_cli_binding: ResumableCliBinding::default(),
            binding_persistence_task: None,
            #[cfg(target_os = "windows")]
            capture_cancellation: None,
            #[cfg(target_os = "windows")]
            cli_identity_task: None,
            #[cfg(target_os = "windows")]
            conceal_resumable_cli_launch: false,
            #[cfg(target_os = "windows")]
            resumable_cli_launch_generation: 0,
            #[cfg(target_os = "windows")]
            session_claim: None,
            #[cfg(target_os = "windows")]
            claimed_process_seen: false,
            #[cfg(target_os = "windows")]
            resume_attempt_generation: 0,
            #[cfg(target_os = "windows")]
            resume_attempt_cancellation: None,
            resume_on_first_focus: false,
            resume_attempted: false,
            ime_state: None,
            self_handle: cx.entity().downgrade(),
            rename_editor: None,
            rename_editor_subscription: None,
            _subscriptions: subscriptions,
            _terminal_subscriptions: terminal_subscriptions,
        }
    }

    pub fn project_worktree_root(&self, cx: &App) -> Option<PathBuf> {
        let ProjectWorktreeAssociation::Recorded(Some(root)) = &self.project_worktree_association
        else {
            return None;
        };
        let project = self.project.upgrade()?;
        project
            .read(cx)
            .worktrees(cx)
            .any(|worktree| worktree.read(cx).abs_path().as_ref() == root.as_path())
            .then(|| root.clone())
    }

    fn with_persistent_terminal_id(mut self, persistent_terminal_id: PersistentTerminalId) -> Self {
        self.persistent_terminal_id = Some(persistent_terminal_id);
        self.needs_serialize = true;
        self.persistence_generation = self.persistence_generation.wrapping_add(1);
        self
    }

    /// Enable 'embedded' mode where the terminal displays the full content with an optional limit of lines.
    pub fn set_embedded_mode(
        &mut self,
        max_lines_when_unfocused: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.mode = TerminalMode::Embedded {
            max_lines_when_unfocused,
        };
        cx.notify();
    }

    /// Rounds the background without clipping terminal content to the corners.
    pub fn set_background_corner_radii(
        &mut self,
        corner_radii: Option<Corners<Rems>>,
        cx: &mut Context<Self>,
    ) {
        self.background_corner_radii = corner_radii;
        cx.notify();
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Fixes the view's input policy at construction, without restricting producer output
    /// or local navigation.
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    fn mouse_input_mode(&self) -> MouseInputMode {
        if self.read_only {
            MouseInputMode::LocalSelection
        } else {
            MouseInputMode::ReportToTerminal
        }
    }

    /// Explicitly override whether workspace-specific context menu actions (e.g. creating or
    /// closing terminal tabs, inline assist) are shown.
    ///
    /// This lets hosts that aren't workspace panes (such as the agent panel) hide these
    /// actions without `terminal_view` needing to know about those hosts. When never called,
    /// visibility is derived from the terminal's `mode`.
    pub fn set_show_workspace_actions(&mut self, show: bool, cx: &mut Context<Self>) {
        self.show_workspace_actions = Some(show);
        cx.notify();
    }

    fn shows_workspace_actions(&self) -> bool {
        self.show_workspace_actions
            .unwrap_or_else(|| !matches!(self.mode, TerminalMode::Embedded { .. }))
    }

    const MAX_EMBEDDED_LINES: usize = 1_000;

    /// Returns the current `ContentMode` depending on the set `TerminalMode` and the current number of lines
    ///
    /// Note: Even in embedded mode, the terminal will fallback to scrollable when its content exceeds `MAX_EMBEDDED_LINES`
    pub fn content_mode(&self, window: &Window, cx: &App) -> ContentMode {
        match &self.mode {
            TerminalMode::Standalone => ContentMode::Scrollable,
            TerminalMode::Embedded {
                max_lines_when_unfocused,
            } => {
                let terminal = self.terminal.read(cx);
                let total_lines = terminal.total_lines();

                if total_lines > Self::MAX_EMBEDDED_LINES {
                    ContentMode::Scrollable
                } else {
                    let mut displayed_lines = terminal.used_lines().min(total_lines);

                    if !self.focus_handle.is_focused(window)
                        && let Some(max_lines) = max_lines_when_unfocused
                    {
                        displayed_lines = displayed_lines.min(*max_lines)
                    }

                    ContentMode::Inline {
                        displayed_lines,
                        total_lines,
                    }
                }
            }
        }
    }

    /// Sets the marked (pre-edit) text from the IME.
    pub(crate) fn set_marked_text(&mut self, text: String, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        if text.is_empty() {
            return self.clear_marked_text(cx);
        }
        self.ime_state = Some(ImeState { marked_text: text });
        cx.notify();
    }

    /// Gets the current marked range (UTF-16).
    pub(crate) fn marked_text_range(&self) -> Option<StdRange<usize>> {
        self.ime_state
            .as_ref()
            .map(|state| 0..state.marked_text.encode_utf16().count())
    }

    /// Clears the marked (pre-edit) text state.
    pub(crate) fn clear_marked_text(&mut self, cx: &mut Context<Self>) {
        if self.ime_state.is_some() {
            self.ime_state = None;
            cx.notify();
        }
    }

    /// Commits (sends) the given text to the PTY. Called by InputHandler::replace_text_in_range.
    pub(crate) fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if !self.read_only && !text.is_empty() {
            self.terminal.update(cx, |term, _| {
                term.input(text.to_string().into_bytes());
            });
        }
    }

    pub(crate) fn terminal_bounds(&self, cx: &App) -> TerminalBounds {
        self.terminal.read(cx).last_content().terminal_bounds
    }

    pub fn entity(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    pub fn has_bell(&self) -> bool {
        self.has_bell
    }

    pub fn custom_title(&self) -> Option<&str> {
        self.custom_title.as_deref()
    }

    pub fn resumable_cli_binding(&self) -> &ResumableCliBinding {
        &self.resumable_cli_binding
    }

    pub fn resumable_cli_provider(&self) -> Option<ResumableCliProvider> {
        self.resumable_cli_binding
            .session()
            .map(ResumableCliSession::provider)
            .or_else(|| {
                self.resumable_cli_binding
                    .pending_codex_cwd
                    .as_ref()
                    .map(|_| ResumableCliProvider::Codex)
            })
    }

    pub fn active_resumable_cli_provider(&self) -> Option<ResumableCliProvider> {
        #[cfg(target_os = "windows")]
        {
            self.session_claim
                .as_ref()
                .and_then(|_| self.resumable_cli_provider())
        }
        #[cfg(not(target_os = "windows"))]
        {
            None
        }
    }

    #[cfg(target_os = "windows")]
    fn observe_cli_identity(
        &mut self,
        observer: resumable_cli::CliIdentityObserver,
        cx: &mut Context<Self>,
    ) {
        self.begin_capture_attempt(observer.cancellation.clone());
        let cancellation = observer.cancellation.clone();
        let terminal_id = self.terminal.entity_id();
        self.cli_identity_task = Some(cx.spawn(async move |this, cx| {
            let mut observer = observer;
            let mut callback_error_shown = false;
            while let Some(event) = observer.events.next().await {
                let current = this
                    .read_with(cx, |view, _| {
                        view.terminal.entity_id() == terminal_id
                            && view
                                .capture_cancellation
                                .as_ref()
                                .is_some_and(|current| current.is_same_attempt(&cancellation))
                            && !cancellation.is_cancelled()
                    })
                    .unwrap_or(false);
                if !current {
                    break;
                }
                let capture = match event {
                    Ok(capture) => capture,
                    Err(error) => {
                        // The listener stays alive after malformed callbacks. A dismissed
                        // warning must not reappear on every callback from the same launch.
                        if callback_error_shown {
                            log::debug!("Suppressed repeated CLI identity callback error");
                        } else {
                            callback_error_shown = true;
                            if let Err(update_error) =
                                this.update(cx, |view, cx| view.show_resumable_cli_error(error, cx))
                            {
                                log::error!(
                                    "CLI identity error could not be shown: {update_error:#}"
                                );
                                break;
                            }
                        }
                        continue;
                    }
                };
                let result = async {
                    let write = this.update(
                        cx,
                        |view, cx| -> anyhow::Result<Option<Task<anyhow::Result<()>>>> {
                            // A resumed root thread can still finish startup hooks after
                            // another thread receives a prompt. Only prompt evidence may
                            // update an existing conversation or its execution context.
                            if !capture.is_prompt
                                && capture.session.provider() == ResumableCliProvider::Codex
                                && view.resumable_cli_binding.session.is_some()
                            {
                                return Ok(None);
                            }
                            if view.resumable_cli_binding.session.as_ref() == Some(&capture.session)
                                && view.session_claim.is_some()
                            {
                                return Ok(None);
                            }
                            if view
                                .session_claim
                                .as_ref()
                                .is_some_and(|claim| claim.owns(&capture.session))
                            {
                                return Ok(Some(
                                    view.set_resumable_cli_session(capture.session, cx),
                                ));
                            }
                            let claim = resumable_cli::try_claim_target(&capture.session).context(
                                "CLI conversation is already active in another terminal",
                            )?;
                            Ok(Some(view.set_resumable_cli_session_with_claim(
                                capture.session,
                                claim,
                                cx,
                            )))
                        },
                    )??;
                    if let Some(write) = write {
                        write.await?;
                    }
                    anyhow::Ok(())
                }
                .await;
                if let Err(error) = result {
                    if let Err(update_error) =
                        this.update(cx, |view, cx| view.show_resumable_cli_error(error, cx))
                    {
                        log::error!("CLI identity error could not be shown: {update_error:#}");
                        break;
                    }
                }
            }
            cancellation.cancel();
        }));
    }

    #[cfg(target_os = "windows")]
    fn begin_capture_attempt(&mut self, cancellation: resumable_cli::CaptureCancellation) {
        self.cli_identity_task = None;
        if let Some(previous) = self.capture_cancellation.replace(cancellation) {
            previous.cancel();
        }
    }

    #[cfg(target_os = "windows")]
    fn complete_capture_attempt(
        &mut self,
        cancellation: &resumable_cli::CaptureCancellation,
    ) -> bool {
        if self
            .capture_cancellation
            .as_ref()
            .is_some_and(|current| current.is_same_attempt(cancellation))
        {
            self.capture_cancellation = None;
            true
        } else {
            false
        }
    }

    #[cfg(target_os = "windows")]
    fn begin_resume_attempt(&mut self) -> (u64, futures::channel::oneshot::Receiver<()>) {
        self.invalidate_resume_attempt();
        let (cancellation, cancelled) = futures::channel::oneshot::channel();
        self.resume_attempt_cancellation = Some(cancellation);
        (self.resume_attempt_generation, cancelled)
    }

    #[cfg(target_os = "windows")]
    fn invalidate_resume_attempt(&mut self) {
        if let Some(cancellation) = self.resume_attempt_cancellation.take() {
            let _ = cancellation.send(());
        }
        self.resume_attempt_generation = self.resume_attempt_generation.wrapping_add(1);
    }

    #[cfg(target_os = "windows")]
    fn complete_resume_attempt(&mut self, generation: u64) {
        if self.resume_attempt_generation == generation {
            self.resume_attempt_cancellation = None;
        }
    }

    #[cfg(target_os = "windows")]
    fn resume_attempt_is_current(
        &self,
        generation: u64,
        terminal_id: gpui::EntityId,
        expected_revision: u64,
        expected_session: &ResumableCliSession,
    ) -> bool {
        self.resume_attempt_generation == generation
            && self.terminal.entity_id() == terminal_id
            && self.resumable_cli_binding.revision == expected_revision
            && self.resumable_cli_binding.session.as_ref() == Some(expected_session)
    }

    #[cfg(target_os = "windows")]
    fn update_session_claim_for_foreground_process(&mut self, command: Option<&str>) {
        let Some(provider) = self.resumable_cli_provider() else {
            // New launches have no durable identity until their capture completes.
            // Shell startup events must not cancel the capture that establishes it.
            return;
        };
        let observation =
            observe_claimed_process(Some(provider), self.claimed_process_seen, command);
        self.claimed_process_seen = observation.provider_seen;
        if observation.release {
            if let Some(cancellation) = self.capture_cancellation.take() {
                cancellation.cancel();
            }
            self.cli_identity_task = None;
            if self.session_claim.take().is_some() {
                log::info!(
                    "released {} resumable session claim after foreground process changed to {}",
                    provider.as_str(),
                    command.unwrap_or("<none>")
                );
            }
        }
    }

    pub fn set_resumable_cli_session(
        &mut self,
        session: ResumableCliSession,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.replace_resumable_cli_session(Some(session), cx)
    }

    #[cfg(target_os = "windows")]
    fn set_resumable_cli_session_with_claim(
        &mut self,
        session: ResumableCliSession,
        claim: resumable_cli::SessionClaim,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let expected_session = session.clone();
        let terminal_id = self.terminal.entity_id();
        let persistence = self.replace_resumable_cli_session(Some(session), cx);
        let generation = self.resume_attempt_generation;
        cx.spawn(async move |this, cx| {
            persistence.await?;
            this.update(cx, |this, _| {
                // Forget, replacement or another write can supersede this callback
                // while persistence is in flight. Do not revive its process claim.
                if this.resume_attempt_generation != generation
                    || this.terminal.entity_id() != terminal_id
                    || this.resumable_cli_binding.session.as_ref() != Some(&expected_session)
                {
                    return;
                }
                this.session_claim = Some(claim);
                // A successful identity callback is emitted from the provider
                // process itself, so the running-process claim is established
                // even if process discovery observed the provider before the
                // durable binding write completed.
                this.claimed_process_seen = true;
            })?;
            Ok(())
        })
    }

    pub fn forget_resumable_cli_session(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        #[cfg(target_os = "windows")]
        if let Some(cancellation) = self.capture_cancellation.take() {
            cancellation.cancel();
        }
        #[cfg(target_os = "windows")]
        {
            self.cli_identity_task = None;
        }
        self.replace_resumable_cli_association(None, None, cx)
    }

    fn replace_resumable_cli_session(
        &mut self,
        session: Option<ResumableCliSession>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.replace_resumable_cli_association(session, None, cx)
    }

    fn replace_resumable_cli_association(
        &mut self,
        session: Option<ResumableCliSession>,
        pending_codex_cwd: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        #[cfg(target_os = "windows")]
        self.invalidate_resume_attempt();
        let Some(workspace_id) = self.workspace_id else {
            return Task::ready(Err(anyhow::anyhow!(
                "resumable CLI association requires a persisted workspace"
            )));
        };
        if self.terminal.read(cx).task().is_some() {
            return Task::ready(Err(anyhow::anyhow!(
                "task terminals cannot own resumable CLI associations"
            )));
        }

        let db = TerminalDb::global(cx);
        let persistent_terminal_id = self.persistent_terminal_id.clone();
        let item_id = cx.entity_id().as_u64();
        let previous_binding_persistence_task = self.binding_persistence_task.take();
        let binding_persistence_task = cx
            .spawn(async move |this, cx| {
                if let Some(previous_binding_persistence_task) = previous_binding_persistence_task
                    && let Err(error) = previous_binding_persistence_task.await
                {
                    log::error!("previous resumable CLI binding write failed: {error:#}");
                }
                let expected_revision = this
                    .read_with(cx, |this, _| this.resumable_cli_binding.revision)
                    .map_err(Arc::new)?;
                let revision = if let Some(persistent_terminal_id) = persistent_terminal_id {
                    db.compare_and_set_persistent_center_resumable_cli_association(
                        persistent_terminal_id,
                        workspace_id,
                        expected_revision,
                        session.clone(),
                        pending_codex_cwd.clone(),
                    )
                    .await
                    .map_err(Arc::new)?
                } else {
                    db.compare_and_set_resumable_cli_association(
                        item_id,
                        workspace_id,
                        expected_revision,
                        session.clone(),
                        pending_codex_cwd.clone(),
                    )
                    .await
                    .map_err(Arc::new)?
                };
                if revision.is_none() {
                    log::error!(
                        "resumable CLI binding compare-and-set rejected revision {expected_revision}"
                    );
                }
                let revision = require_binding_revision(revision)?;

                this.update(cx, |this, cx| {
                    #[cfg(target_os = "windows")]
                    {
                        if !this.session_claim.as_ref().is_some_and(|claim| {
                            session.as_ref().is_some_and(|session| claim.owns(session))
                        }) {
                            this.session_claim = None;
                            this.claimed_process_seen = false;
                        }
                    }
                    this.resumable_cli_binding = ResumableCliBinding { revision, session, pending_codex_cwd };
                    cx.emit(ItemEvent::UpdateTab);
                    cx.notify();
                })
                .map_err(Arc::new)
            })
            .shared();
        self.binding_persistence_task = Some(binding_persistence_task.clone());
        cx.spawn(async move |_, _| {
            binding_persistence_task
                .await
                .map_err(|error| anyhow::anyhow!("{error:#}"))
        })
    }

    #[cfg(target_os = "windows")]
    fn write_ai_command_after_startup(&mut self, command: Vec<u8>, cx: &mut Context<Self>) {
        self.conceal_resumable_cli_launch = true;
        self.resumable_cli_launch_generation = self.resumable_cli_launch_generation.wrapping_add(1);
        let launch_generation = self.resumable_cli_launch_generation;
        cx.notify();
        let startup = self.terminal.update(cx, |terminal, _| {
            terminal.start_init_command_startup_handshake()
        });
        let terminal = self.terminal.downgrade();
        cx.spawn(async move |this, cx| {
            let timeout = cx.background_executor().timer(Duration::from_secs(5));
            futures::select_biased! {
                _ = startup.fuse() => {}
                _ = timeout.fuse() => {}
            }
            let delivered = terminal.update(cx, |terminal, cx| {
                terminal.write_init_command_after_startup(command, cx)
            })?;
            if !delivered {
                this.update(cx, |this, cx| {
                    if this.resumable_cli_launch_generation == launch_generation {
                        this.conceal_resumable_cli_launch = false;
                        this.show_resumable_cli_error(
                            anyhow::anyhow!("The terminal was used before the agent could launch. Start a new resumable agent terminal and try again."),
                            cx,
                        );
                        cx.notify();
                    }
                })?;
            } else {
                cx.background_executor()
                    .timer(Duration::from_secs(10))
                    .await;
                this.update(cx, |this, cx| {
                    if this.resumable_cli_launch_generation == launch_generation
                        && this.conceal_resumable_cli_launch
                    {
                        this.conceal_resumable_cli_launch = false;
                        cx.notify();
                    }
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    #[cfg(target_os = "windows")]
    fn should_conceal_resumable_cli_launch(&mut self, cx: &App) -> bool {
        if !self.conceal_resumable_cli_launch {
            return false;
        }

        if self
            .terminal
            .read(cx)
            .last_content
            .mode
            .contains(Modes::ALT_SCREEN)
        {
            self.conceal_resumable_cli_launch = false;
            false
        } else {
            true
        }
    }

    #[cfg(target_os = "windows")]
    fn replace_terminal(
        &mut self,
        terminal: Entity<Terminal>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.invalidate_resume_attempt();
        if let Some(cancellation) = self.capture_cancellation.take() {
            cancellation.cancel();
        }
        self.cli_identity_task = None;
        self.session_claim = None;
        self.claimed_process_seen = false;
        self._terminal_subscriptions =
            subscribe_for_terminal_events(&terminal, self.workspace.clone(), window, cx);
        self.scroll_handle = TerminalScrollHandle::new(terminal.read(cx));
        self.terminal = terminal;
        self.has_bell = false;
        self.resume_on_first_focus = false;
        self.resume_attempted = false;
        cx.notify();
    }

    pub fn set_custom_title(&mut self, label: Option<String>, cx: &mut Context<Self>) {
        let label = label.filter(|l| !l.trim().is_empty());
        if self.custom_title != label {
            self.custom_title = label;
            self.needs_serialize = true;
            self.persistence_generation = self.persistence_generation.wrapping_add(1);
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        }
    }

    pub(crate) fn persist_snapshot(
        &mut self,
        item_id: workspace::ItemId,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let terminal = self.terminal().read(cx);
        if terminal.task().is_some() {
            return None;
        }

        if !self.needs_serialize {
            return None;
        }

        let workspace_id = self.workspace_id?;
        let persistent_terminal_id = self.persistent_terminal_id.clone();
        let cwd = terminal.working_directory();
        let custom_title = self.custom_title.clone();
        let project_worktree_association = self.project_worktree_association.clone();
        let persistence_generation = self.persistence_generation;
        let previous_persistence_task = self.persistence_task.take();

        let db = TerminalDb::global(cx);
        let persistence_task = cx
            .spawn(async move |this, cx| {
                if let Some(previous_persistence_task) = previous_persistence_task
                    && let Err(error) = previous_persistence_task.await
                {
                    log::error!("previous terminal snapshot write failed: {error:#}");
                }
                if let Some(persistent_terminal_id) = persistent_terminal_id {
                    db.save_persistent_center_snapshot_with_association(
                        persistent_terminal_id,
                        workspace_id,
                        cwd,
                        custom_title,
                        project_worktree_association,
                    )
                    .await
                    .map_err(Arc::new)?;
                } else {
                    db.save_snapshot_with_association(
                        item_id,
                        workspace_id,
                        cwd,
                        custom_title,
                        project_worktree_association,
                    )
                    .await
                    .map_err(Arc::new)?;
                }
                this.update(cx, |this, _| {
                    if this.persistence_generation == persistence_generation {
                        this.needs_serialize = false;
                    }
                })
                .map_err(Arc::new)?;
                Ok(())
            })
            .shared();
        self.persistence_task = Some(persistence_task.clone());
        Some(cx.spawn(async move |_, _| {
            persistence_task
                .await
                .map_err(|error| anyhow::anyhow!("{error:#}"))
        }))
    }

    pub(crate) fn mark_needs_serialize(&mut self, cx: &mut Context<Self>) {
        self.needs_serialize = true;
        self.persistence_generation = self.persistence_generation.wrapping_add(1);
        cx.emit(ItemEvent::UpdateTab);
    }

    pub fn is_renaming(&self) -> bool {
        self.rename_editor.is_some()
    }

    pub fn rename_editor_is_focused(&self, window: &Window, cx: &App) -> bool {
        self.rename_editor
            .as_ref()
            .is_some_and(|editor| editor.focus_handle(cx).is_focused(window))
    }

    fn finish_renaming(&mut self, save: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.rename_editor.take() else {
            return;
        };
        self.rename_editor_subscription = None;
        if save {
            let new_label = editor.read(cx).text(cx).trim().to_string();
            let label = if new_label.is_empty() {
                None
            } else {
                // Only set custom_title if the text differs from the terminal's dynamic title.
                // This prevents subtle layout changes when clicking away without making changes.
                let terminal_title = self.terminal.read(cx).title(true);
                if new_label == terminal_title {
                    None
                } else {
                    Some(new_label)
                }
            };
            self.set_custom_title(label, cx);
        }
        cx.notify();
        self.focus_handle.focus(window, cx);
    }

    pub fn rename_terminal(
        &mut self,
        _: &RenameTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.terminal.read(cx).task().is_some() {
            return;
        }

        let current_label = self
            .custom_title
            .clone()
            .unwrap_or_else(|| self.terminal.read(cx).title(true));

        let rename_editor = cx.new(|cx| Editor::single_line(window, cx));
        let rename_editor_subscription = cx.subscribe_in(&rename_editor, window, {
            let rename_editor = rename_editor.clone();
            move |_this, _, event, window, cx| {
                if let editor::EditorEvent::Blurred = event {
                    // Defer to let focus settle (avoids canceling during double-click).
                    let rename_editor = rename_editor.clone();
                    cx.defer_in(window, move |this, window, cx| {
                        let still_current = this
                            .rename_editor
                            .as_ref()
                            .is_some_and(|current| current == &rename_editor);
                        if still_current && !rename_editor.focus_handle(cx).is_focused(window) {
                            this.finish_renaming(false, window, cx);
                        }
                    });
                }
            }
        });

        self.rename_editor = Some(rename_editor.clone());
        self.rename_editor_subscription = Some(rename_editor_subscription);

        rename_editor.update(cx, |editor, cx| {
            editor.set_text(current_label, window, cx);
            editor.select_all(&SelectAll, window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
        cx.notify();
    }

    pub fn clear_bell(&mut self, cx: &mut Context<TerminalView>) {
        if std::mem::replace(&mut self.has_bell, false) {
            cx.emit(Event::Wakeup);
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        }
        cx.emit(BellAcknowledged);
    }

    #[cfg(target_os = "windows")]
    fn resumable_cli_menu_actions(&self) -> Vec<(SharedString, Box<dyn gpui::Action>)> {
        if self.resumable_cli_provider().is_none() {
            return Vec::new();
        }
        let resume_label = if self.resume_attempted {
            "Retry AI Session"
        } else {
            "Resume AI Session"
        };
        vec![
            (resume_label.into(), Box::new(ResumeAiSession)),
            ("Start New AI Session".into(), Box::new(StartNewAiSession)),
            ("Forget AI Session".into(), Box::new(ForgetAiSession)),
        ]
    }

    fn should_deploy_context_menu(&self, shift: bool, cx: &App) -> bool {
        self.resumable_cli_provider().is_some()
            || !self
                .terminal
                .read(cx)
                .mouse_mode(shift, self.mouse_input_mode())
    }

    pub fn deploy_context_menu(
        &mut self,
        position: GpuiPoint<Pixels>,
        has_selection: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let assistant_enabled = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).panel::<TerminalPanel>(cx))
            .is_some_and(|terminal_panel| terminal_panel.read(cx).assistant_enabled());
        let context_menu = ContextMenu::build(window, cx, |menu, _, _| {
            menu.context(self.focus_handle.clone())
                .when(self.shows_workspace_actions(), |menu| {
                    menu.action("New Terminal", Box::new(NewTerminal::default()))
                        .when(cfg!(target_os = "windows"), |menu| {
                            menu.action(
                                "New Resumable Codex Terminal",
                                Box::new(NewResumableCodexTerminal),
                            )
                            .action(
                                "New Resumable Claude Terminal",
                                Box::new(NewResumableClaudeTerminal),
                            )
                            .action(
                                "New Resumable Copilot Terminal",
                                Box::new(NewResumableCopilotTerminal),
                            )
                            .action(
                                "New Resumable Hermes Terminal",
                                Box::new(NewResumableHermesTerminal),
                            )
                        })
                        .separator()
                })
                .action("Copy", Box::new(Copy))
                .when(
                    !self.read_only && !matches!(self.mode, TerminalMode::Embedded { .. }),
                    |menu| {
                        menu.action("Paste", Box::new(Paste))
                            .action("Paste Text", Box::new(PasteText))
                    },
                )
                .action("Select All", Box::new(SelectAll))
                .when(
                    !self.read_only && !matches!(self.mode, TerminalMode::Embedded { .. }),
                    |menu| menu.action("Clear", Box::new(Clear)),
                )
                .when(
                    assistant_enabled && !matches!(self.mode, TerminalMode::Embedded { .. }),
                    |menu| {
                        menu.separator()
                            .when(!self.read_only, |menu| {
                                menu.action("Inline Assist", Box::new(InlineAssist::default()))
                            })
                            .when(has_selection && self.shows_workspace_actions(), |menu| {
                                menu.action("Add to Agent Thread", Box::new(AddSelectionToThread))
                            })
                    },
                )
                .when(cfg!(target_os = "windows"), |menu| {
                    #[cfg(target_os = "windows")]
                    {
                        self.resumable_cli_menu_actions()
                            .into_iter()
                            .fold(menu, |menu, (label, action)| menu.action(label, action))
                    }
                    #[cfg(not(target_os = "windows"))]
                    {
                        menu
                    }
                })
                .when(self.shows_workspace_actions(), |menu| {
                    menu.separator().action(
                        "Close Terminal Tab",
                        Box::new(CloseActiveItem {
                            save_intent: None,
                            close_pinned: true,
                        }),
                    )
                })
        });

        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe_in(
            &context_menu,
            window,
            |this, _, _: &DismissEvent, window, cx| {
                if this.context_menu.as_ref().is_some_and(|context_menu| {
                    context_menu.0.focus_handle(cx).contains_focused(window, cx)
                }) {
                    cx.focus_self(window);
                }
                this.context_menu.take();
                cx.notify();
            },
        );

        self.context_menu = Some((context_menu, position, subscription));
    }

    fn settings_changed(&mut self, cx: &mut Context<Self>) {
        let settings = TerminalSettings::get_global(cx);
        let breadcrumb_visibility_changed = self.show_breadcrumbs != settings.toolbar.breadcrumbs;
        self.show_breadcrumbs = settings.toolbar.breadcrumbs;

        let should_blink = match settings.blinking {
            TerminalBlink::Off => false,
            TerminalBlink::On => true,
            TerminalBlink::TerminalControlled => self.blinking_terminal_enabled,
        };
        let new_cursor_shape = settings.cursor_shape;
        let old_cursor_shape = self.cursor_shape;
        if old_cursor_shape != new_cursor_shape {
            self.cursor_shape = new_cursor_shape;
            self.terminal.update(cx, |term, _| {
                term.set_cursor_shape(self.cursor_shape);
            });
        }

        self.blink_manager.update(
            cx,
            if should_blink {
                BlinkManager::enable
            } else {
                BlinkManager::disable
            },
        );

        if breadcrumb_visibility_changed {
            cx.emit(ItemEvent::UpdateBreadcrumbs);
        }
        cx.notify();
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        if self
            .terminal
            .read(cx)
            .last_content
            .mode
            .contains(Modes::ALT_SCREEN)
        {
            self.terminal.update(cx, |term, cx| {
                term.try_keystroke(
                    &Keystroke::parse("ctrl-cmd-space").unwrap(),
                    TerminalSettings::get_global(cx).option_as_meta,
                )
            });
        } else {
            window.show_character_palette();
        }
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |term, _| term.select_all());
        cx.notify();
    }

    fn rerun_task(&mut self, _: &RerunTask, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let task = self
            .terminal
            .read(cx)
            .task()
            .map(|task| terminal_rerun_override(&task.spawned_task.id))
            .unwrap_or_default();
        window.dispatch_action(Box::new(task), cx);
    }

    fn clear(&mut self, _: &Clear, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        self.scroll_top = px(0.);
        self.terminal.update(cx, |term, _| term.clear());
        cx.notify();
    }

    fn max_scroll_top(&self, cx: &App) -> Pixels {
        let terminal = self.terminal.read(cx);

        let Some(block) = self.block_below_cursor.as_ref() else {
            return Pixels::ZERO;
        };

        let line_height = terminal.last_content().terminal_bounds.line_height;
        let viewport_lines = terminal.viewport_lines();
        let cursor_line = viewport_line_for_point(
            terminal.last_content.cursor.point,
            terminal.last_content.display_offset,
        )
        .unwrap_or_default();
        let max_scroll_top_in_lines =
            (block.height as usize).saturating_sub(viewport_lines.saturating_sub(cursor_line + 1));

        max_scroll_top_in_lines as f32 * line_height
    }

    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let terminal_content = self.terminal.read(cx).last_content();

        if self.block_below_cursor.is_some() && terminal_content.display_offset == 0 {
            let line_height = terminal_content.terminal_bounds.line_height;
            let y_delta = event.delta.pixel_delta(line_height).y;
            if y_delta < Pixels::ZERO || self.scroll_top > Pixels::ZERO {
                self.scroll_top = cmp::max(
                    Pixels::ZERO,
                    cmp::min(self.scroll_top - y_delta, self.max_scroll_top(cx)),
                );
                cx.notify();
                return;
            }
        }
        self.terminal.update(cx, |term, cx| {
            term.scroll_wheel(
                event,
                TerminalSettings::get_global(cx).scroll_multiplier.max(0.01),
                self.mouse_input_mode(),
            )
        });
    }

    fn is_alt_screen(&self, cx: &App) -> bool {
        self.terminal
            .read(cx)
            .last_content
            .mode
            .contains(Modes::ALT_SCREEN)
    }

    fn scroll_line_up(&mut self, _: &ScrollLineUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        let terminal_content = self.terminal.read(cx).last_content();
        if self.block_below_cursor.is_some()
            && terminal_content.display_offset == 0
            && self.scroll_top > Pixels::ZERO
        {
            let line_height = terminal_content.terminal_bounds.line_height;
            self.scroll_top = cmp::max(self.scroll_top - line_height, Pixels::ZERO);
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_line_up());
        cx.notify();
    }

    fn scroll_line_down(&mut self, _: &ScrollLineDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        let terminal_content = self.terminal.read(cx).last_content();
        if self.block_below_cursor.is_some() && terminal_content.display_offset == 0 {
            let max_scroll_top = self.max_scroll_top(cx);
            if self.scroll_top < max_scroll_top {
                let line_height = terminal_content.terminal_bounds.line_height;
                self.scroll_top = cmp::min(self.scroll_top + line_height, max_scroll_top);
            }
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_line_down());
        cx.notify();
    }

    fn scroll_page_up(&mut self, _: &ScrollPageUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        if self.scroll_top == Pixels::ZERO {
            self.terminal.update(cx, |term, _| term.scroll_page_up());
        } else {
            let line_height = self
                .terminal
                .read(cx)
                .last_content
                .terminal_bounds
                .line_height();
            let visible_block_lines = (self.scroll_top / line_height) as usize;
            let viewport_lines = self.terminal.read(cx).viewport_lines();
            let visible_content_lines = viewport_lines - visible_block_lines;

            if visible_block_lines >= viewport_lines {
                self.scroll_top = ((visible_block_lines - viewport_lines) as f32) * line_height;
            } else {
                self.scroll_top = px(0.);
                self.terminal
                    .update(cx, |term, _| term.scroll_up_by(visible_content_lines));
            }
        }
        cx.notify();
    }

    fn scroll_page_down(&mut self, _: &ScrollPageDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_page_down());
        let terminal = self.terminal.read(cx);
        if terminal.last_content().display_offset < terminal.viewport_lines() {
            self.scroll_top = self.max_scroll_top(cx);
        }
        cx.notify();
    }

    fn scroll_to_top(&mut self, _: &ScrollToTop, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_to_top());
        cx.notify();
    }

    fn scroll_to_bottom(&mut self, _: &ScrollToBottom, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_to_bottom());
        if self.block_below_cursor.is_some() {
            self.scroll_top = self.max_scroll_top(cx);
        }
        cx.notify();
    }

    fn toggle_vi_mode(&mut self, _: &ToggleViMode, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |term, _| term.toggle_vi_mode());
        cx.notify();
    }

    pub fn should_show_cursor(&self, focused: bool, cx: &mut Context<Self>) -> bool {
        // Hide cursor when in embedded mode and not focused (read-only output like Agent panel)
        if let TerminalMode::Embedded { .. } = &self.mode {
            if !focused {
                return false;
            }
        }

        // For Standalone mode: always show cursor when not focused or in special modes
        if !focused
            || self
                .terminal
                .read(cx)
                .last_content
                .mode
                .contains(Modes::ALT_SCREEN)
        {
            return true;
        }

        // When focused, check blinking settings and blink manager state
        match TerminalSettings::get_global(cx).blinking {
            TerminalBlink::Off => true,
            TerminalBlink::TerminalControlled => {
                !self.blinking_terminal_enabled || self.blink_manager.read(cx).visible()
            }
            TerminalBlink::On => self.blink_manager.read(cx).visible(),
        }
    }

    pub fn pause_cursor_blinking(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.blink_manager.update(cx, BlinkManager::pause_blinking);
    }

    pub fn terminal(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    pub fn set_block_below_cursor(
        &mut self,
        block: BlockProperties,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.block_below_cursor = Some(Rc::new(block));
        self.scroll_to_bottom(&ScrollToBottom, window, cx);
        cx.notify();
    }

    pub fn clear_block_below_cursor(&mut self, cx: &mut Context<Self>) {
        self.block_below_cursor = None;
        self.scroll_top = Pixels::ZERO;
        cx.notify();
    }

    ///Attempt to paste the clipboard into the terminal
    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |term, _| term.copy(None));
        cx.notify();
    }

    /// Specific handler for the [`editor::actions::Copy`] action in order for
    /// the `Edit > Copy` menu item to not be disabled, as the app expects a
    /// handler for this action in order to enable/disable the menu item.
    fn editor_copy(
        &mut self,
        _: &editor::actions::Copy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy(&Copy, window, cx);
    }

    ///Attempt to paste the clipboard into the terminal
    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };

        match clipboard.entries().first() {
            Some(ClipboardEntry::Image(image)) if !image.bytes.is_empty() => {
                self.forward_ctrl_v(cx);
            }
            Some(ClipboardEntry::ExternalPaths(paths)) => {
                self.add_paths_to_terminal(paths.paths(), window, cx);
            }
            _ => {
                if let Some(text) = clipboard.text() {
                    self.terminal
                        .update(cx, |terminal, _cx| terminal.paste(&text));
                }
            }
        }
    }

    /// Specific handler for the [`editor::actions::Paste`] action in order for
    /// the `Edit > Paste` menu item to not be disabled, as the app expects a
    /// handler for this action in order to enable/disable the menu item.
    fn editor_paste(
        &mut self,
        _: &editor::actions::Paste,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.paste(&Paste, window, cx);
    }

    ///Attempt to paste the clipboard text into the terminal
    fn paste_text(&mut self, _: &PasteText, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };

        if let Some(text) = clipboard.text() {
            self.terminal
                .update(cx, |terminal, _cx| terminal.paste(&text));
        }
    }

    /// Emits a raw Ctrl+V so TUI agents can read the OS clipboard directly
    /// and attach images using their native workflows.
    fn forward_ctrl_v(&self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |term, _| {
            term.input(vec![0x16]);
        });
    }

    pub fn add_paths_to_terminal(&self, paths: &[PathBuf], window: &mut Window, cx: &mut App) {
        if self.read_only {
            return;
        }
        let mut text = paths
            .iter()
            .filter_map(|path| Some(format!(" {}", shlex::try_quote(path.to_str()?).ok()?)))
            .collect::<String>();
        text.push(' ');
        window.focus(&self.focus_handle(cx), cx);
        self.terminal.update(cx, |terminal, _| {
            terminal.paste(&text);
        });
    }

    fn send_text(&mut self, text: &SendText, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        self.clear_bell(cx);
        self.blink_manager.update(cx, BlinkManager::pause_blinking);
        self.terminal.update(cx, |term, _| {
            term.input(text.0.to_string().into_bytes());
        });
    }

    fn send_keystroke(&mut self, text: &SendKeystroke, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(keystroke) = Keystroke::parse(&text.0).log_err() {
            self.clear_bell(cx);
            self.blink_manager.update(cx, BlinkManager::pause_blinking);
            self.process_keystroke(&keystroke, cx);
        }
    }

    fn dispatch_context(&self, cx: &App) -> KeyContext {
        let mut dispatch_context = KeyContext::new_with_defaults();
        dispatch_context.add("Terminal");

        if self.terminal.read(cx).vi_mode_enabled() {
            dispatch_context.add("vi_mode");
        }

        let mode = self.terminal.read(cx).last_content.mode;
        dispatch_context.set(
            "screen",
            if mode.contains(Modes::ALT_SCREEN) {
                "alt"
            } else {
                "normal"
            },
        );

        if mode.contains(Modes::APP_CURSOR) {
            dispatch_context.add("DECCKM");
        }
        if mode.contains(Modes::APP_KEYPAD) {
            dispatch_context.add("DECPAM");
        } else {
            dispatch_context.add("DECPNM");
        }
        if mode.contains(Modes::SHOW_CURSOR) {
            dispatch_context.add("DECTCEM");
        }
        if mode.contains(Modes::LINE_WRAP) {
            dispatch_context.add("DECAWM");
        }
        if mode.contains(Modes::ORIGIN) {
            dispatch_context.add("DECOM");
        }
        if mode.contains(Modes::INSERT) {
            dispatch_context.add("IRM");
        }
        //LNM is apparently the name for this. https://vt100.net/docs/vt510-rm/LNM.html
        if mode.contains(Modes::LINE_FEED_NEW_LINE) {
            dispatch_context.add("LNM");
        }
        if mode.contains(Modes::FOCUS_IN_OUT) {
            dispatch_context.add("report_focus");
        }
        if mode.contains(Modes::ALTERNATE_SCROLL) {
            dispatch_context.add("alternate_scroll");
        }
        if mode.contains(Modes::BRACKETED_PASTE) {
            dispatch_context.add("bracketed_paste");
        }
        if mode.intersects(Modes::MOUSE_MODE) {
            dispatch_context.add("any_mouse_reporting");
        }
        {
            let mouse_reporting = if mode.contains(Modes::MOUSE_REPORT_CLICK) {
                "click"
            } else if mode.contains(Modes::MOUSE_DRAG) {
                "drag"
            } else if mode.contains(Modes::MOUSE_MOTION) {
                "motion"
            } else {
                "off"
            };
            dispatch_context.set("mouse_reporting", mouse_reporting);
        }
        {
            let format = if mode.contains(Modes::SGR_MOUSE) {
                "sgr"
            } else if mode.contains(Modes::UTF8_MOUSE) {
                "utf8"
            } else {
                "normal"
            };
            dispatch_context.set("mouse_format", format);
        };

        if self.terminal.read(cx).last_content.selection.is_some() {
            dispatch_context.add("selection");
        }

        dispatch_context
    }

    fn set_terminal(
        &mut self,
        terminal: Entity<Terminal>,
        window: &mut Window,
        cx: &mut Context<TerminalView>,
    ) {
        self._terminal_subscriptions =
            subscribe_for_terminal_events(&terminal, self.workspace.clone(), window, cx);
        self.terminal = terminal;
    }

    fn rerun_button(&self, task: &TaskState) -> Option<IconButton> {
        if self.read_only || !task.spawned_task.show_rerun {
            return None;
        }

        let task_id = task.spawned_task.id.clone();
        Some(
            IconButton::new("rerun-icon", IconName::Rerun)
                .icon_size(IconSize::Small)
                .size(ButtonSize::Compact)
                .icon_color(Color::Default)
                .shape(ui::IconButtonShape::Square)
                .tooltip(move |_window, cx| Tooltip::for_action("Rerun task", &RerunTask, cx))
                .on_click(move |_, window, cx| {
                    window.dispatch_action(Box::new(terminal_rerun_override(&task_id)), cx);
                }),
        )
    }
}

fn terminal_rerun_override(task: &TaskId) -> zed_actions::Rerun {
    zed_actions::Rerun {
        task_id: Some(task.0.clone()),
        allow_concurrent_runs: Some(true),
        use_new_terminal: Some(false),
        reevaluate_context: false,
    }
}

fn subscribe_for_terminal_events(
    terminal: &Entity<Terminal>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut Context<TerminalView>,
) -> Vec<Subscription> {
    let terminal_subscription = cx.observe(terminal, |_, _, cx| cx.notify());
    let mut previous_cwd = None;
    let terminal_events_subscription = cx.subscribe_in(
        terminal,
        window,
        move |terminal_view, terminal, event, window, cx| {
            let current_cwd = terminal.read(cx).working_directory();
            if current_cwd != previous_cwd {
                previous_cwd = current_cwd;
                terminal_view.needs_serialize = true;
                cx.emit(ItemEvent::UpdateTab);
                terminal_view.persistence_generation =
                    terminal_view.persistence_generation.wrapping_add(1);
            }

            match event {
                Event::Wakeup => {
                    cx.notify();
                    window.invalidate_character_coordinates();
                    cx.emit(Event::Wakeup);
                    cx.emit(ItemEvent::UpdateTab);
                    cx.emit(SearchEvent::MatchesInvalidated);
                }

                Event::OutputReplaced => {
                    terminal_view.hover = None;
                    terminal_view.hover_tooltip_update = Task::ready(());
                    terminal_view.scroll_top = Pixels::ZERO;
                    terminal_view.scroll_handle.future_display_offset.set(None);
                    terminal_view.scroll_handle.update(terminal.read(cx));
                    terminal_view.block_below_cursor = None;
                    window.invalidate_character_coordinates();
                    cx.emit(SearchEvent::MatchesInvalidated);
                    cx.emit(SearchEvent::ActiveMatchChanged);
                    cx.emit(ItemEvent::UpdateTab);
                    cx.notify();
                }

                Event::Bell => {
                    let has_bell = !(window.is_window_active()
                        && terminal_view.focus_handle.is_focused(window));
                    if terminal_view.has_bell != has_bell {
                        terminal_view.has_bell = has_bell;
                        cx.emit(ItemEvent::UpdateTab);
                        cx.notify();
                    }
                    if !has_bell {
                        cx.emit(BellAcknowledged);
                    }
                    if let TerminalBell::System = TerminalSettings::get_global(cx).bell {
                        window.play_system_bell();
                    }
                    cx.emit(Event::Wakeup);
                }

                Event::BlinkChanged(blinking) => {
                    terminal_view.blinking_terminal_enabled = *blinking;

                    // If in terminal-controlled mode and focused, update blink manager
                    if matches!(
                        TerminalSettings::get_global(cx).blinking,
                        TerminalBlink::TerminalControlled
                    ) && terminal_view.focus_handle.is_focused(window)
                    {
                        terminal_view.blink_manager.update(cx, |manager, cx| {
                            if *blinking {
                                manager.enable(cx);
                            } else {
                                manager.disable(cx);
                            }
                        });
                    }
                }

                Event::TitleChanged => {
                    #[cfg(target_os = "windows")]
                    terminal_view.update_session_claim_for_foreground_process(
                        terminal
                            .read(cx)
                            .foreground_process_command_name()
                            .as_deref(),
                    );
                    cx.emit(ItemEvent::UpdateTab);
                }

                Event::NewNavigationTarget(maybe_navigation_target) => {
                    match maybe_navigation_target
                        .as_ref()
                        .zip(terminal.read(cx).last_content.last_hovered_word.as_ref())
                    {
                        Some((MaybeNavigationTarget::Url(url), hovered_word)) => {
                            if Some(hovered_word)
                                != terminal_view
                                    .hover
                                    .as_ref()
                                    .map(|hover| &hover.hovered_word)
                            {
                                terminal_view.hover = Some(HoverTarget {
                                    tooltip: url.clone(),
                                    hovered_word: hovered_word.clone(),
                                });
                                terminal_view.hover_tooltip_update = Task::ready(());
                                cx.notify();
                            }
                        }
                        Some((MaybeNavigationTarget::PathLike(path_like_target), hovered_word)) => {
                            if Some(hovered_word)
                                != terminal_view
                                    .hover
                                    .as_ref()
                                    .map(|hover| &hover.hovered_word)
                            {
                                terminal_view.hover = None;
                                terminal_view.hover_tooltip_update = hover_path_like_target(
                                    &workspace,
                                    hovered_word.clone(),
                                    path_like_target,
                                    cx,
                                );
                                cx.notify();
                            }
                        }
                        None => {
                            terminal_view.hover = None;
                            terminal_view.hover_tooltip_update = Task::ready(());
                            cx.notify();
                        }
                    }
                }

                Event::Open(maybe_navigation_target) => match maybe_navigation_target {
                    MaybeNavigationTarget::Url(url) => cx.open_url(url),
                    MaybeNavigationTarget::PathLike(path_like_target) => open_path_like_target(
                        &workspace,
                        terminal_view,
                        path_like_target,
                        window,
                        cx,
                    ),
                },
                Event::BreadcrumbsChanged => cx.emit(ItemEvent::UpdateBreadcrumbs),
                Event::CloseTerminal => cx.emit(ItemEvent::CloseItem),
                Event::SelectionsChanged => {
                    window.invalidate_character_coordinates();
                    cx.emit(SearchEvent::ActiveMatchChanged)
                }
            }
        },
    );
    vec![terminal_subscription, terminal_events_subscription]
}

fn regex_search_for_query(query: &SearchQuery) -> Option<Search> {
    let str = query.as_str();
    if query.is_regex() {
        if str == "." {
            return None;
        }
        Search::new(str)
    } else {
        Search::new(&regex::escape(str))
    }
}

#[derive(Default)]
struct TerminalScrollbarSettingsWrapper;

impl ScrollbarVisibility for TerminalScrollbarSettingsWrapper {
    fn visibility(&self, cx: &App) -> scrollbars::ShowScrollbar {
        TerminalSettings::get_global(cx)
            .scrollbar
            .show
            .map(ui_scrollbar_settings_from_raw)
            .unwrap_or_else(|| EditorSettings::get_global(cx).scrollbar.show)
    }
}

impl TerminalView {
    /// Attempts to process a keystroke in the terminal. Returns true if handled.
    ///
    /// In vi mode, explicitly triggers a re-render because vi navigation (like j/k)
    /// updates the cursor locally without sending data to the shell, so there's no
    /// shell output to automatically trigger a re-render.
    fn process_keystroke(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) -> bool {
        if self.read_only && !self.terminal.read(cx).vi_mode_enabled() {
            return false;
        }
        let (handled, vi_mode_enabled) = self.terminal.update(cx, |term, cx| {
            (
                term.try_keystroke(keystroke, TerminalSettings::get_global(cx).option_as_meta),
                term.vi_mode_enabled(),
            )
        });

        if handled && vi_mode_enabled {
            cx.notify();
        }

        handled
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_bell(cx);
        self.pause_cursor_blinking(window, cx);

        if event.prefer_character_input
            && event.keystroke.key_char.is_some()
            && !self.terminal.read(cx).vi_mode_enabled()
        {
            return;
        }

        if self.process_keystroke(&event.keystroke, cx) {
            cx.stop_propagation();
        }
    }

    fn focus_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.is_window_active() && self.focus_handle.is_focused(window) {
            self.clear_bell(cx);
        }
        self.terminal.update(cx, |terminal, _| {
            terminal.set_cursor_shape(self.cursor_shape);
            if !self.read_only {
                terminal.focus_in();
            }
        });

        let should_blink = match TerminalSettings::get_global(cx).blinking {
            TerminalBlink::Off => false,
            TerminalBlink::On => true,
            TerminalBlink::TerminalControlled => self.blinking_terminal_enabled,
        };

        if should_blink {
            self.blink_manager.update(cx, BlinkManager::enable);
        }

        #[cfg(target_os = "windows")]
        if self.resume_on_first_focus
            && self
                .workspace
                .upgrade()
                .is_some_and(|workspace| !workspace.read(cx).is_restoring())
        {
            self.resume_on_first_focus = false;
            self.request_ai_session_resume(false, window, cx);
        }

        window.invalidate_character_coordinates();
        cx.notify();
    }

    #[cfg(target_os = "windows")]
    fn resume_ai_session(
        &mut self,
        _: &ResumeAiSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_ai_session_resume(true, window, cx);
    }

    #[cfg(target_os = "windows")]
    fn request_ai_session_resume(
        &mut self,
        allow_duplicate_confirmation: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.resumable_cli_binding.session.clone() else {
            if let Some(cwd) = self.resumable_cli_binding.pending_codex_cwd.clone() {
                self.start_codex_terminal(None, cwd, None, window, cx);
            }
            return;
        };
        let Some(claim) = resumable_cli::try_claim_target(&session) else {
            if !allow_duplicate_confirmation {
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        workspace.show_error(
                            anyhow::anyhow!(
                                "This AI session is already active in another terminal. Use Resume AI Session to confirm a second attempt."
                            ),
                            cx,
                        );
                    });
                }
                return;
            }

            let confirmation = window.prompt(
                gpui::PromptLevel::Warning,
                "This AI session is already active in another terminal.",
                Some("Starting a second process for the same native conversation may cause conflicting changes."),
                &["Resume Anyway", "Cancel"],
                cx,
            );
            cx.spawn_in(window, async move |this, cx| {
                if confirmation.await == Ok(0) {
                    this.update_in(cx, |this, window, cx| {
                        let claim = resumable_cli::replace_target_claim(&session);
                        this.start_ai_session_resume(session, claim, window, cx);
                    })?;
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
            return;
        };

        self.start_ai_session_resume(session, claim, window, cx);
    }

    #[cfg(target_os = "windows")]
    fn start_ai_session_resume(
        &mut self,
        session: ResumableCliSession,
        claim: resumable_cli::SessionClaim,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let provider = session.provider();
        if provider == ResumableCliProvider::Codex {
            let cwd = session.resume_cwd().to_path_buf();
            self.start_codex_terminal(Some(session), cwd, Some(claim), _window, cx);
            return;
        }
        if provider == ResumableCliProvider::Claude {
            self.start_claude_terminal(session, claim, _window, cx);
            return;
        }
        let contract = cx.background_executor().spawn(async move {
            match provider {
                ResumableCliProvider::Codex => resumable_cli::verify_codex_contract(),
                ResumableCliProvider::Claude => resumable_cli::verify_claude_contract(),
                ResumableCliProvider::Copilot => resumable_cli::verify_copilot_contract(),
                ResumableCliProvider::Hermes => resumable_cli::verify_hermes_contract(),
            }
        });
        self.start_ai_session_resume_with_contract(session, claim, contract, cx);
    }

    #[cfg(target_os = "windows")]
    fn start_claude_terminal(
        &mut self,
        session: ResumableCliSession,
        claim: resumable_cli::SessionClaim,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project = self.project.clone();
        let terminal_id = self.terminal.entity_id();
        let expected_revision = self.resumable_cli_binding.revision;
        let (generation, cancelled) = self.begin_resume_attempt();
        cx.spawn_in(window, async move |this, cx| {
            let preparation = async {
                cx.background_executor()
                    .spawn(async { resumable_cli::verify_claude_contract() })
                    .await?;
                let cwd = session.resume_cwd().to_path_buf();
                anyhow::ensure!(
                    cwd.is_dir(),
                    "The saved Claude working directory is no longer accessible"
                );
                let listener = resumable_cli::start_claude_capture(cwd.clone())?;
                let command = resumable_cli::claude_resume_command_with_plugin(
                    &session,
                    &listener.plugin_path,
                )?;
                let terminal = project
                    .update(cx, |project, cx| {
                        project.create_terminal_shell_with_env(
                            Some(cwd),
                            listener.observer.environment.clone(),
                            cx,
                        )
                    })?
                    .await?;
                anyhow::Ok((terminal, listener.observer, command))
            };
            let ready = futures::select_biased! {
                _ = cancelled.fuse() => return anyhow::Ok(()),
                ready = preparation.fuse() => ready,
            };
            this.update_in(cx, |view, window, cx| {
                if !view.resume_attempt_is_current(
                    generation,
                    terminal_id,
                    expected_revision,
                    &session,
                ) {
                    return;
                }
                view.complete_resume_attempt(generation);
                match ready {
                    Ok((terminal, observer, command)) => {
                        view.replace_terminal(terminal, window, cx);
                        view.session_claim = Some(claim);
                        view.observe_cli_identity(observer, cx);
                        view.resume_attempted = true;
                        view.write_ai_command_after_startup(command, cx);
                    }
                    Err(error) => view.show_resumable_cli_error(error, cx),
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    #[cfg(target_os = "windows")]
    fn start_codex_terminal(
        &mut self,
        session: Option<ResumableCliSession>,
        cwd: PathBuf,
        claim: Option<resumable_cli::SessionClaim>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let fs = workspace.read(cx).app_state().fs.clone();
        let project = self.project.clone();
        let launch_cwd = cwd.clone();
        let preparation = cx.spawn_in(window, async move |_, cx| {
            let path = resumable_cli::codex_hooks_path()?;
            let existing = if fs.is_file(&path).await {
                Some(fs.load(&path).await?)
            } else {
                None
            };
            let merge = resumable_cli::merge_codex_hook(existing.as_deref())?;
            if merge.changed {
                let detail = format!(
                    "Zed will preserve unrelated hooks and update its identity hooks in {}. Codex may separately require you to trust the changed hooks:\n\n{}",
                    path.display(), merge.entry_preview,
                );
                let answer = cx.update(|window, cx| {
                    window.prompt(
                        gpui::PromptLevel::Info,
                        "Update Codex identity hooks?",
                        Some(&detail),
                        &["Review and Update", "Cancel"],
                        cx,
                    )
                })?.await;
                if answer != Ok(0) {
                    return Ok(None);
                }
                fs.atomic_write(path, merge.content).await?;
            }
            cx.background_executor()
                .spawn(async { resumable_cli::verify_codex_contract() })
                .await?;
            anyhow::ensure!(
                fs.is_dir(&launch_cwd).await,
                "The saved Codex working directory is no longer accessible"
            );
            let observer = resumable_cli::start_codex_identity_observer(launch_cwd.clone())?;
            let terminal = project.update(cx, |project, cx| {
                project.create_terminal_shell_with_env(
                    Some(launch_cwd), observer.environment.clone(), cx,
                )
            })?.await?;
            anyhow::Ok(Some((terminal, observer)))
        });
        self.start_codex_terminal_with_preparation(session, cwd, claim, preparation, window, cx);
    }

    #[cfg(target_os = "windows")]
    fn start_codex_terminal_with_preparation(
        &mut self,
        session: Option<ResumableCliSession>,
        cwd: PathBuf,
        claim: Option<resumable_cli::SessionClaim>,
        preparation: Task<
            anyhow::Result<Option<(Entity<Terminal>, resumable_cli::CliIdentityObserver)>>,
        >,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous_terminal = self.terminal.entity_id();
        let (generation, cancelled) = self.begin_resume_attempt();
        let expected_revision = self.resumable_cli_binding.revision;
        cx.spawn_in(window, async move |this, cx| {
            let mut launch_generation = generation;
            let mut launch_revision = expected_revision;
            let outcome: anyhow::Result<()> = async {
                let ready = futures::select_biased! {
                    _ = cancelled.fuse() => return Ok(()),
                    ready = preparation.fuse() => ready?,
                };
                let Some((terminal, observer)) = ready else {
                    return Ok(());
                };
                let current = this
                    .read_with(cx, |view, _| {
                        view.resume_attempt_generation == generation
                            && view.terminal.entity_id() == previous_terminal
                            && view.resumable_cli_binding.revision == expected_revision
                    })
                    .unwrap_or(false);
                if !current {
                    return Ok(());
                }
                let command = match session.as_ref() {
                    Some(session) => resumable_cli::codex_resume_command(session)?,
                    None => resumable_cli::codex_new_command(),
                };
                if session.is_none() {
                    let (write, next_generation) = this.update(cx, |view, cx| {
                        let write =
                            view.replace_resumable_cli_association(None, Some(cwd.clone()), cx);
                        (write, view.resume_attempt_generation)
                    })?;
                    launch_generation = next_generation;
                    write.await?;
                    launch_revision = expected_revision + 1;
                }
                this.update_in(cx, |view, window, cx| {
                    if view.resume_attempt_generation != launch_generation
                        || view.terminal.entity_id() != previous_terminal
                        || view.resumable_cli_binding.revision != launch_revision
                        || view.resumable_cli_binding.session != session
                        || (session.is_none()
                            && view.resumable_cli_binding.pending_codex_cwd.as_ref() != Some(&cwd))
                    {
                        return;
                    }
                    view.replace_terminal(terminal, window, cx);
                    view.session_claim = claim;
                    view.observe_cli_identity(observer, cx);
                    view.resume_attempted = true;
                    view.write_ai_command_after_startup(command, cx);
                })?;
                Ok(())
            }
            .await;
            this.update(cx, |view, cx| {
                if view.resume_attempt_generation == launch_generation {
                    view.complete_resume_attempt(launch_generation);
                    if let Err(error) = outcome {
                        view.show_resumable_cli_error(error, cx);
                    }
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    #[cfg(target_os = "windows")]
    fn start_ai_session_resume_with_contract(
        &mut self,
        session: ResumableCliSession,
        claim: resumable_cli::SessionClaim,
        contract: Task<anyhow::Result<()>>,
        cx: &mut Context<Self>,
    ) {
        // A new request supersedes any in-flight resume even when preparing its command fails.
        let (attempt_generation, cancellation) = self.begin_resume_attempt();
        let command = match session.provider() {
            ResumableCliProvider::Codex => resumable_cli::codex_resume_command(&session),
            ResumableCliProvider::Claude => resumable_cli::claude_resume_command(&session),
            ResumableCliProvider::Copilot => resumable_cli::copilot_resume_command(&session),
            ResumableCliProvider::Hermes => resumable_cli::hermes_resume_command(&session),
        };
        let command = match command {
            Ok(command) => command,
            Err(error) => {
                self.complete_resume_attempt(attempt_generation);
                self.show_resumable_cli_error(error, cx);
                return;
            }
        };
        let terminal_id = self.terminal.entity_id();
        let terminal = self.terminal.downgrade();
        let expected_revision = self.resumable_cli_binding.revision;
        let use_startup_handshake = !self.resume_attempted;
        self.resume_attempted = true;

        cx.spawn(async move |this, cx| {
            let attempt_is_current = |this: &TerminalView| {
                this.resume_attempt_is_current(
                    attempt_generation,
                    terminal_id,
                    expected_revision,
                    &session,
                )
            };
            let contract = futures::select_biased! {
                _ = cancellation.fuse() => return anyhow::Ok(()),
                contract = contract.fuse() => contract,
            };
            if let Err(error) = contract {
                if this.read_with(cx, |this, _| attempt_is_current(this)).unwrap_or(false) {
                    this.update(cx, |this, cx| {
                        this.complete_resume_attempt(attempt_generation);
                        this.show_resumable_cli_error(error, cx);
                    })?;
                }
                return anyhow::Ok(());
            }

            if !this
                .read_with(cx, |this, _| attempt_is_current(this))
                .unwrap_or(false)
            {
                return anyhow::Ok(());
            }

            if use_startup_handshake {
                let Ok(startup) = terminal.update(cx, |terminal, _| {
                    terminal.start_init_command_startup_handshake()
                }) else {
                    return anyhow::Ok(());
                };
                let timeout = cx.background_executor().timer(Duration::from_secs(5));
                futures::select_biased! {
                    _ = startup.fuse() => {}
                    _ = timeout.fuse() => {}
                }
                if !this
                    .read_with(cx, |this, _| attempt_is_current(this))
                    .unwrap_or(false)
                {
                    return anyhow::Ok(());
                }
                let Ok(delivered) = terminal.update(cx, |terminal, cx| {
                    terminal.write_init_command_after_startup(command, cx)
                }) else {
                    return anyhow::Ok(());
                };
                if !delivered {
                    if this.read_with(cx, |this, _| attempt_is_current(this)).unwrap_or(false) {
                        this.update(cx, |this, cx| {
                            this.complete_resume_attempt(attempt_generation);
                            this.show_resumable_cli_error(
                                anyhow::anyhow!("The terminal was used before the AI session could resume. Retry from the terminal tab menu."),
                                cx,
                            )
                        })?;
                    }
                    return anyhow::Ok(());
                }
            } else {
                if terminal
                    .update(cx, |terminal, _| terminal.input(command))
                    .is_err()
                {
                    return anyhow::Ok(());
                }
            }
            this.update(cx, |this, _| {
                if attempt_is_current(this) {
                    this.complete_resume_attempt(attempt_generation);
                    this.session_claim = Some(claim);
                    this.claimed_process_seen = false;
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    #[cfg(target_os = "windows")]
    fn forget_ai_session(
        &mut self,
        _: &ForgetAiSession,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.forget_resumable_cli_session(cx).detach_and_log_err(cx);
    }

    #[cfg(target_os = "windows")]
    fn start_new_ai_session(
        &mut self,
        _: &StartNewAiSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(previous_session) = self.resumable_cli_binding.session.clone() else {
            if let Some(cwd) = self.resumable_cli_binding.pending_codex_cwd.clone() {
                self.start_codex_terminal(None, cwd, None, window, cx);
            }
            return;
        };
        let provider = previous_session.provider();
        let confirmation = window.prompt(
            gpui::PromptLevel::Warning,
            &format!("Start a new {} session?", provider.display_name()),
            Some(if provider == ResumableCliProvider::Codex {
                "This replaces the saved conversation with a new Codex launch. The new conversation will be saved when Codex reports its identity."
            } else {
                "The current association will be retained unless the provider reports a new exact session identity."
            }),
            &["Start New", "Cancel"],
            cx,
        );
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let fs = workspace.read(cx).app_state().fs.clone();
        let project = self.project.clone();

        cx.spawn_in(window, async move |this, cx| {
            if confirmation.await != Ok(0) {
                return anyhow::Ok(());
            }

            let outcome: anyhow::Result<()> = async {
                let working_directory = previous_session.resume_cwd().to_path_buf();
                if provider == ResumableCliProvider::Codex {
                    this.update_in(cx, |view, window, cx| view.start_codex_terminal(None,
                        working_directory, None, window, cx))?;
                    return Ok(());
                }
                anyhow::ensure!(
                    fs.is_dir(&working_directory).await,
                    "The saved AI session working directory is no longer accessible"
                );

                if provider == ResumableCliProvider::Hermes {
                    let hermes_profile = resumable_cli::active_hermes_profile()?;
                    let config_path = resumable_cli::hermes_config_path(&hermes_profile);
                    let existing = if fs.is_file(&config_path).await {
                        Some(fs.load(&config_path).await?)
                    } else {
                        None
                    };
                    let hook_merge = resumable_cli::merge_hermes_hook(existing.as_deref())?;
                    if hook_merge.changed {
                        let detail = format!(
                            "Zed will add this entry to {} and preserve unrelated configuration. Hermes will separately ask you to approve this exact command on first use:\n\n{}",
                            config_path.display(),
                            hook_merge.entry_preview
                        );
                        let answer = cx
                            .update(|window, cx| {
                                window.prompt(
                                    gpui::PromptLevel::Info,
                                    "Allow resumable Hermes terminals?",
                                    Some(&detail),
                                    &[
                                        "Review and Add Globally",
                                        "Manual Instructions",
                                        "Cancel",
                                    ],
                                    cx,
                                )
                            })?
                            .await;
                        match answer {
                            Ok(0) => {
                                if let Some(parent) = config_path.parent() {
                                    fs.create_dir(parent).await?;
                                }
                                fs.atomic_write(config_path, hook_merge.content).await?;
                            }
                            Ok(1) => {
                                let manual = format!(
                                    "Add the reviewed entry to {} and run Start New again:\n\n{}",
                                    config_path.display(),
                                    hook_merge.entry_preview
                                );
                                cx.update(|window, cx| {
                                    window.prompt(
                                        gpui::PromptLevel::Info,
                                        "Manual Hermes hook installation",
                                        Some(&manual),
                                        &["OK"],
                                        cx,
                                    )
                                })?
                                .await
                                .ok();
                                return Ok(());
                            }
                            _ => return Ok(()),
                        }
                    }

                    cx.background_executor()
                        .spawn(async { resumable_cli::verify_hermes_contract() })
                        .await?;
                    let listener = resumable_cli::start_hermes_capture(
                        working_directory.clone(),
                        hermes_profile.clone(),
                    )?;
                    let terminal = project
                        .update(cx, |project, cx| {
                            project.create_terminal_shell_with_env(
                                Some(working_directory),
                                listener.environment,
                                cx,
                            )
                        })?
                        .await?;
                    let capture_cancellation = listener.cancellation.clone();
                    this.update_in(cx, |this, window, cx| {
                        this.replace_terminal(terminal, window, cx);
                        this.begin_capture_attempt(capture_cancellation.clone());
                        this.write_ai_command_after_startup(
                            resumable_cli::hermes_new_command(&hermes_profile),
                            cx,
                        );
                    })?;

                    let capture = listener.result.await;
                    if capture_cancellation.is_cancelled() {
                        return Ok(());
                    }
                    let capture = capture.context("Hermes identity callback ended before replacement capture")??;
                    anyhow::ensure!(
                        this.update(cx, |this, _| this
                            .complete_capture_attempt(&capture_cancellation))?,
                        "Hermes identity arrived after the launch attempt was replaced"
                    );
                    let claim = resumable_cli::try_claim_target(&capture.session).context(
                        "Hermes reported an AI session already active in another terminal",
                    )?;
                    this.update(cx, |this, cx| {
                            this.set_resumable_cli_session_with_claim(
                                capture.session,
                                claim,
                                cx,
                            )
                        })?
                        .await?;
                    return Ok(());
                }

                if provider == ResumableCliProvider::Copilot {
                    cx.background_executor()
                        .spawn(async { resumable_cli::verify_copilot_contract() })
                        .await?;
                    let listener = resumable_cli::start_copilot_capture(working_directory.clone())?;
                    let command = resumable_cli::copilot_new_command(&listener.session_id)?;
                    let terminal = project
                        .update(cx, |project, cx| {
                            project.create_terminal_shell_with_env(
                                Some(working_directory),
                                listener.environment.clone(),
                                cx,
                            )
                        })?
                        .await?;
                    let capture_cancellation = listener.cancellation.clone();
                    this.update_in(cx, |this, window, cx| {
                        this.replace_terminal(terminal, window, cx);
                        this.begin_capture_attempt(capture_cancellation.clone());
                        this.write_ai_command_after_startup(command, cx);
                    })?;

                    let capture = listener.result.await;
                    if capture_cancellation.is_cancelled() {
                        return Ok(());
                    }
                    let capture = capture.context("Copilot identity record capture ended before replacement capture")??;
                    anyhow::ensure!(
                        this.update(cx, |this, _| this
                            .complete_capture_attempt(&capture_cancellation))?,
                        "Copilot identity arrived after the launch attempt was replaced"
                    );
                    let claim = resumable_cli::try_claim_target(&capture.session).context(
                        "Copilot reported an AI session already active in another terminal",
                    )?;
                    this.update(cx, |this, cx| {
                            this.set_resumable_cli_session_with_claim(
                                capture.session,
                                claim,
                                cx,
                            )
                        })?
                        .await?;
                    return Ok(());
                }

                if provider == ResumableCliProvider::Claude {
                    cx.background_executor()
                        .spawn(async { resumable_cli::verify_claude_contract() })
                        .await?;
                    let listener = resumable_cli::start_claude_capture(working_directory.clone())?;
                    let command = resumable_cli::claude_new_command(
                        &listener.session_id,
                        &listener.plugin_path,
                    )?;
                    let terminal = project
                        .update(cx, |project, cx| {
                            project.create_terminal_shell_with_env(
                                Some(working_directory),
                                listener.observer.environment.clone(),
                                cx,
                            )
                        })?
                        .await?;
                    this.update_in(cx, |this, window, cx| {
                        this.replace_terminal(terminal, window, cx);
                        this.observe_cli_identity(listener.observer, cx);
                        this.write_ai_command_after_startup(command, cx);
                    })?;
                    return Ok(());
                }

                anyhow::bail!("unsupported agent replacement path")

            }
            .await;

            if let Err(error) = outcome {
                this.update(cx, |this, cx| {
                    let message = this.capture_error_message(provider, &error, cx);
                    this.show_resumable_cli_error(anyhow::anyhow!(message), cx);
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    #[cfg(target_os = "windows")]
    fn capture_error_message(
        &self,
        provider: ResumableCliProvider,
        error: &anyhow::Error,
        cx: &App,
    ) -> String {
        let pane = self
            .workspace
            .read_with(cx, |workspace, cx| {
                workspace
                    .panes()
                    .iter()
                    .position(|pane| {
                        pane.read(cx)
                            .items()
                            .any(|item| item.item_id() == self.self_handle.entity_id())
                    })
                    .map(|index| format!("Center {}", index + 1))
            })
            .ok()
            .flatten();
        let title = self.tab_content_text(0, cx);
        resumable_cli::capture_error_message(provider, pane.as_deref(), Some(&title), error)
    }

    #[cfg(target_os = "windows")]
    fn show_resumable_cli_error(&self, error: anyhow::Error, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| workspace.show_error(error, cx));
        }
    }

    fn focus_out(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.blink_manager.update(cx, BlinkManager::disable);
        self.terminal.update(cx, |terminal, _| {
            if !self.read_only {
                terminal.focus_out();
            }
            terminal.set_cursor_shape(CursorShape::Hollow);
        });
        cx.notify();
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // TODO: this should be moved out of render
        self.scroll_handle.update(self.terminal.read(cx));

        if let Some(new_display_offset) = self.scroll_handle.future_display_offset.take() {
            self.terminal.update(cx, |term, _| {
                let delta = new_display_offset as i32 - term.last_content.display_offset as i32;
                match delta.cmp(&0) {
                    cmp::Ordering::Greater => term.scroll_up_by(delta as usize),
                    cmp::Ordering::Less => term.scroll_down_by(-delta as usize),
                    cmp::Ordering::Equal => {}
                }
            });
        }

        let terminal_handle = self.terminal.clone();
        let terminal_view_handle = cx.entity();

        let focused = self.focus_handle.is_focused(window);
        #[cfg(target_os = "windows")]
        let conceal_resumable_cli_launch = self.should_conceal_resumable_cli_launch(cx);
        #[cfg(not(target_os = "windows"))]
        let conceal_resumable_cli_launch = false;

        div()
            .id("terminal-view")
            .size_full()
            .relative()
            .track_focus(&self.focus_handle(cx))
            .key_context(self.dispatch_context(cx))
            .on_action(cx.listener(TerminalView::send_text))
            .on_action(cx.listener(TerminalView::send_keystroke))
            .on_action(cx.listener(TerminalView::copy))
            .on_action(cx.listener(TerminalView::editor_copy))
            .on_action(cx.listener(TerminalView::paste))
            .on_action(cx.listener(TerminalView::editor_paste))
            .on_action(cx.listener(TerminalView::paste_text))
            .on_action(cx.listener(TerminalView::clear))
            .on_action(cx.listener(TerminalView::scroll_line_up))
            .on_action(cx.listener(TerminalView::scroll_line_down))
            .on_action(cx.listener(TerminalView::scroll_page_up))
            .on_action(cx.listener(TerminalView::scroll_page_down))
            .on_action(cx.listener(TerminalView::scroll_to_top))
            .on_action(cx.listener(TerminalView::scroll_to_bottom))
            .on_action(cx.listener(TerminalView::toggle_vi_mode))
            .on_action(cx.listener(TerminalView::show_character_palette))
            .on_action(cx.listener(TerminalView::select_all))
            .on_action(cx.listener(TerminalView::rerun_task))
            .on_action(cx.listener(TerminalView::rename_terminal))
            .when(cfg!(target_os = "windows"), |element| {
                #[cfg(target_os = "windows")]
                {
                    element
                        .on_action(cx.listener(TerminalView::resume_ai_session))
                        .on_action(cx.listener(TerminalView::start_new_ai_session))
                        .on_action(cx.listener(TerminalView::forget_ai_session))
                }
                #[cfg(not(target_os = "windows"))]
                {
                    element
                }
            })
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    if this.should_deploy_context_menu(event.modifiers.shift, cx) {
                        let had_selection = this.terminal.read(cx).last_content.selection.is_some();
                        if !had_selection {
                            this.terminal.update(cx, |terminal, _| {
                                terminal.select_word_at_event_position(event);
                            });
                        }
                        let has_selection = !had_selection
                            || this
                                .terminal
                                .read(cx)
                                .last_content
                                .selection_text
                                .as_ref()
                                .is_some_and(|text| !text.is_empty());
                        this.deploy_context_menu(event.position, has_selection, window, cx);
                        cx.notify();
                    }
                }),
            )
            .child(
                // TODO: Oddly this wrapper div is needed for TerminalElement to not steal events from the context menu
                div()
                    .id("terminal-view-container")
                    .size_full()
                    .bg(cx.theme().colors().editor_background)
                    .when_some(self.background_corner_radii, |this, radii| {
                        this.rounded_tl(radii.top_left)
                            .rounded_tr(radii.top_right)
                            .rounded_bl(radii.bottom_left)
                            .rounded_br(radii.bottom_right)
                    })
                    .child(TerminalElement::new(
                        terminal_handle,
                        terminal_view_handle,
                        self.workspace.clone(),
                        self.focus_handle.clone(),
                        focused,
                        self.should_show_cursor(focused, cx),
                        self.block_below_cursor.clone(),
                        self.mode.clone(),
                    ))
                    .when(self.content_mode(window, cx).is_scrollable(), |div| {
                        let colors = cx.theme().colors();
                        div.custom_scrollbars(
                            Scrollbars::for_settings::<TerminalScrollbarSettingsWrapper>()
                                .show_along(ScrollAxes::Vertical)
                                .with_stable_track_along(
                                    ScrollAxes::Vertical,
                                    colors.editor_background,
                                )
                                .tracked_scroll_handle(&self.scroll_handle),
                            window,
                            cx,
                        )
                    }),
            )
            .when(conceal_resumable_cli_launch, |terminal_view| {
                terminal_view.child(
                    div()
                        .id("resumable-cli-launch-cover")
                        .absolute()
                        .inset_0()
                        .bg(cx.theme().colors().editor_background),
                )
            })
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

fn project_worktree_root_for_directory(
    project: &Project,
    working_directory: Option<&Path>,
    cx: &App,
) -> Option<PathBuf> {
    let path = project.find_project_path(working_directory?, cx)?;
    Some(
        project
            .worktree_for_id(path.worktree_id, cx)?
            .read(cx)
            .abs_path()
            .to_path_buf(),
    )
}

fn project_path_for_terminal(
    project: &Project,
    root: Option<&Path>,
    cx: &App,
) -> Option<ProjectPath> {
    let root = root?;
    project.worktrees(cx).find_map(|worktree| {
        let worktree = worktree.read(cx);
        (worktree.abs_path().as_ref() == root).then(|| ProjectPath {
            worktree_id: worktree.id(),
            path: util::rel_path::RelPath::empty_arc(),
        })
    })
}

impl Item for TerminalView {
    type Event = ItemEvent;

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        let project = self.project.upgrade()?;
        let root = self.project_worktree_root(cx);
        project_path_for_terminal(project.read(cx), root.as_deref(), cx)
    }

    fn include_in_nav_history() -> bool {
        false
    }

    fn affects_workspace_theme(&self, _: &App) -> bool {
        false
    }

    fn tab_tooltip_content(&self, cx: &App) -> Option<TabTooltipContent> {
        Some(TabTooltipContent::Custom(Box::new(Tooltip::element({
            let terminal = self.terminal().read(cx);
            let title = terminal.title(false);
            let pid = terminal.pid_getter()?.fallback_pid();

            move |_, _| {
                v_flex()
                    .gap_1()
                    .child(Label::new(title.clone()))
                    .child(h_flex().flex_grow_1().child(Divider::horizontal()))
                    .child(
                        Label::new(format!("Process ID (PID): {}", pid))
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .into_any_element()
            }
        }))))
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let terminal = self.terminal().read(cx);
        let title = self
            .custom_title
            .as_ref()
            .filter(|title| !title.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| terminal.title(true));

        let (icon, icon_color, rerun_button) = match terminal.task() {
            Some(terminal_task) => match &terminal_task.status {
                TaskStatus::Running => (
                    IconName::PlayFilled,
                    Color::Disabled,
                    self.rerun_button(terminal_task),
                ),
                TaskStatus::Unknown => (
                    IconName::Warning,
                    Color::Warning,
                    self.rerun_button(terminal_task),
                ),
                TaskStatus::Completed { success } => {
                    let rerun_button = self.rerun_button(terminal_task);

                    if *success {
                        (IconName::Check, Color::Success, rerun_button)
                    } else {
                        (IconName::XCircle, Color::Error, rerun_button)
                    }
                }
            },
            None => (IconName::Terminal, Color::Muted, None),
        };

        let self_handle = self.self_handle.clone();
        h_flex()
            .gap_1()
            .group("term-tab-icon")
            .when(!params.selected, |this| {
                this.track_focus(&self.focus_handle)
            })
            .on_action(move |action: &RenameTerminal, window, cx| {
                self_handle
                    .update(cx, |this, cx| this.rename_terminal(action, window, cx))
                    .ok();
            })
            .child(
                h_flex()
                    .group("term-tab-icon")
                    .child(
                        div()
                            .when(rerun_button.is_some(), |this| {
                                this.hover(|style| style.invisible().w_0())
                            })
                            .child(Icon::new(icon).color(icon_color)),
                    )
                    .when_some(rerun_button, |this, rerun_button| {
                        this.child(
                            div()
                                .absolute()
                                .visible_on_hover("term-tab-icon")
                                .child(rerun_button),
                        )
                    }),
            )
            .child(
                div()
                    .relative()
                    .child(
                        Label::new(title)
                            .single_line()
                            .color(params.text_color())
                            .when(self.is_renaming(), |this| this.alpha(0.)),
                    )
                    .when_some(self.rename_editor.clone(), |this, editor| {
                        let self_handle = self.self_handle.clone();
                        let self_handle_cancel = self.self_handle.clone();
                        this.child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .size_full()
                                .child(editor)
                                .on_action(move |_: &menu::Confirm, window, cx| {
                                    self_handle
                                        .update(cx, |this, cx| {
                                            this.finish_renaming(true, window, cx)
                                        })
                                        .ok();
                                })
                                .on_action(move |_: &menu::Cancel, window, cx| {
                                    self_handle_cancel
                                        .update(cx, |this, cx| {
                                            this.finish_renaming(false, window, cx)
                                        })
                                        .ok();
                                }),
                        )
                    }),
            )
            .into_any()
    }

    fn tab_content_text(&self, detail: usize, cx: &App) -> SharedString {
        if let Some(custom_title) = self.custom_title.as_ref().filter(|l| !l.trim().is_empty()) {
            return custom_title.clone().into();
        }
        let terminal = self.terminal().read(cx);
        terminal.title(detail == 0).into()
    }

    fn tab_indicator(&self, cx: &App) -> Option<Indicator> {
        let is_agent = self.active_resumable_cli_provider().is_some()
            || self
                .terminal()
                .read(cx)
                .foreground_process_command_name()
                .is_some_and(|command| is_known_terminal_agent_command(&command));
        is_agent.then(|| Indicator::bar().color(Color::Custom(gpui::rgba(0x3aafa9b3).into())))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn handle_drop(
        &self,
        active_pane: &Pane,
        dropped: &dyn Any,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        if self.read_only {
            return false;
        }
        let Some(project) = self.project.upgrade() else {
            return false;
        };

        if let Some(paths) = dropped.downcast_ref::<ExternalPaths>() {
            let is_local = project.read(cx).is_local();
            if is_local {
                self.add_paths_to_terminal(paths.paths(), window, cx);
                return true;
            }

            return false;
        } else if let Some(tab) = dropped.downcast_ref::<DraggedTab>() {
            let Some(self_handle) = self.self_handle.upgrade() else {
                return false;
            };

            let Some(workspace) = self.workspace.upgrade() else {
                return false;
            };

            let Some(this_pane) = workspace.read(cx).pane_for(&self_handle) else {
                return false;
            };

            let item = if tab.pane == this_pane {
                active_pane.item_for_index(tab.ix)
            } else {
                tab.pane.read(cx).item_for_index(tab.ix)
            };

            let Some(item) = item else {
                return false;
            };

            if item.downcast::<TerminalView>().is_some() {
                let Some(split_direction) = active_pane.drag_split_direction() else {
                    return false;
                };

                let Some(terminal_panel) = workspace.read(cx).panel::<TerminalPanel>(cx) else {
                    return false;
                };

                if !terminal_panel.read(cx).center.panes().contains(&&this_pane) {
                    return false;
                }

                let source = tab.pane.clone();
                let item_id_to_move = item.item_id();
                let is_zoomed = {
                    let terminal_panel = terminal_panel.read(cx);
                    if terminal_panel.active_pane == this_pane {
                        active_pane.is_zoomed()
                    } else {
                        terminal_panel.active_pane.read(cx).is_zoomed()
                    }
                };

                let workspace = workspace.downgrade();
                let terminal_panel = terminal_panel.downgrade();
                // Defer the split operation to avoid re-entrancy panic.
                // The pane may be the one currently being updated, so we cannot
                // call mark_positions (via split) synchronously.
                window
                    .spawn(cx, async move |cx| {
                        cx.update(|window, cx| {
                            let Ok(new_pane) = terminal_panel.update(cx, |terminal_panel, cx| {
                                let new_pane = terminal_panel::new_terminal_pane(
                                    workspace, project, is_zoomed, window, cx,
                                );
                                terminal_panel.apply_tab_bar_buttons(&new_pane, cx);
                                terminal_panel.center.split(
                                    &this_pane,
                                    &new_pane,
                                    split_direction,
                                    cx,
                                );
                                anyhow::Ok(new_pane)
                            }) else {
                                return;
                            };

                            let Some(new_pane) = new_pane.log_err() else {
                                return;
                            };

                            workspace::move_item(
                                &source,
                                &new_pane,
                                item_id_to_move,
                                new_pane.read(cx).active_item_index(),
                                true,
                                window,
                                cx,
                            );
                        })
                        .ok();
                    })
                    .detach();

                return true;
            } else {
                if let Some(project_path) = item.project_path(cx)
                    && let Some(path) = project.read(cx).absolute_path(&project_path, cx)
                {
                    self.add_paths_to_terminal(&[path], window, cx);
                    return true;
                }
            }

            return false;
        } else if let Some(selection) = dropped.downcast_ref::<DraggedSelection>() {
            let project = project.read(cx);
            let paths = selection
                .items()
                .map(|selected_entry| selected_entry.entry_id)
                .filter_map(|entry_id| project.path_for_entry(entry_id, cx))
                .filter_map(|project_path| project.absolute_path(&project_path, cx))
                .collect::<Vec<_>>();

            if !paths.is_empty() {
                self.add_paths_to_terminal(&paths, window, cx);
            }

            return true;
        } else if let Some(&entry_id) = dropped.downcast_ref::<ProjectEntryId>() {
            let project = project.read(cx);
            if let Some(path) = project
                .path_for_entry(entry_id, cx)
                .and_then(|project_path| project.absolute_path(&project_path, cx))
            {
                self.add_paths_to_terminal(&[path], window, cx);
            }

            return true;
        }

        false
    }

    fn tab_extra_context_menu_actions(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<(SharedString, Box<dyn gpui::Action>)> {
        let terminal = self.terminal.read(cx);
        if terminal.task().is_none() {
            let mut actions: Vec<(SharedString, Box<dyn gpui::Action>)> =
                vec![("Rename".into(), Box::new(RenameTerminal))];
            #[cfg(target_os = "windows")]
            actions.extend(self.resumable_cli_menu_actions());
            actions
        } else {
            Vec::new()
        }
    }

    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        let Ok(terminal) = self.project.update(cx, |project, cx| {
            let cwd = project
                .active_project_directory(cx)
                .map(|it| it.to_path_buf());
            project.clone_terminal(self.terminal(), cx, cwd)
        }) else {
            return Task::ready(None);
        };
        let persistent_terminal_id = self
            .persistent_terminal_id
            .as_ref()
            .map(|_| PersistentTerminalId::new());
        cx.spawn_in(window, async move |this, cx| {
            let terminal = terminal.await.log_err()?;
            this.update_in(cx, |this, window, cx| {
                cx.new(|cx| {
                    let view = TerminalView::new(
                        terminal,
                        this.workspace.clone(),
                        workspace_id,
                        this.project.clone(),
                        window,
                        cx,
                    );
                    if let Some(persistent_terminal_id) = persistent_terminal_id {
                        view.with_persistent_terminal_id(persistent_terminal_id)
                    } else {
                        view
                    }
                })
            })
            .ok()
        })
    }

    fn is_dirty(&self, cx: &App) -> bool {
        match self.terminal.read(cx).task() {
            Some(task) => task.status == TaskStatus::Running,
            None => self.has_bell(),
        }
    }

    fn has_conflict(&self, _cx: &App) -> bool {
        false
    }

    fn can_save_as(&self, _cx: &App) -> bool {
        false
    }

    fn as_searchable(
        &self,
        handle: &Entity<Self>,
        _: &App,
    ) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(handle.clone()))
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        if self.show_breadcrumbs && !self.terminal().read(cx).breadcrumb_text.trim().is_empty() {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        Some((
            vec![HighlightedText {
                text: self.terminal().read(cx).breadcrumb_text.clone().into(),
                highlights: vec![],
            }],
            None,
        ))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.terminal().read(cx).task().is_none() {
            if let Some((new_id, old_id)) = workspace.database_id().zip(self.workspace_id) {
                log::debug!(
                    "Updating workspace id for the terminal, old: {old_id:?}, new: {new_id:?}",
                );
                let db = TerminalDb::global(cx);
                let entity_id = cx.entity_id().as_u64();
                let persistent_item_id = self
                    .persistent_terminal_id
                    .as_ref()
                    .map(|id| id.as_str().to_owned());
                cx.background_spawn(async move {
                    if let Some(persistent_item_id) = persistent_item_id {
                        db.update_persistent_center_workspace_id(new_id, old_id, persistent_item_id)
                            .await
                    } else {
                        db.update_workspace_id(new_id, old_id, entity_id).await
                    }
                })
                .detach();
            }
            self.workspace_id = workspace.database_id();
        }
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl SerializableItem for TerminalView {
    fn serialized_item_kind() -> &'static str {
        "Terminal"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<workspace::ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        let db = TerminalDb::global(cx);
        delete_unloaded_items(alive_items, workspace_id, "terminals", &db, cx)
    }

    fn cleanup_with_persistent_item_ids(
        workspace_id: WorkspaceId,
        _alive_items: Vec<workspace::ItemId>,
        persistent_item_ids: Vec<PersistentItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        let persistent_item_ids = persistent_item_ids
            .into_iter()
            .map(PersistentTerminalId::from_layout)
            .collect::<Option<Vec<_>>>();
        let Some(mut persistent_item_ids) = persistent_item_ids else {
            return Task::ready(Err(anyhow::anyhow!(
                "invalid persistent center terminal identity; cleanup suppressed"
            )));
        };
        match TerminalPanel::saved_persistent_terminal_ids(workspace_id, cx) {
            Ok(dock_ids) => persistent_item_ids.extend(dock_ids),
            Err(error) => {
                return Task::ready(Err(error.context(
                    "terminal cleanup suppressed while reading saved dock identities",
                )));
            }
        }
        let db = TerminalDb::global(cx);
        cx.background_spawn(async move {
            db.delete_unloaded_persistent_center_terminals(workspace_id, persistent_item_ids)
                .await
        })
    }

    fn persistent_item_id(&self) -> Option<PersistentItemId> {
        self.persistent_terminal_id
            .as_ref()
            .map(PersistentTerminalId::as_persistent_item_id)
    }

    fn serialize(
        &mut self,
        _workspace: &mut Workspace,
        item_id: workspace::ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        self.persist_snapshot(item_id, cx)
    }

    fn should_serialize(&self, _: &Self::Event) -> bool {
        self.needs_serialize
    }

    fn deserialize(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let database = TerminalDb::global(cx);
        let persistent_id = PersistentTerminalId::legacy(workspace_id, item_id);
        window.spawn(cx, async move |cx| {
            database
                .migrate_legacy_terminal(workspace_id, item_id, persistent_id.clone())
                .await?;
            cx.update(|window, cx| {
                Self::deserialize_with_persistent_item_id(
                    project,
                    workspace,
                    workspace_id,
                    item_id,
                    Some(persistent_id.as_persistent_item_id()),
                    window,
                    cx,
                )
            })?
            .await
        })
    }

    fn deserialize_with_persistent_item_id(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: workspace::ItemId,
        persistent_item_id: Option<PersistentItemId>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let (persistent_terminal_id, migrate_legacy) = match persistent_item_id {
            Some(persistent_item_id) => {
                let Some(persistent_terminal_id) =
                    PersistentTerminalId::from_layout(persistent_item_id)
                else {
                    return Task::ready(Err(anyhow::anyhow!(
                        "invalid persistent center terminal identity"
                    )));
                };
                (persistent_terminal_id, false)
            }
            None => (PersistentTerminalId::legacy(workspace_id, item_id), true),
        };
        let db = TerminalDb::global(cx);
        let fallback_cwd = workspace
            .upgrade()
            .and_then(|workspace| default_working_directory(workspace.read(cx), cx));

        window.spawn(cx, async move |cx| {
            if migrate_legacy {
                db.migrate_legacy_center_terminals(workspace_id).await?;
            }
            let snapshot =
                db.get_persistent_center_snapshot(&persistent_terminal_id, workspace_id)?;
            let (cwd, custom_title, resumable_cli_binding) = snapshot
                .map(|(cwd, title, binding)| {
                    let cwd = cwd.filter(|cwd| !cwd.as_os_str().is_empty());
                    let title = title.filter(|title| !title.trim().is_empty());
                    (cwd.or_else(|| fallback_cwd.clone()), title, binding)
                })
                .unwrap_or((fallback_cwd, None, ResumableCliBinding::default()));
            let mut project_worktree_association = db
                .persistent_project_worktree_association(&persistent_terminal_id, workspace_id)
                .log_err()
                .unwrap_or(ProjectWorktreeAssociation::Recorded(None));
            if project_worktree_association == ProjectWorktreeAssociation::Unknown
                && let Some(session) = resumable_cli_binding.session()
            {
                let inferred_root = project.read_with(cx, |project, cx| {
                    project_worktree_root_for_directory(project, Some(session.resume_cwd()), cx)
                });
                db.record_persistent_project_worktree_association(
                    persistent_terminal_id.clone(),
                    workspace_id,
                    ProjectWorktreeAssociation::Recorded(inferred_root),
                )
                .await?;
                project_worktree_association = db.persistent_project_worktree_association(
                    &persistent_terminal_id,
                    workspace_id,
                )?;
            }
            let terminal = project
                .update(cx, |project, cx| project.create_terminal_shell(cwd, cx))
                .await?;
            cx.update(|window, cx| {
                cx.new(|cx| {
                    let mut view = TerminalView::new(
                        terminal,
                        workspace,
                        Some(workspace_id),
                        project.downgrade(),
                        window,
                        cx,
                    )
                    .with_persistent_terminal_id(persistent_terminal_id);
                    view.project_worktree_association = project_worktree_association;
                    view.custom_title = custom_title;
                    view.resumable_cli_binding = resumable_cli_binding;
                    view.resume_on_first_focus = view.resumable_cli_provider().is_some();
                    view.needs_serialize = false;
                    view
                })
            })
        })
    }
}

impl SearchableItem for TerminalView {
    type Match = Range;

    fn supported_options(&self) -> SearchOptions {
        SearchOptions {
            case: false,
            word: false,
            regex: true,
            replacement: false,
            selection: false,
            select_all: false,
            find_in_results: false,
        }
    }

    /// Clear stored matches
    fn clear_matches(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.terminal().update(cx, |term, _| term.matches.clear())
    }

    /// Store matches returned from find_matches somewhere for rendering
    fn update_matches(
        &mut self,
        matches: &[Self::Match],
        _active_match_index: Option<usize>,
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal()
            .update(cx, |term, _| term.matches = matches.to_vec())
    }

    /// Returns the selection content to pre-load into this search
    fn query_suggestion(
        &mut self,
        _seed_query_override: Option<SeedQuerySetting>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> String {
        self.terminal()
            .read(cx)
            .last_content
            .selection_text
            .clone()
            .unwrap_or_default()
    }

    /// Focus match at given index into the Vec of matches
    fn activate_match(
        &mut self,
        index: usize,
        _: &[Self::Match],
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal()
            .update(cx, |term, _| term.activate_match(index));
        cx.notify();
    }

    /// Add selections for all matches given.
    fn select_matches(
        &mut self,
        matches: &[Self::Match],
        _token: SearchToken,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal()
            .update(cx, |term, _| term.select_matches(matches));
        cx.notify();
    }

    /// Get all of the matches for this query, should be done on the background
    fn find_matches(
        &mut self,
        query: Arc<SearchQuery>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Vec<Self::Match>> {
        if let Some(s) = regex_search_for_query(&query) {
            self.terminal()
                .update(cx, |term, cx| term.find_matches(s, cx))
        } else {
            Task::ready(vec![])
        }
    }

    /// Reports back to the search toolbar what the active match should be (the selection)
    fn active_match_index(
        &mut self,
        direction: Direction,
        matches: &[Self::Match],
        _token: SearchToken,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        // Selection head might have a value if there's a selection that isn't
        // associated with a match. Therefore, if there are no matches, we should
        // report None, no matter the state of the terminal

        if !matches.is_empty() {
            if let Some(selection_head) = self.terminal().read(cx).selection_head {
                // If selection head is contained in a match. Return that match
                match direction {
                    Direction::Prev => {
                        // If no selection before selection head, return the first match
                        Some(
                            matches
                                .iter()
                                .enumerate()
                                .rev()
                                .find(|(_, search_match)| {
                                    search_match.contains(selection_head)
                                        || search_match.start() < selection_head
                                })
                                .map(|(ix, _)| ix)
                                .unwrap_or(0),
                        )
                    }
                    Direction::Next => {
                        // If no selection after selection head, return the last match
                        Some(
                            matches
                                .iter()
                                .enumerate()
                                .find(|(_, search_match)| {
                                    search_match.contains(selection_head)
                                        || search_match.start() > selection_head
                                })
                                .map(|(ix, _)| ix)
                                .unwrap_or(matches.len().saturating_sub(1)),
                        )
                    }
                }
            } else {
                // Matches found but no active selection, return the first last one (closest to cursor)
                Some(matches.len().saturating_sub(1))
            }
        } else {
            None
        }
    }
    fn replace(
        &mut self,
        _: &Self::Match,
        _: &SearchQuery,
        _token: SearchToken,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
        // Replacement is not supported in terminal view, so this is a no-op.
    }
}

/// Gets the working directory for the given workspace, respecting the user's settings.
/// Falls back to home directory when no project directory is available.
///
/// For remote projects, local-only resolution (home dir fallback, shell expansion,
/// local `is_dir` checks) is skipped -- returning `None` lets the remote shell
/// open in the remote user's home directory by default.
pub fn default_working_directory(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let is_remote = workspace.project().read(cx).is_remote();
    let directory = match &TerminalSettings::get_global(cx).working_directory {
        WorkingDirectory::CurrentFileDirectory => workspace
            .project()
            .read(cx)
            .active_entry_directory(cx)
            .or_else(|| current_project_directory(workspace, cx)),
        WorkingDirectory::CurrentProjectDirectory => current_project_directory(workspace, cx),
        WorkingDirectory::FirstProjectDirectory => first_project_directory(workspace, cx),
        WorkingDirectory::AlwaysHome => None,
        WorkingDirectory::Always { directory } if !is_remote => shellexpand::full(directory)
            .ok()
            .map(|dir| Path::new(&dir.to_string()).to_path_buf())
            .filter(|dir| dir.is_dir()),
        WorkingDirectory::Always { .. } => None,
    };

    if is_remote {
        directory
    } else {
        directory.or_else(dirs::home_dir)
    }
}

fn current_project_directory(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    workspace
        .project()
        .read(cx)
        .active_project_directory(cx)
        .as_deref()
        .map(Path::to_path_buf)
        .or_else(|| first_project_directory(workspace, cx))
}

///Gets the first project's home directory, or the home directory
fn first_project_directory(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let worktree = workspace.worktrees(cx).next()?.read(cx);
    let worktree_path = worktree.abs_path();
    if worktree.root_entry()?.is_dir() {
        Some(worktree_path.to_path_buf())
    } else {
        // If worktree is a file, return its parent directory
        worktree_path.parent().map(|p| p.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, UpdateGlobal, VisualTestContext};
    use project::{Entry, Project, ProjectPath, Worktree};
    use remote::RemoteClient;
    use std::path::{Path, PathBuf};
    use util::paths::PathStyle;
    use util::rel_path::RelPath;
    use workspace::item::test::{TestItem, TestProjectItem};
    use workspace::{AppState, MultiWorkspace, SelectedEntry};

    #[gpui::test]
    async fn terminal_bell_is_acknowledged_on_content_focus(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (pane, terminal, view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.update(|window, cx| assert!(!view.read(cx).focus_handle.is_focused(window)));
        terminal.update(&mut cx, |_, cx| cx.emit(Event::Bell));
        cx.run_until_parked();
        assert!(view.read_with(&cx, |view, cx| view.is_dirty(cx)));

        pane.update_in(&mut cx, |pane, window, cx| {
            pane.activate_item(0, true, true, window, cx);
        });
        cx.run_until_parked();
        assert!(!view.read_with(&cx, |view, cx| view.is_dirty(cx)));

        terminal.update(&mut cx, |_, cx| cx.emit(Event::Bell));
        cx.run_until_parked();
        assert!(!view.read_with(&cx, |view, cx| view.is_dirty(cx)));
    }

    #[gpui::test]
    async fn terminal_bell_refreshes_tab_without_more_output(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        add_display_only_terminal(&project, window_handle, true, false, cx);
        cx.run_until_parked();
        let updates = Rc::new(std::cell::Cell::new(0));
        let _subscription = cx.update(|cx| {
            let updates = updates.clone();
            cx.subscribe(&view, move |_, event: &ItemEvent, _| {
                if matches!(event, ItemEvent::UpdateTab) {
                    updates.set(updates.get() + 1);
                }
            })
        });
        terminal.update(cx, |_, cx| cx.emit(Event::Bell));
        cx.run_until_parked();
        assert_eq!(updates.get(), 1);
        view.update(cx, |view, cx| view.clear_bell(cx));
        cx.run_until_parked();
        assert_eq!(updates.get(), 2);
        assert!(!view.read_with(cx, |view, cx| view.is_dirty(cx)));
        view.update(cx, |view, cx| view.clear_bell(cx));
        cx.run_until_parked();
        assert_eq!(updates.get(), 2);
    }

    #[gpui::test]
    async fn terminal_bell_tracks_inactive_window_and_target_focus(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (pane, first_terminal, first) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        let (_, second_terminal, second) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        for terminal in [&first_terminal, &second_terminal, &first_terminal] {
            terminal.update(&mut cx, |_, cx| cx.emit(Event::Bell));
        }
        cx.run_until_parked();
        assert!(first.read_with(&cx, |view, cx| view.is_dirty(cx)));
        assert!(second.read_with(&cx, |view, cx| view.is_dirty(cx)));
        pane.update_in(&mut cx, |pane, window, cx| {
            pane.activate_item(0, true, true, window, cx);
        });
        cx.run_until_parked();
        assert!(!first.read_with(&cx, |view, cx| view.is_dirty(cx)));
        assert!(second.read_with(&cx, |view, cx| view.is_dirty(cx)));
        assert!(
            first_terminal
                .update(&mut cx, |terminal, _| terminal.take_input_log())
                .is_empty()
        );

        cx.deactivate_window();
        first_terminal.update(&mut cx, |_, cx| cx.emit(Event::Bell));
        cx.run_until_parked();
        assert!(first.read_with(&cx, |view, cx| view.is_dirty(cx)));
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        assert!(!first.read_with(&cx, |view, cx| view.is_dirty(cx)));
        assert!(second.read_with(&cx, |view, cx| view.is_dirty(cx)));
        second.update_in(&mut cx, |view, window, cx| {
            view.send_text(&SendText("x".into()), window, cx)
        });
        cx.run_until_parked();
        assert!(!second.read_with(&cx, |view, cx| view.is_dirty(cx)));
    }

    fn expected_drop_text(paths: &[PathBuf]) -> String {
        let mut text = String::new();
        for path in paths {
            text.push(' ');
            text.push_str(&shlex::try_quote(path.to_str().unwrap()).unwrap());
        }
        text.push(' ');
        text
    }

    fn assert_drop_writes_to_terminal(
        pane: &Entity<Pane>,
        terminal_view_index: usize,
        terminal: &Entity<Terminal>,
        dropped: &dyn Any,
        expected_text: &str,
        window: &mut Window,
        cx: &mut Context<MultiWorkspace>,
    ) {
        let _ = terminal.update(cx, |terminal, _| terminal.take_input_log());

        let handled = pane.update(cx, |pane, cx| {
            pane.item_for_index(terminal_view_index)
                .unwrap()
                .handle_drop(pane, dropped, window, cx)
        });
        assert!(handled, "handle_drop should return true for {:?}", dropped);

        let mut input_log = terminal.update(cx, |terminal, _| terminal.take_input_log());
        assert_eq!(input_log.len(), 1, "expected exactly one write to terminal");
        let written =
            String::from_utf8(input_log.remove(0)).expect("terminal write should be valid UTF-8");
        assert_eq!(written, expected_text);
    }

    // DEC private mode 1049: a program writes this to enter the alternate screen buffer.
    const ENTER_ALT_SCREEN: &[u8] = b"\x1b[?1049h";

    // CSI `1;2A` = cursor-up with the xterm Shift modifier (`1 + 1` for Shift).
    const SHIFT_UP_ESCAPE: &[u8] = b"\x1b[1;2A";

    #[cfg(target_os = "windows")]
    fn test_resumable_session(_id: &str) -> ResumableCliSession {
        ResumableCliSession::new(
            ResumableCliProvider::Codex,
            uuid::Uuid::now_v7().to_string(),
            std::env::current_dir().unwrap().canonicalize().unwrap(),
            ResumableCliEvidence::ProviderEvent,
        )
        .unwrap()
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn resumable_cli_actions_are_shared_by_tab_and_terminal_context_menus(
        cx: &mut TestAppContext,
    ) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        window_handle
            .update(cx, |_, window, cx| {
                terminal.update(cx, |terminal, cx| {
                    terminal.write_output(b"\x1b[?1000h", cx);
                    terminal.sync(window, cx);
                });
            })
            .unwrap();
        assert!(terminal.read_with(cx, |terminal, _| {
            terminal.mouse_mode(false, MouseInputMode::ReportToTerminal)
        }));

        let (labels, opens_context_menu) = terminal_view.update(cx, |view, cx| {
            view.resumable_cli_binding = ResumableCliBinding {
                pending_codex_cwd: None,
                revision: 1,
                session: Some(test_resumable_session(
                    "0199a4f5-1fe0-7000-8000-000000000008",
                )),
            };
            (
                view.resumable_cli_menu_actions()
                    .into_iter()
                    .map(|(label, _)| label.to_string())
                    .collect::<Vec<_>>(),
                view.should_deploy_context_menu(false, cx),
            )
        });
        assert!(opens_context_menu);
        assert_eq!(
            labels,
            [
                "Resume AI Session",
                "Start New AI Session",
                "Forget AI Session"
            ]
        );
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn resumable_cli_launch_cover_clears_on_alternate_screen(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);

        terminal_view.update(cx, |view, cx| {
            view.conceal_resumable_cli_launch = true;
            assert!(view.should_conceal_resumable_cli_launch(cx));
        });

        window_handle
            .update(cx, |_, window, cx| {
                terminal.update(cx, |terminal, cx| {
                    terminal.write_output(ENTER_ALT_SCREEN, cx);
                    terminal.sync(window, cx);
                });
            })
            .unwrap();

        terminal_view.update(cx, |view, cx| {
            assert!(!view.should_conceal_resumable_cli_launch(cx));
            assert!(!view.conceal_resumable_cli_launch);
        });
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn claimed_process_observation_state_table() {
        let provider = Some(ResumableCliProvider::Codex);

        assert_eq!(
            observe_claimed_process(None, true, Some("codex")),
            ClaimObservation {
                provider_seen: false,
                release: true,
            }
        );
        for command in [Some("powershell"), Some("cmd"), None] {
            assert_eq!(
                observe_claimed_process(provider, false, command),
                ClaimObservation {
                    provider_seen: false,
                    release: false,
                }
            );
        }
        assert_eq!(
            observe_claimed_process(provider, false, Some("CoDeX")),
            ClaimObservation {
                provider_seen: true,
                release: false,
            }
        );
        assert_eq!(
            observe_claimed_process(provider, true, Some("codex")),
            ClaimObservation {
                provider_seen: true,
                release: false,
            }
        );
        for command in [Some("powershell"), Some("cmd"), None] {
            assert_eq!(
                observe_claimed_process(provider, true, command),
                ClaimObservation {
                    provider_seen: false,
                    release: true,
                }
            );
        }
    }

    #[test]
    fn binding_revision_contract_reports_compare_and_set_rejection() {
        assert_eq!(require_binding_revision(Some(3)).unwrap(), 3);
        let error = require_binding_revision(None).unwrap_err().to_string();
        assert_eq!(error, SUPERSEDED_BINDING_ERROR);
        assert!(!error.contains("serialized writer"));
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn resume_attempt_guard_rejects_superseded_terminal_and_binding(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        let session = test_resumable_session("0199a4f5-1fe0-7000-8000-000000000001");

        let (first_generation, terminal_id) = terminal_view.update(cx, |view, _| {
            view.resumable_cli_binding = ResumableCliBinding {
                pending_codex_cwd: None,
                revision: 7,
                session: Some(session.clone()),
            };
            (view.begin_resume_attempt().0, terminal.entity_id())
        });
        assert!(
            terminal_view.read_with(cx, |view, _| view.resume_attempt_is_current(
                first_generation,
                terminal_id,
                7,
                &session
            ))
        );

        let second_generation = terminal_view.update(cx, |view, _| view.begin_resume_attempt().0);
        assert!(
            !terminal_view.read_with(cx, |view, _| view.resume_attempt_is_current(
                first_generation,
                terminal_id,
                7,
                &session
            ))
        );
        assert!(
            terminal_view.read_with(cx, |view, _| view.resume_attempt_is_current(
                second_generation,
                terminal_id,
                7,
                &session
            ))
        );

        let forget = terminal_view.update(cx, |view, cx| view.forget_resumable_cli_session(cx));
        assert!(forget.await.is_err());
        assert!(
            !terminal_view.read_with(cx, |view, _| view.resume_attempt_is_current(
                second_generation,
                terminal_id,
                7,
                &session
            ))
        );

        let third_generation = terminal_view.update(cx, |view, _| view.begin_resume_attempt().0);
        let replacement = cx.new(|cx| {
            terminal::TerminalBuilder::new_display_only(
                CursorShape::default(),
                terminal::terminal_settings::AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        window_handle
            .update(cx, |_, window, cx| {
                terminal_view.update(cx, |view, cx| {
                    view.replace_terminal(replacement, window, cx)
                });
            })
            .unwrap();
        assert!(
            !terminal_view.read_with(cx, |view, _| view.resume_attempt_is_current(
                third_generation,
                terminal_id,
                7,
                &session
            ))
        );
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn codex_observer_persists_prompt_switches_and_ignores_late_startup(
        cx: &mut TestAppContext,
    ) {
        // The observer intentionally waits for an external event stream between callbacks.
        cx.executor().allow_parking();
        cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        let workspace_id = cx
            .update(|cx| workspace::WorkspaceDb::global(cx))
            .next_id()
            .await
            .unwrap();
        let cwd = std::env::current_dir().unwrap();
        let persistent_id = PersistentTerminalId::legacy(workspace_id, 2401);
        let pending = view.update(cx, |view, cx| {
            view.workspace_id = Some(workspace_id);
            view.persistent_terminal_id = Some(persistent_id.clone());
            view.custom_title = Some("my Codex".into());
            view.project_worktree_association =
                ProjectWorktreeAssociation::Recorded(Some(cwd.clone()));
            view.replace_resumable_cli_association(None, Some(cwd.clone()), cx)
        });
        pending.await.unwrap();
        let (observer, mut sender) = resumable_cli::codex_identity_fixture();
        let cancellation = observer.cancellation.clone();
        view.update(cx, |view, cx| view.observe_cli_identity(observer, cx));
        let first = test_resumable_session("0199a4f5-1fe0-7000-8000-000000002401");
        let second = test_resumable_session("0199a4f5-1fe0-7000-8000-000000002402");
        let changed_context = ResumableCliSession::new(
            ResumableCliProvider::Codex,
            first.exact_resume_id().to_owned(),
            cwd.join("other-project"),
            ResumableCliEvidence::ProviderEvent,
        )
        .unwrap();
        for (session, is_prompt, expected, revision) in [
            (first.clone(), false, first.clone(), 2),
            (changed_context, false, first.clone(), 2),
            (first.clone(), true, first.clone(), 2),
            (second.clone(), false, first.clone(), 2),
            (second.clone(), true, second.clone(), 3),
            (first.clone(), false, second.clone(), 3),
            (second.clone(), true, second.clone(), 3),
            (first.clone(), true, first.clone(), 4),
        ] {
            sender
                .try_send(Ok(resumable_cli::CliIdentity {
                    prompt_started_at: None,
                    session,
                    is_prompt,
                }))
                .unwrap();
            cx.run_until_parked();
            let binding = view.read_with(cx, |view, _| view.resumable_cli_binding.clone());
            assert_eq!(binding.session(), Some(&expected));
            assert_eq!(binding.revision(), revision);
            assert!(binding.pending_codex_cwd().is_none());
            assert_eq!(
                view.read_with(cx, |view, _| view.terminal.entity_id()),
                terminal.entity_id()
            );
            assert_eq!(
                view.read_with(cx, |view, _| view.persistent_terminal_id.clone()),
                Some(persistent_id.clone())
            );
            assert_eq!(
                view.read_with(cx, |view, _| view.custom_title.clone()),
                Some("my Codex".into())
            );
            assert_eq!(
                view.read_with(cx, |view, _| view.project_worktree_association.clone()),
                ProjectWorktreeAssociation::Recorded(Some(cwd.clone()))
            );
            assert!(resumable_cli::try_claim_target(&expected).is_none());
        }
        let forgotten = view.update(cx, |view, cx| view.forget_resumable_cli_session(cx));
        forgotten.await.unwrap();
        assert!(cancellation.is_cancelled());
        assert!(
            sender
                .try_send(Ok(resumable_cli::CliIdentity {
                    prompt_started_at: None,
                    session: first.clone(),
                    is_prompt: true
                }))
                .is_err()
        );
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, _| view.resumable_cli_binding.session().is_none()));
        assert!(resumable_cli::try_claim_target(&first).is_some());
        assert!(resumable_cli::try_claim_target(&second).is_some());
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn claude_capture_survives_shell_startup_and_persists_selected_conversation(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, _terminal, view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        let workspace_id = cx
            .update(|cx| workspace::WorkspaceDb::global(cx))
            .next_id()
            .await
            .unwrap();
        let persistent_id = PersistentTerminalId::legacy(workspace_id, 2501);
        view.update(cx, |view, _| {
            view.workspace_id = Some(workspace_id);
            view.persistent_terminal_id = Some(persistent_id.clone());
        });
        let (observer, mut sender, plugin_path) = resumable_cli::claude_identity_fixture().unwrap();
        let cancellation = observer.cancellation.clone();
        view.update(cx, |view, cx| {
            view.observe_cli_identity(observer, cx);
            for command in [None, Some("pwsh"), Some("claude")] {
                view.update_session_claim_for_foreground_process(command);
            }
        });
        assert!(!cancellation.is_cancelled());
        assert!(plugin_path.is_dir());
        let cwd = std::env::current_dir().unwrap();
        for id in [
            "b969468e-7853-45b5-8c67-c6de21692501",
            "b969468e-7853-45b5-8c67-c6de21692502",
        ] {
            let session = ResumableCliSession::new(
                ResumableCliProvider::Claude,
                id.into(),
                cwd.clone(),
                ResumableCliEvidence::ProviderEvent,
            )
            .unwrap();
            sender
                .try_send(Ok(resumable_cli::CliIdentity {
                    session: session.clone(),
                    is_prompt: false,
                    prompt_started_at: Some(2501),
                }))
                .unwrap();
            cx.run_until_parked();
            assert_eq!(
                view.read_with(cx, |view, _| view.resumable_cli_binding.session().cloned()),
                Some(session.clone())
            );
            let saved = cx
                .update(|cx| TerminalDb::global(cx))
                .get_persistent_center_snapshot(&persistent_id, workspace_id)
                .unwrap()
                .unwrap();
            assert_eq!(saved.2.session(), Some(&session));
            assert!(!cancellation.is_cancelled());
            assert!(plugin_path.is_dir());
        }
        view.update(cx, |view, cx| view.forget_resumable_cli_session(cx))
            .await
            .unwrap();
        assert!(cancellation.is_cancelled());
        assert!(!plugin_path.exists());
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn codex_callback_errors_stay_dismissed_without_blocking_valid_identity(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let (project, workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, _terminal, view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        let workspace_id = cx
            .update(|cx| workspace::WorkspaceDb::global(cx))
            .next_id()
            .await
            .unwrap();
        view.update(cx, |view, _| view.workspace_id = Some(workspace_id));
        let (observer, mut sender) = resumable_cli::codex_identity_fixture();
        view.update(cx, |view, cx| view.observe_cli_identity(observer, cx));
        sender
            .try_send(Err(anyhow::anyhow!("Malformed callback")))
            .unwrap();
        cx.run_until_parked();
        let notifications = workspace.read_with(cx, |workspace, _| workspace.notification_ids());
        assert_eq!(notifications.len(), 1);
        workspace.update(cx, |workspace, cx| {
            workspace.dismiss_notification(&notifications[0], cx);
        });
        sender
            .try_send(Err(anyhow::anyhow!("Malformed callback")))
            .unwrap();
        cx.run_until_parked();
        assert!(workspace.read_with(cx, |workspace, _| workspace.notification_ids().is_empty()));

        let session = test_resumable_session("0199a4f5-1fe0-7000-8000-000000002499");
        sender
            .try_send(Ok(resumable_cli::CliIdentity {
                prompt_started_at: None,
                session: session.clone(),
                is_prompt: true,
            }))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.resumable_cli_binding.session().cloned()),
            Some(session)
        );
        sender
            .try_send(Err(anyhow::anyhow!("Malformed callback")))
            .unwrap();
        cx.run_until_parked();
        assert!(workspace.read_with(cx, |workspace, _| workspace.notification_ids().is_empty()));

        let (observer, mut sender) = resumable_cli::codex_identity_fixture();
        view.update(cx, |view, cx| view.observe_cli_identity(observer, cx));
        sender
            .try_send(Err(anyhow::anyhow!("Malformed callback")))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.notification_ids().len()),
            1
        );
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn codex_conflicting_owner_and_failed_switch_keep_the_previous_binding(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, _terminal, view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        let workspace_id = cx
            .update(|cx| workspace::WorkspaceDb::global(cx))
            .next_id()
            .await
            .unwrap();
        let first = test_resumable_session("0199a4f5-1fe0-7000-8000-000000002407");
        let second = test_resumable_session("0199a4f5-1fe0-7000-8000-000000002408");
        let claim = resumable_cli::try_claim_target(&first).unwrap();
        view.update(cx, |view, _| view.workspace_id = Some(workspace_id));
        view.update(cx, |view, cx| {
            view.set_resumable_cli_session_with_claim(first.clone(), claim, cx)
        })
        .await
        .unwrap();
        let (observer, mut sender) = resumable_cli::codex_identity_fixture();
        view.update(cx, |view, cx| view.observe_cli_identity(observer, cx));
        let other_owner = resumable_cli::try_claim_target(&second).unwrap();
        sender
            .try_send(Ok(resumable_cli::CliIdentity {
                session: second.clone(),
                is_prompt: true,
                prompt_started_at: Some(2401),
            }))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.resumable_cli_binding.session().cloned()),
            Some(first.clone())
        );
        assert!(resumable_cli::try_claim_target(&first).is_none());
        drop(other_owner);
        let database = cx.update(|cx| TerminalDb::global(cx));
        assert_eq!(
            database
                .compare_and_set_resumable_cli_session(
                    view.entity_id().as_u64(),
                    workspace_id,
                    1,
                    Some(first.clone())
                )
                .await
                .unwrap(),
            Some(2)
        );
        sender
            .try_send(Ok(resumable_cli::CliIdentity {
                session: second.clone(),
                is_prompt: true,
                prompt_started_at: Some(2402),
            }))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.resumable_cli_binding.session().cloned()),
            Some(first.clone())
        );
        assert_eq!(
            view.read_with(cx, |view, _| view.resumable_cli_binding.revision()),
            1
        );
        assert!(resumable_cli::try_claim_target(&first).is_none());
        assert!(resumable_cli::try_claim_target(&second).is_some());
        view.update(cx, |view, _| {
            view.capture_cancellation.take().unwrap().cancel();
            view.cli_identity_task = None;
        });
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn codex_pending_and_bound_restoration_launch_once_with_a_new_observer(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let workspace_id = cx
            .update(|cx| workspace::WorkspaceDb::global(cx))
            .next_id()
            .await
            .unwrap();
        for pending in [true, false] {
            let (_pane, original, view) =
                add_display_only_terminal(&project, window_handle, false, false, cx);
            let session = test_resumable_session(if pending {
                "0199a4f5-1fe0-7000-8000-000000002404"
            } else {
                "0199a4f5-1fe0-7000-8000-000000002405"
            });
            let cwd = session.resume_cwd().to_owned();
            view.update(cx, |view, _| view.workspace_id = Some(workspace_id));
            let save = view.update(cx, |view, cx| {
                if pending {
                    view.replace_resumable_cli_association(None, Some(cwd.clone()), cx)
                } else {
                    view.set_resumable_cli_session(session.clone(), cx)
                }
            });
            save.await.unwrap();
            let (old_observer, _old_sender) = resumable_cli::codex_identity_fixture();
            let old_cancellation = old_observer.cancellation.clone();
            view.update(cx, |view, cx| view.observe_cli_identity(old_observer, cx));
            let replacement = cx.new(|cx| {
                terminal::TerminalBuilder::new_display_only(
                    CursorShape::default(),
                    terminal::terminal_settings::AlternateScroll::On,
                    None,
                    0,
                    cx.background_executor(),
                    PathStyle::local(),
                )
                .subscribe(cx)
            });
            let (observer, mut sender) = resumable_cli::codex_identity_fixture();
            let cancellation = observer.cancellation.clone();
            let expected_command = if pending {
                resumable_cli::codex_new_command()
            } else {
                resumable_cli::codex_resume_command(&session).unwrap()
            };
            let claim = (!pending).then(|| resumable_cli::try_claim_target(&session).unwrap());
            window_handle
                .update(cx, |_, window, cx| {
                    view.update(cx, |view, cx| {
                        view.start_codex_terminal_with_preparation(
                            (!pending).then(|| session.clone()),
                            cwd,
                            claim,
                            Task::ready(Ok(Some((replacement.clone(), observer)))),
                            window,
                            cx,
                        )
                    });
                })
                .unwrap();
            cx.run_until_parked();
            assert_eq!(
                view.read_with(cx, |view, _| view.terminal.entity_id()),
                replacement.entity_id()
            );
            assert!(old_cancellation.is_cancelled());
            assert!(!cancellation.is_cancelled());
            assert!(
                original
                    .update(cx, |terminal, _| terminal.take_input_log())
                    .is_empty()
            );
            assert_eq!(
                replacement.update(cx, |terminal, _| terminal.take_input_log()),
                vec![expected_command]
            );
            assert_eq!(
                view.read_with(cx, |view, _| view.session_claim.is_none()),
                pending
            );
            sender
                .try_send(Ok(resumable_cli::CliIdentity {
                    session: session.clone(),
                    is_prompt: true,
                    prompt_started_at: Some(2401),
                }))
                .unwrap();
            cx.run_until_parked();
            assert_eq!(
                view.read_with(cx, |view, _| view.resumable_cli_binding.session().cloned()),
                Some(session.clone())
            );
            assert!(resumable_cli::try_claim_target(&session).is_none());
            assert!(
                replacement
                    .update(cx, |terminal, _| terminal.take_input_log())
                    .is_empty()
            );
            view.update(cx, |view, cx| view.forget_resumable_cli_session(cx))
                .await
                .unwrap();
            assert!(cancellation.is_cancelled());
        }
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn codex_failed_restore_keeps_binding_and_forget_cancels_preparation(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, original, view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        let workspace_id = cx
            .update(|cx| workspace::WorkspaceDb::global(cx))
            .next_id()
            .await
            .unwrap();
        let session = test_resumable_session("0199a4f5-1fe0-7000-8000-000000002406");
        view.update(cx, |view, _| view.workspace_id = Some(workspace_id));
        view.update(cx, |view, cx| {
            view.set_resumable_cli_session(session.clone(), cx)
        })
        .await
        .unwrap();
        let claim = resumable_cli::try_claim_target(&session).unwrap();
        window_handle
            .update(cx, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.start_codex_terminal_with_preparation(
                        Some(session.clone()),
                        session.resume_cwd().to_owned(),
                        Some(claim),
                        Task::ready(Err(anyhow::anyhow!("probe: unavailable saved directory"))),
                        window,
                        cx,
                    )
                });
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.resumable_cli_binding.session().cloned()),
            Some(session.clone())
        );
        assert_eq!(
            view.read_with(cx, |view, _| view.terminal.entity_id()),
            original.entity_id()
        );
        assert!(view.read_with(cx, |view, _| view.capture_cancellation.is_none()));
        assert!(
            original
                .update(cx, |terminal, _| terminal.take_input_log())
                .is_empty()
        );
        let claim = resumable_cli::try_claim_target(&session).unwrap();
        let (sender, receiver) = futures::channel::oneshot::channel();
        window_handle
            .update(cx, |_, window, cx| {
                view.update(cx, |view, cx| {
                    let preparation = cx.spawn(async move |_, _| receiver.await.unwrap());
                    view.start_codex_terminal_with_preparation(
                        Some(session.clone()),
                        session.resume_cwd().to_owned(),
                        Some(claim),
                        preparation,
                        window,
                        cx,
                    );
                });
            })
            .unwrap();
        cx.run_until_parked();
        view.update(cx, |view, cx| view.forget_resumable_cli_session(cx))
            .await
            .unwrap();
        cx.run_until_parked();
        assert!(sender.send(Ok(None)).is_err());
        assert!(view.read_with(cx, |view, _| view.resumable_cli_binding.session().is_none()));
        assert!(resumable_cli::try_claim_target(&session).is_some());
        assert!(
            original
                .update(cx, |terminal, _| terminal.take_input_log())
                .is_empty()
        );
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn superseded_identity_write_does_not_revive_a_claim(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, _terminal, view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        let workspace_id = cx
            .update(|cx| workspace::WorkspaceDb::global(cx))
            .next_id()
            .await
            .unwrap();
        let session = test_resumable_session("0199a4f5-1fe0-7000-8000-000000002403");
        let claim = resumable_cli::try_claim_target(&session).unwrap();
        let (capture, forget) = view.update(cx, |view, cx| {
            view.workspace_id = Some(workspace_id);
            let capture = view.set_resumable_cli_session_with_claim(session.clone(), claim, cx);
            let forget = view.forget_resumable_cli_session(cx);
            (capture, forget)
        });
        capture.await.unwrap();
        forget.await.unwrap();
        assert!(view.read_with(cx, |view, _| view.session_claim.is_none()));
        assert!(view.read_with(cx, |view, _| view.resumable_cli_binding.session().is_none()));
        assert!(resumable_cli::try_claim_target(&session).is_some());
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn failed_binding_persistence_does_not_promote_claim(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, _terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        let session = test_resumable_session("0199a4f5-1fe0-7000-8000-000000000002");
        let claim = resumable_cli::try_claim_target(&session).unwrap();

        let persistence = terminal_view.update(cx, |view, cx| {
            view.set_resumable_cli_session_with_claim(session.clone(), claim, cx)
        });
        let error = persistence.await.unwrap_err();
        assert!(error.to_string().contains("persisted workspace"));
        assert!(terminal_view.read_with(cx, |view, _| view.session_claim.is_none()));
        assert!(resumable_cli::try_claim_target(&session).is_some());
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn forget_during_resume_preflight_delivers_nothing_and_releases_claim(
        cx: &mut TestAppContext,
    ) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        let session = test_resumable_session("0199a4f5-1fe0-7000-8000-000000000003");
        let claim = resumable_cli::try_claim_target(&session).unwrap();
        let (sender, receiver) = futures::channel::oneshot::channel();

        terminal_view.update(cx, |view, cx| {
            view.resumable_cli_binding = ResumableCliBinding {
                pending_codex_cwd: None,
                revision: 4,
                session: Some(session.clone()),
            };
            view.resume_attempted = true;
            let contract = cx.spawn(async move |_, _| receiver.await.unwrap());
            view.start_ai_session_resume_with_contract(session.clone(), claim, contract, cx);
        });
        let forget = terminal_view.update(cx, |view, cx| view.forget_resumable_cli_session(cx));
        assert!(forget.await.is_err());
        let _ = sender.send(Ok(()));
        cx.run_until_parked();

        assert!(
            terminal
                .update(cx, |terminal, _| terminal.take_input_log())
                .is_empty()
        );
        assert!(terminal_view.read_with(cx, |view, _| view.session_claim.is_none()));
        assert!(resumable_cli::try_claim_target(&session).is_some());
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn pty_replacement_during_resume_preflight_delivers_to_neither_terminal(
        cx: &mut TestAppContext,
    ) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, old_terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        let session = test_resumable_session("0199a4f5-1fe0-7000-8000-000000000005");
        let claim = resumable_cli::try_claim_target(&session).unwrap();
        let (sender, receiver) = futures::channel::oneshot::channel();

        terminal_view.update(cx, |view, cx| {
            view.resumable_cli_binding = ResumableCliBinding {
                pending_codex_cwd: None,
                revision: 6,
                session: Some(session.clone()),
            };
            view.resume_attempted = true;
            let contract = cx.spawn(async move |_, _| receiver.await.unwrap());
            view.start_ai_session_resume_with_contract(session.clone(), claim, contract, cx);
        });
        let new_terminal = cx.new(|cx| {
            terminal::TerminalBuilder::new_display_only(
                CursorShape::default(),
                terminal::terminal_settings::AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        window_handle
            .update(cx, |_, window, cx| {
                terminal_view.update(cx, |view, cx| {
                    view.replace_terminal(new_terminal.clone(), window, cx)
                });
            })
            .unwrap();
        let _ = sender.send(Ok(()));
        cx.run_until_parked();

        assert!(
            old_terminal
                .update(cx, |terminal, _| terminal.take_input_log())
                .is_empty()
        );
        assert!(
            new_terminal
                .update(cx, |terminal, _| terminal.take_input_log())
                .is_empty()
        );
        assert!(resumable_cli::try_claim_target(&session).is_some());
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn closing_view_during_resume_preflight_drops_terminal_and_claim(
        cx: &mut TestAppContext,
    ) {
        let (project, workspace, window_handle) = init_test_with_window(cx).await;
        let (terminal, terminal_view) = window_handle
            .update(cx, |_, window, cx| {
                let terminal = cx.new(|cx| {
                    terminal::TerminalBuilder::new_display_only(
                        CursorShape::default(),
                        terminal::terminal_settings::AlternateScroll::On,
                        None,
                        0,
                        cx.background_executor(),
                        PathStyle::local(),
                    )
                    .subscribe(cx)
                });
                let terminal_view = cx.new(|cx| {
                    TerminalView::new(
                        terminal.clone(),
                        workspace.downgrade(),
                        None,
                        project.downgrade(),
                        window,
                        cx,
                    )
                });
                (terminal.downgrade(), terminal_view)
            })
            .unwrap();
        let session = test_resumable_session("0199a4f5-1fe0-7000-8000-000000000006");
        let claim = resumable_cli::try_claim_target(&session).unwrap();
        let (_sender, receiver) = futures::channel::oneshot::channel();

        terminal_view.update(cx, |view, cx| {
            view.resumable_cli_binding = ResumableCliBinding {
                pending_codex_cwd: None,
                revision: 8,
                session: Some(session.clone()),
            };
            view.resume_attempted = true;
            let contract = cx.spawn(async move |_, _| receiver.await.unwrap());
            view.start_ai_session_resume_with_contract(session.clone(), claim, contract, cx);
        });
        drop(terminal_view);
        cx.run_until_parked();

        assert!(terminal.upgrade().is_none());
        assert!(resumable_cli::try_claim_target(&session).is_some());
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn newer_resume_attempt_delivers_once_and_owns_claim(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        let session = test_resumable_session("0199a4f5-1fe0-7000-8000-000000000004");
        let old_claim = resumable_cli::try_claim_target(&session).unwrap();
        let new_claim = resumable_cli::replace_target_claim(&session);
        let (sender, receiver) = futures::channel::oneshot::channel();

        terminal_view.update(cx, |view, cx| {
            view.resumable_cli_binding = ResumableCliBinding {
                pending_codex_cwd: None,
                revision: 5,
                session: Some(session.clone()),
            };
            view.resume_attempted = true;
            let delayed_contract = cx.spawn(async move |_, _| receiver.await.unwrap());
            view.start_ai_session_resume_with_contract(
                session.clone(),
                old_claim,
                delayed_contract,
                cx,
            );
            view.start_ai_session_resume_with_contract(
                session.clone(),
                new_claim,
                Task::ready(Ok(())),
                cx,
            );
        });
        cx.run_until_parked();
        let _ = sender.send(Ok(()));
        cx.run_until_parked();

        assert_eq!(
            terminal
                .update(cx, |terminal, _| terminal.take_input_log())
                .len(),
            1
        );
        assert!(terminal_view.read_with(cx, |view, _| view.session_claim.is_some()));
        terminal_view.update(cx, |view, _| view.session_claim = None);
        assert!(resumable_cli::try_claim_target(&session).is_some());
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn failed_resume_delivery_releases_local_attempt_claim(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        let session = test_resumable_session("0199a4f5-1fe0-7000-8000-000000000007");
        let claim = resumable_cli::try_claim_target(&session).unwrap();

        terminal_view.update(cx, |view, cx| {
            view.resumable_cli_binding = ResumableCliBinding {
                pending_codex_cwd: None,
                revision: 9,
                session: Some(session.clone()),
            };
            view.start_ai_session_resume_with_contract(
                session.clone(),
                claim,
                Task::ready(Ok(())),
                cx,
            );
        });
        terminal.update(cx, |terminal, _| terminal.input(b"user input".to_vec()));
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(5));
        cx.run_until_parked();

        assert_eq!(
            terminal.update(cx, |terminal, _| terminal.take_input_log()),
            vec![b"user input".to_vec()]
        );
        assert!(terminal_view.read_with(cx, |view, _| view.session_claim.is_none()));
        assert!(resumable_cli::try_claim_target(&session).is_some());
    }

    #[gpui::test]
    async fn replacing_output_resets_view_scrolling_hover_and_search(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, false, true, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        terminal.update(&mut cx, |terminal, cx| {
            terminal.write_raw_output(b"\x1b[1 q", cx);
        });
        cx.run_until_parked();
        assert!(terminal_view.read_with(&cx, |view, _| view.blinking_terminal_enabled));
        cx.update(|_, cx| {
            terminal_view.update(cx, |view, _| {
                view.scroll_top = px(25.);
                view.scroll_handle.future_display_offset.set(Some(3));
                view.hover = Some(HoverTarget {
                    tooltip: "stale".into(),
                    hovered_word: HoveredWord {
                        word: "stale".into(),
                        word_match: Range::new(
                            terminal::Point::new(0, 0),
                            terminal::Point::new(0, 5),
                        ),
                        id: 0,
                    },
                });
            });
            terminal.update(cx, |terminal, cx| {
                terminal.write_raw_output(b"old", cx);
                terminal.matches.push(Range::new(
                    terminal::Point::new(0, 0),
                    terminal::Point::new(0, 3),
                ));
                terminal
                    .replace_display_output(b"new", cx)
                    .expect("replace");
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            let view = terminal_view.read(cx);
            assert_eq!(view.scroll_top, Pixels::ZERO);
            assert!(view.scroll_handle.future_display_offset.get().is_none());
            assert!(view.hover.is_none());
            assert!(view.is_read_only());
            assert!(!view.blinking_terminal_enabled);
            assert!(terminal.read(cx).matches.is_empty());
        });
    }

    #[gpui::test]
    async fn edit_menu_copy_and_paste_are_available_when_terminal_is_focused(
        cx: &mut TestAppContext,
    ) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, _terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);

        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, cx| {
            let _ = window.draw(cx);
            assert!(window.is_action_available(&editor::actions::Copy, cx));
            assert!(window.is_action_available(&editor::actions::Paste, cx));

            cx.write_to_clipboard(gpui::ClipboardItem::new_string("foo".to_string()));
            terminal.update(cx, |terminal, _| terminal.take_input_log());
            window.dispatch_action(Box::new(editor::actions::Paste), cx);
        });
        cx.run_until_parked();

        cx.update(|_, cx| {
            let input_log = terminal.update(cx, |terminal, _| terminal.take_input_log());
            assert_eq!(input_log, vec![b"foo".to_vec()]);
        });
    }

    #[gpui::test]
    async fn read_only_blocks_input_actions_and_preserves_output(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let rerun_count = Rc::new(std::cell::Cell::new(0));
        cx.update(|cx| {
            cx.on_action({
                let rerun_count = rerun_count.clone();
                move |_: &zed_actions::Rerun, _| rerun_count.set(rerun_count.get() + 1)
            });
        });
        let (pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, true, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);

        for output in [
            b"provider output".as_slice(),
            b"\x1b[?1049hprovider alternate-screen output".as_slice(),
        ] {
            let content = cx.update(|window, cx| {
                terminal.update(cx, |terminal, cx| {
                    terminal.write_output(output, cx);
                    terminal.sync(window, cx);
                    terminal.get_content()
                })
            });
            cx.simulate_keystrokes("a enter ctrl-v shift-up");
            cx.update(|_, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string("clipboard input".into()));
            });

            let actions: Vec<Box<dyn Action>> = vec![
                Box::new(SendText("direct input".into())),
                Box::new(SendKeystroke("enter".into())),
                Box::new(Paste),
                Box::new(editor::actions::Paste),
                Box::new(PasteText),
                Box::new(Clear),
                Box::new(ShowCharacterPalette),
                Box::new(RerunTask),
            ];
            for action in actions {
                cx.update(|window, cx| window.dispatch_action(action, cx));
                cx.run_until_parked();
            }
            cx.update(|window, cx| {
                let paths = ExternalPaths(vec![PathBuf::from(util::path!("/root.txt"))].into());
                assert!(!pane.update(cx, |pane, cx| {
                    terminal_view.update(cx, |view, cx| view.handle_drop(pane, &paths, window, cx))
                }));
                terminal_view.update(cx, |view, cx| {
                    view.add_paths_to_terminal(paths.paths(), window, cx);
                });
                terminal.update(cx, |terminal, cx| {
                    terminal.sync(window, cx);
                    assert!(terminal.take_input_log().is_empty());
                    assert!(terminal.take_pty_write_log().is_empty());
                    assert_eq!(terminal.get_content(), content);
                });
            });
        }
        assert_eq!(rerun_count.get(), 0);
    }

    #[gpui::test]
    async fn read_only_preserves_vi_navigation_without_enabling_input(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, _terminal_view) =
            add_display_only_terminal(&project, window_handle, true, true, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"first line\nsecond line\n", cx);
                terminal.sync(window, cx);
            });
            window.dispatch_action(Box::new(ToggleViMode), cx);
        });
        cx.run_until_parked();
        let cursor = terminal.read_with(&cx, |terminal, _| {
            assert!(terminal.vi_mode_enabled());
            terminal.last_content.cursor.point
        });
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(SendKeystroke("k".into())), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            terminal.read_with(&cx, |terminal, _| terminal.last_content.cursor.point.line),
            cursor.line - 1,
        );
        cx.simulate_keystrokes("v l");
        assert!(
            terminal
                .read_with(&cx, |terminal, _| terminal
                    .last_content
                    .selection_text
                    .clone())
                .is_some_and(|text| !text.is_empty()),
        );
        cx.simulate_keystrokes("y");
        assert!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
                .is_some_and(|text| !text.is_empty()),
        );
        cx.simulate_keystrokes("i");
        assert!(!terminal.read_with(&cx, |terminal, _| terminal.vi_mode_enabled()));
        cx.simulate_keystrokes("a enter");
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(SendKeystroke("enter".into())), cx);
        });
        cx.run_until_parked();
        assert!(
            terminal
                .update(&mut cx, |terminal, _| terminal.take_input_log())
                .is_empty()
        );
    }

    #[gpui::test]
    async fn read_only_mouse_selection_and_focus_do_not_report_input(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, _terminal_view) =
            add_display_only_terminal(&project, window_handle, true, true, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"\x1b[?1003h\x1b[?1006h\x1b[?1004hhello world\n", cx);
                terminal.sync(window, cx);
                terminal.take_pty_write_log();
            });
            window.blur(cx);
        });
        cx.run_until_parked();
        let bounds = terminal.read_with(&cx, |terminal, _| terminal.last_content.terminal_bounds);
        let start =
            bounds.bounds.origin + gpui::point(bounds.cell_width * 0.1, bounds.line_height * 0.5);
        let end =
            bounds.bounds.origin + gpui::point(bounds.cell_width * 5.1, bounds.line_height * 0.5);
        cx.simulate_mouse_move(start, None, gpui::Modifiers::default());
        cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(end, Some(MouseButton::Left), gpui::Modifiers::default());
        cx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());
        cx.update(|window, cx| window.dispatch_action(Box::new(Copy), cx));
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
            Some("hello".into()),
        );
        cx.update(|window, cx| window.blur(cx));
        cx.run_until_parked();
        assert!(
            terminal
                .update(&mut cx, |terminal, _| terminal.take_pty_write_log())
                .is_empty()
        );
    }

    #[gpui::test]
    async fn shift_up_scrolls_history_in_normal_screen(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let (_pane, terminal, _terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);

        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let output = (0..200)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(output.as_bytes(), cx);
                terminal.sync(window, cx);
            });
        });
        terminal.read_with(&cx, |terminal, _| {
            assert!(!terminal.last_content.mode.contains(Modes::ALT_SCREEN));
            assert_eq!(terminal.last_content.display_offset, 0);
        });

        cx.simulate_keystrokes("shift-up");
        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| terminal.sync(window, cx));
        });

        assert_eq!(
            terminal.read_with(&cx, |terminal, _| terminal.last_content.display_offset),
            1,
            "shift-up should scroll terminal history in the normal screen",
        );
        assert!(
            terminal
                .update(&mut cx, |terminal, _| terminal.take_input_log())
                .is_empty(),
            "shift-up in the normal screen should not be forwarded to the shell",
        );
    }

    #[gpui::test]
    async fn shift_up_is_forwarded_to_program_in_alt_screen(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let (_pane, terminal, _terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);

        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(ENTER_ALT_SCREEN, cx);
                terminal.sync(window, cx);
            });
        });
        terminal.read_with(&cx, |terminal, _| {
            assert!(terminal.last_content.mode.contains(Modes::ALT_SCREEN));
        });

        cx.simulate_keystrokes("shift-up");
        assert_eq!(
            terminal.update(&mut cx, |terminal, _| terminal.take_input_log()),
            vec![SHIFT_UP_ESCAPE.to_vec()],
            "shift-up should be forwarded to the program in the alternate screen",
        );
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    async fn ctrl_q_is_forwarded_to_terminal_not_quit(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let (_pane, terminal, _terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);

        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        cx.simulate_keystrokes("ctrl-q");
        assert_eq!(
            terminal.update(&mut cx, |terminal, _| terminal.take_input_log()),
            vec![vec![0x11]],
            "ctrl-q in a focused terminal should send 0x11 to the PTY, not trigger zed::Quit",
        );
    }

    #[gpui::test]
    async fn altgr_character_input_is_not_swallowed_as_meta_sequence(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.terminal.get_or_insert_default().option_as_meta = Some(true);
                });
            });
        });
        let (_pane, terminal, _terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);

        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let mut altgr_a = Keystroke::parse("ctrl-alt-a").unwrap();
        altgr_a.key_char = Some("ą".to_string());

        let propagate = cx.update(|window, cx| {
            window
                .dispatch_event(
                    gpui::PlatformInput::KeyDown(KeyDownEvent {
                        keystroke: altgr_a.clone(),
                        is_held: false,
                        prefer_character_input: true,
                    }),
                    cx,
                )
                .propagate
        });
        assert!(
            propagate,
            "a keystroke that produced a character must propagate so the platform can commit the text",
        );
        assert_eq!(
            terminal.update(&mut cx, |terminal, _| terminal.take_input_log()),
            Vec::<Vec<u8>>::new(),
            "AltGr-produced characters must not be sent as meta escape sequences",
        );

        let propagate = cx.update(|window, cx| {
            window
                .dispatch_event(
                    gpui::PlatformInput::KeyDown(KeyDownEvent {
                        keystroke: altgr_a,
                        is_held: false,
                        prefer_character_input: false,
                    }),
                    cx,
                )
                .propagate
        });
        assert!(!propagate);
        assert_eq!(
            terminal.update(&mut cx, |terminal, _| terminal.take_input_log()),
            vec![b"\x1b\x01".to_vec()],
            "ctrl-alt-a without character input preference is still sent as meta ctrl-a",
        );
    }

    // Working directory calculation tests

    // No Worktrees in project -> home_dir()
    #[gpui::test]
    async fn no_worktree(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            //Make sure environment is as expected
            assert!(active_entry.is_none());
            assert!(workspace.worktrees(cx).next().is_none());

            let res = default_working_directory(workspace, cx);
            assert_eq!(res, dirs::home_dir());
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, None);
        });
    }

    #[gpui::test]
    async fn remote_no_worktree_uses_remote_shell_default_cwd(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let (_project, workspace) = init_remote_test(cx, server_cx).await;

        cx.read(|cx| {
            let workspace = workspace.read(cx);

            assert!(workspace.project().read(cx).is_remote());
            assert!(workspace.worktrees(cx).next().is_none());
            assert_eq!(default_working_directory(workspace, cx), None);
        });
    }

    // No active entry, but a worktree, worktree is a file -> parent directory
    #[gpui::test]
    async fn no_active_entry_worktree_is_file(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;

        create_file_wt(project.clone(), "/root.txt", cx).await;
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            //Make sure environment is as expected
            assert!(active_entry.is_none());
            assert!(workspace.worktrees(cx).next().is_some());

            let res = default_working_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/").to_path_buf()));
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/").to_path_buf()));
        });
    }

    // No active entry, but a worktree, worktree is a folder -> worktree_folder
    #[gpui::test]
    async fn no_active_entry_worktree_is_dir(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;

        let (_wt, _entry) = create_folder_wt(project.clone(), "/root/", cx).await;
        cx.update(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            assert!(active_entry.is_none());
            assert!(workspace.worktrees(cx).next().is_some());

            let res = default_working_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/root/").to_path_buf()));
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/root/").to_path_buf()));
        });
    }

    // Active entry with a work tree, worktree is a file -> worktree_folder()
    #[gpui::test]
    async fn active_entry_worktree_is_file(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;

        let (_wt, _entry) = create_folder_wt(project.clone(), "/root1/", cx).await;
        let (wt2, entry2) = create_file_wt(project.clone(), "/root2.txt", cx).await;
        insert_active_entry_for(wt2, entry2, project.clone(), cx);

        cx.update(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            assert!(active_entry.is_some());

            let res = default_working_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/root1/").to_path_buf()));
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/root1/").to_path_buf()));
        });
    }

    // Active entry, with a worktree, worktree is a folder -> worktree_folder
    #[gpui::test]
    async fn active_entry_worktree_is_dir(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;

        let (_wt, _entry) = create_folder_wt(project.clone(), "/root1/", cx).await;
        let (wt2, entry2) = create_folder_wt(project.clone(), "/root2/", cx).await;
        insert_active_entry_for(wt2, entry2, project.clone(), cx);

        cx.update(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            assert!(active_entry.is_some());

            let res = default_working_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/root2/").to_path_buf()));
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/root1/").to_path_buf()));
        });
    }

    // active_entry_directory: No active entry -> returns None (used by CurrentFileDirectory)
    #[gpui::test]
    async fn active_entry_directory_no_active_entry(cx: &mut TestAppContext) {
        let (project, _workspace) = init_test(cx).await;

        let (_wt, _entry) = create_folder_wt(project.clone(), "/root/", cx).await;

        cx.update(|cx| {
            assert!(project.read(cx).active_entry().is_none());

            let res = project.read(cx).active_entry_directory(cx);
            assert_eq!(res, None);
        });
    }

    // active_entry_directory: Active entry is file -> returns parent directory (used by CurrentFileDirectory)
    #[gpui::test]
    async fn active_entry_directory_active_file(cx: &mut TestAppContext) {
        let (project, _workspace) = init_test(cx).await;

        let (wt, _entry) = create_folder_wt(project.clone(), "/root/", cx).await;
        let entry = create_file_in_worktree(wt.clone(), "src/main.rs", cx).await;
        insert_active_entry_for(wt, entry, project.clone(), cx);

        cx.update(|cx| {
            let res = project.read(cx).active_entry_directory(cx);
            assert_eq!(res, Some(Path::new("/root/src").to_path_buf()));
        });
    }

    // active_entry_directory: Active entry is directory -> returns that directory (used by CurrentFileDirectory)
    #[gpui::test]
    async fn active_entry_directory_active_dir(cx: &mut TestAppContext) {
        let (project, _workspace) = init_test(cx).await;

        let (wt, entry) = create_folder_wt(project.clone(), "/root/", cx).await;
        insert_active_entry_for(wt, entry, project.clone(), cx);

        cx.update(|cx| {
            let res = project.read(cx).active_entry_directory(cx);
            assert_eq!(res, Some(Path::new("/root/").to_path_buf()));
        });
    }

    /// Creates a worktree with 1 file: /root.txt
    pub async fn init_test(cx: &mut TestAppContext) -> (Entity<Project>, Entity<Workspace>) {
        let (project, workspace, _) = init_test_with_window(cx).await;
        (project, workspace)
    }

    fn load_default_keymap(cx: &mut App) {
        cx.bind_keys(
            settings::KeymapFile::load_asset_allow_partial_failure(
                settings::DEFAULT_KEYMAP_PATH,
                cx,
            )
            .unwrap(),
        );
    }

    pub(super) fn add_display_only_terminal(
        project: &Entity<Project>,
        window_handle: gpui::WindowHandle<MultiWorkspace>,
        focus: bool,
        read_only: bool,
        cx: &mut TestAppContext,
    ) -> (Entity<Pane>, Entity<Terminal>, Entity<TerminalView>) {
        let project = project.clone();
        window_handle
            .update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().clone();
                let active_pane = workspace.read(cx).active_pane().clone();

                let terminal = cx.new(|cx| {
                    terminal::TerminalBuilder::new_display_only(
                        CursorShape::default(),
                        terminal::terminal_settings::AlternateScroll::On,
                        None,
                        0,
                        cx.background_executor(),
                        PathStyle::local(),
                    )
                    .subscribe(cx)
                });
                let terminal_view = cx.new(|cx| {
                    TerminalView::new(
                        terminal.clone(),
                        workspace.downgrade(),
                        None,
                        project.downgrade(),
                        window,
                        cx,
                    )
                    .with_read_only(read_only)
                });

                active_pane.update(cx, |pane, cx| {
                    pane.add_item(
                        Box::new(terminal_view.clone()),
                        true,
                        false,
                        None,
                        window,
                        cx,
                    );
                });

                if focus {
                    let focus_handle = terminal_view.read(cx).focus_handle.clone();
                    focus_handle.focus(window, cx);
                }

                (active_pane, terminal, terminal_view)
            })
            .unwrap()
    }

    fn add_terminal_with_launch_directory(
        project: &Entity<Project>,
        window_handle: gpui::WindowHandle<MultiWorkspace>,
        working_directory: PathBuf,
        cx: &mut TestAppContext,
    ) -> (Entity<Pane>, Entity<Terminal>, Entity<TerminalView>) {
        window_handle
            .update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().clone();
                let pane = workspace.read(cx).active_pane().clone();
                let terminal = cx.new(|cx| {
                    let builder = terminal::TerminalBuilder::new_display_only(
                        CursorShape::default(),
                        terminal::terminal_settings::AlternateScroll::On,
                        None,
                        0,
                        cx.background_executor(),
                        PathStyle::local(),
                    )
                    .with_initial_working_directory(Some(working_directory.clone()));
                    let mut terminal = builder.subscribe(cx);
                    terminal.set_working_directory_for_test(Some(working_directory));
                    terminal
                });
                let view = cx.new(|cx| {
                    TerminalView::new(
                        terminal.clone(),
                        workspace.downgrade(),
                        None,
                        project.downgrade(),
                        window,
                        cx,
                    )
                });
                pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(view.clone()), true, false, None, window, cx)
                });
                (pane, terminal, view)
            })
            .unwrap()
    }

    #[gpui::test]
    async fn terminal_project_association_stays_at_creation_root(cx: &mut TestAppContext) {
        let (project, workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.workspace.window_title_format =
                        Some("${projectName}${separator}${branch}".into())
                })
            });
        });
        let root_a = PathBuf::from(util::path!("/project-a"));
        let root_b = PathBuf::from(util::path!("/project-b"));
        let fs = project.read_with(cx, |project, _| project.fs().clone());
        for (root, branch) in [(&root_a, "branch-a"), (&root_b, "branch-b")] {
            fs.as_fake()
                .insert_tree(root.join(".git").as_path(), serde_json::json!({}))
                .await;
            fs.as_fake()
                .set_branch_name(&root.join(".git"), Some(branch));
        }
        let (worktree_a, entry_a) = create_folder_wt(project.clone(), &root_a, cx).await;
        let (worktree_b, _) = create_folder_wt(project.clone(), &root_b, cx).await;
        let file_b = create_file_in_worktree(worktree_b.clone(), "file.rs", cx).await;
        cx.run_until_parked();
        let id_a = worktree_a.read_with(cx, |worktree, _| worktree.id());
        let id_b = worktree_b.read_with(cx, |worktree, _| worktree.id());
        let expected = ProjectPath {
            worktree_id: id_a,
            path: RelPath::empty_arc(),
        };
        let (pane, terminal, view) =
            add_terminal_with_launch_directory(&project, window_handle, root_a.clone(), cx);
        assert_eq!(
            view.read_with(cx, |view, cx| view.active_project_path(cx)),
            Some(expected.clone())
        );
        let file_item = window_handle
            .update(cx, |_, window, cx| {
                let project_item = cx.new(|_| TestProjectItem {
                    entry_id: Some(file_b.id),
                    project_path: Some(ProjectPath {
                        worktree_id: id_b,
                        path: file_b.path.clone(),
                    }),
                    is_dirty: false,
                });
                let item = cx.new(|cx| TestItem::new(cx).with_project_items(&[project_item]));
                pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(item.clone()), true, true, None, window, cx)
                });
                item
            })
            .unwrap();
        let git_store = project.read_with(cx, |project, _| project.git_store().clone());
        for live_directory in [
            &root_b,
            &PathBuf::from(util::path!("/outside-projects")),
            &root_a.join("src"),
        ] {
            terminal.update(cx, |terminal, _| {
                terminal.set_working_directory_for_test(Some(live_directory.clone()))
            });
            window_handle
                .update(cx, |_, window, cx| {
                    pane.update(cx, |pane, cx| {
                        let index = pane
                            .items()
                            .position(|item| item.item_id() == file_item.entity_id())
                            .unwrap();
                        pane.activate_item(index, true, true, window, cx);
                    });
                    git_store.update(cx, |store, cx| store.set_active_repo_for_worktree(id_b, cx));
                    assert_eq!(
                        git_store
                            .read(cx)
                            .active_repository()
                            .unwrap()
                            .read(cx)
                            .work_directory_abs_path
                            .as_ref(),
                        root_b.as_path()
                    );
                    pane.update(cx, |pane, cx| {
                        let index = pane
                            .items()
                            .position(|item| item.item_id() == view.entity_id())
                            .unwrap();
                        pane.activate_item(index, true, true, window, cx);
                    });
                    window.focus(&view.read(cx).focus_handle(cx), cx);
                })
                .unwrap();
            cx.run_until_parked();
            assert_eq!(
                view.read_with(cx, |view, cx| view.active_project_path(cx)),
                Some(expected.clone())
            );
            assert_eq!(
                project.read_with(cx, |project, _| project.active_entry()),
                Some(entry_a.id)
            );
            assert_eq!(
                git_store.read_with(cx, |store, cx| store
                    .active_repository()
                    .unwrap()
                    .read(cx)
                    .work_directory_abs_path
                    .to_path_buf()),
                root_a
            );
            let title = gpui::VisualTestContext::from_window(window_handle.into(), cx)
                .window_title()
                .unwrap();
            assert!(title.contains("branch-a"), "unexpected title: {title}");
            assert!(!title.contains("branch-b"));
            assert!(!view.read_with(cx, |view, cx| view.affects_workspace_theme(cx)));
            workspace.read_with(cx, |workspace, cx| {
                assert_eq!(
                    default_working_directory(workspace, cx),
                    Some(root_a.clone())
                );
                let history = workspace.recent_navigation_history(None, cx);
                assert!(history.iter().all(|(path, _)| !path.path.is_empty()));
                assert!(
                    history
                        .iter()
                        .any(|(path, _)| path.worktree_id == id_b && path.path == file_b.path)
                );
            });
        }
        project.update(cx, |project, cx| project.remove_worktree(id_a, cx));
        terminal.update(cx, |terminal, _| {
            terminal.set_working_directory_for_test(Some(root_b.clone()))
        });
        assert_eq!(
            view.read_with(cx, |view, cx| view.active_project_path(cx)),
            None
        );
    }

    /// Creates a worktree with 1 file /root.txt and returns the project, workspace, and window handle.
    pub(super) async fn init_test_with_window(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Project>,
        Entity<Workspace>,
        gpui::WindowHandle<MultiWorkspace>,
    ) {
        let params = cx.update(AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let project = Project::test(params.fs.clone(), [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();

        (project, workspace, window_handle)
    }

    async fn init_remote_test(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<Workspace>) {
        cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
        server_cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });

        let params = cx.update(AppState::test);
        let (opts, server_session, connect_guard) = RemoteClient::fake_server(cx, server_cx);
        let ping_handler = server_cx.new(|_| ());
        server_session.add_request_handler::<rpc::proto::Ping, _, _, _>(
            ping_handler.downgrade(),
            |_entity, _envelope, _cx| async { Ok(rpc::proto::Ack {}) },
        );
        drop(connect_guard);

        let remote_client = RemoteClient::connect_mock(opts, cx).await;
        let project = cx.update(|cx| {
            Project::remote(
                remote_client,
                params.client.clone(),
                params.node_runtime.clone(),
                params.user_store.clone(),
                params.languages.clone(),
                params.fs.clone(),
                false,
                cx,
            )
        });

        let window_handle = cx.add_window({
            let params = params.clone();
            let project_for_workspace = project.clone();
            move |window, cx| {
                window.activate_window();
                let workspace = cx.new(|cx| {
                    Workspace::new(
                        None,
                        project_for_workspace.clone(),
                        params.clone(),
                        window,
                        cx,
                    )
                });
                MultiWorkspace::new(workspace, window, cx)
            }
        });
        let workspace = window_handle
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();

        (project, workspace)
    }

    /// Creates a file in the given worktree and returns its entry.
    async fn create_file_in_worktree(
        worktree: Entity<Worktree>,
        relative_path: impl AsRef<Path>,
        cx: &mut TestAppContext,
    ) -> Entry {
        cx.update(|cx| {
            worktree.update(cx, |worktree, cx| {
                worktree.create_entry(
                    RelPath::new(relative_path.as_ref(), PathStyle::local())
                        .unwrap()
                        .as_ref()
                        .into(),
                    false,
                    None,
                    cx,
                )
            })
        })
        .await
        .unwrap()
        .into_included()
        .unwrap()
    }

    /// Creates a worktree with 1 folder: /root{suffix}/
    async fn create_folder_wt(
        project: Entity<Project>,
        path: impl AsRef<Path>,
        cx: &mut TestAppContext,
    ) -> (Entity<Worktree>, Entry) {
        create_wt(project, true, path, cx).await
    }

    /// Creates a worktree with 1 file: /root{suffix}.txt
    async fn create_file_wt(
        project: Entity<Project>,
        path: impl AsRef<Path>,
        cx: &mut TestAppContext,
    ) -> (Entity<Worktree>, Entry) {
        create_wt(project, false, path, cx).await
    }

    async fn create_wt(
        project: Entity<Project>,
        is_dir: bool,
        path: impl AsRef<Path>,
        cx: &mut TestAppContext,
    ) -> (Entity<Worktree>, Entry) {
        let (wt, _) = project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(path, true, cx)
            })
            .await
            .unwrap();

        let entry = cx
            .update(|cx| {
                wt.update(cx, |wt, cx| {
                    wt.create_entry(RelPath::empty_arc(), is_dir, None, cx)
                })
            })
            .await
            .unwrap()
            .into_included()
            .unwrap();

        (wt, entry)
    }

    pub fn insert_active_entry_for(
        wt: Entity<Worktree>,
        entry: Entry,
        project: Entity<Project>,
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let p = ProjectPath {
                worktree_id: wt.read(cx).id(),
                path: entry.path,
            };
            project.update(cx, |project, cx| project.set_active_path(Some(p), cx));
        });
    }

    // Terminal drag/drop test

    #[gpui::test]
    async fn test_handle_drop_writes_paths_for_all_drop_types(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;

        let (worktree, _) = create_folder_wt(project.clone(), "/root/", cx).await;
        let first_entry = create_file_in_worktree(worktree.clone(), "first.txt", cx).await;
        let second_entry = create_file_in_worktree(worktree.clone(), "second.txt", cx).await;

        let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
        let first_path = project
            .read_with(cx, |project, cx| {
                project.absolute_path(
                    &ProjectPath {
                        worktree_id,
                        path: first_entry.path.clone(),
                    },
                    cx,
                )
            })
            .unwrap();
        let second_path = project
            .read_with(cx, |project, cx| {
                project.absolute_path(
                    &ProjectPath {
                        worktree_id,
                        path: second_entry.path.clone(),
                    },
                    cx,
                )
            })
            .unwrap();

        let (active_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);

        let tab_item = window_handle
            .update(cx, |_, window, cx| {
                let tab_project_item = cx.new(|_| TestProjectItem {
                    entry_id: Some(second_entry.id),
                    project_path: Some(ProjectPath {
                        worktree_id,
                        path: second_entry.path.clone(),
                    }),
                    is_dirty: false,
                });
                let tab_item =
                    cx.new(|cx| TestItem::new(cx).with_project_items(&[tab_project_item]));
                active_pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(tab_item.clone()), true, false, None, window, cx);
                });
                tab_item
            })
            .unwrap();

        cx.run_until_parked();

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().clone();
                let terminal_view_index =
                    active_pane.read(cx).index_for_item(&terminal_view).unwrap();
                let dragged_tab_index = active_pane.read(cx).index_for_item(&tab_item).unwrap();

                assert!(
                    workspace.read(cx).pane_for(&terminal_view).is_some(),
                    "terminal view not registered with workspace after run_until_parked"
                );

                // Dragging an external file should write its path to the terminal
                let external_paths = ExternalPaths(vec![first_path.clone()].into());
                assert_drop_writes_to_terminal(
                    &active_pane,
                    terminal_view_index,
                    &terminal,
                    &external_paths,
                    &expected_drop_text(std::slice::from_ref(&first_path)),
                    window,
                    cx,
                );

                // Dragging a tab should write the path of the tab's item to the terminal
                let dragged_tab = DraggedTab {
                    pane: active_pane.clone(),
                    item: Box::new(tab_item.clone()),
                    ix: dragged_tab_index,
                    detail: 0,
                    is_active: false,
                };
                assert_drop_writes_to_terminal(
                    &active_pane,
                    terminal_view_index,
                    &terminal,
                    &dragged_tab,
                    &expected_drop_text(std::slice::from_ref(&second_path)),
                    window,
                    cx,
                );

                // Dragging multiple selections should write both paths to the terminal
                let dragged_selection = DraggedSelection {
                    active_selection: SelectedEntry {
                        worktree_id,
                        entry_id: first_entry.id,
                    },
                    marked_selections: Arc::from([
                        SelectedEntry {
                            worktree_id,
                            entry_id: first_entry.id,
                        },
                        SelectedEntry {
                            worktree_id,
                            entry_id: second_entry.id,
                        },
                    ]),
                };
                assert_drop_writes_to_terminal(
                    &active_pane,
                    terminal_view_index,
                    &terminal,
                    &dragged_selection,
                    &expected_drop_text(&[first_path.clone(), second_path.clone()]),
                    window,
                    cx,
                );

                // Dropping a project entry should write the entry's path to the terminal
                let dropped_entry_id = first_entry.id;
                assert_drop_writes_to_terminal(
                    &active_pane,
                    terminal_view_index,
                    &terminal,
                    &dropped_entry_id,
                    &expected_drop_text(&[first_path]),
                    window,
                    cx,
                );
            })
            .unwrap();
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn capture_error_uses_current_title_and_pane_after_a_move(cx: &mut TestAppContext) {
        let (project, workspace, window_handle) = init_test_with_window(cx).await;
        let (original_pane, _terminal, view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        let (_pane, _terminal, other) =
            add_display_only_terminal(&project, window_handle, false, false, cx);
        let pending_view = view.downgrade();
        view.update(cx, |view, cx| {
            view.set_custom_title(Some("before".into()), cx)
        });
        let error = anyhow::anyhow!("invalid type: map, expected a string")
            .context("parsing Claude SessionStart callback");
        let original = pending_view
            .read_with(cx, |view, cx| {
                view.capture_error_message(ResumableCliProvider::Claude, &error, cx)
            })
            .unwrap();
        assert!(original.contains("pane: Center 1, terminal: before"));
        let destination = window_handle
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.split_pane(
                        original_pane.clone(),
                        workspace::SplitDirection::Right,
                        window,
                        cx,
                    )
                })
            })
            .unwrap();
        window_handle
            .update(cx, |_, window, cx| {
                workspace::move_item(
                    &original_pane,
                    &destination,
                    view.entity_id(),
                    0,
                    true,
                    window,
                    cx,
                );
            })
            .unwrap();
        view.update(cx, |view, cx| {
            view.set_custom_title(Some("after".into()), cx)
        });
        let moved = pending_view
            .read_with(cx, |view, cx| {
                view.capture_error_message(ResumableCliProvider::Claude, &error, cx)
            })
            .unwrap();
        assert!(moved.contains("pane: Center 2, terminal: after"));
        assert!(moved.contains(
            "parsing Claude SessionStart callback: invalid type: map, expected a string"
        ));
        assert!(!moved.contains("terminal: before"));
        other.update(cx, |view, cx| {
            view.set_custom_title(Some("other agent".into()), cx)
        });
        let concurrent = other.read_with(cx, |view, cx| {
            view.capture_error_message(
                ResumableCliProvider::Copilot,
                &anyhow::anyhow!("timed out"),
                cx,
            )
        });
        assert!(concurrent.contains("Copilot"));
        assert!(concurrent.contains("pane: Center 1, terminal: other agent"));
        assert!(!concurrent.contains("terminal: after"));
    }

    // Terminal rename tests

    #[gpui::test]
    async fn test_custom_title_initially_none(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (project, workspace) = init_test(cx).await;

        let terminal = project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx))
            .await
            .unwrap();

        let terminal_view = cx
            .add_window(|window, cx| {
                TerminalView::new(
                    terminal,
                    workspace.downgrade(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            })
            .root(cx)
            .unwrap();

        terminal_view.update(cx, |view, _cx| {
            assert!(view.custom_title().is_none());
        });
    }

    #[gpui::test]
    async fn test_set_custom_title(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (project, workspace) = init_test(cx).await;

        let terminal = project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx))
            .await
            .unwrap();

        let terminal_view = cx
            .add_window(|window, cx| {
                TerminalView::new(
                    terminal,
                    workspace.downgrade(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            })
            .root(cx)
            .unwrap();

        terminal_view.update(cx, |view, cx| {
            view.set_custom_title(Some("frontend".to_string()), cx);
            assert_eq!(view.custom_title(), Some("frontend"));
        });
    }

    #[gpui::test]
    async fn test_set_custom_title_empty_becomes_none(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (project, workspace) = init_test(cx).await;

        let terminal = project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx))
            .await
            .unwrap();

        let terminal_view = cx
            .add_window(|window, cx| {
                TerminalView::new(
                    terminal,
                    workspace.downgrade(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            })
            .root(cx)
            .unwrap();

        terminal_view.update(cx, |view, cx| {
            view.set_custom_title(Some("test".to_string()), cx);
            assert_eq!(view.custom_title(), Some("test"));

            view.set_custom_title(Some("".to_string()), cx);
            assert!(view.custom_title().is_none());

            view.set_custom_title(Some("  ".to_string()), cx);
            assert!(view.custom_title().is_none());
        });
    }

    #[gpui::test]
    async fn test_custom_title_marks_needs_serialize(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (project, workspace) = init_test(cx).await;

        let terminal = project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx))
            .await
            .unwrap();

        let terminal_view = cx
            .add_window(|window, cx| {
                TerminalView::new(
                    terminal,
                    workspace.downgrade(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            })
            .root(cx)
            .unwrap();

        terminal_view.update(cx, |view, cx| {
            view.needs_serialize = false;
            view.set_custom_title(Some("new_label".to_string()), cx);
            assert!(view.needs_serialize);
        });
    }

    #[gpui::test]
    async fn test_tab_content_uses_custom_title(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (project, workspace) = init_test(cx).await;

        let terminal = project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx))
            .await
            .unwrap();

        let terminal_view = cx
            .add_window(|window, cx| {
                TerminalView::new(
                    terminal,
                    workspace.downgrade(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            })
            .root(cx)
            .unwrap();

        terminal_view.update(cx, |view, cx| {
            view.set_custom_title(Some("my-server".to_string()), cx);
            let text = view.tab_content_text(0, cx);
            assert_eq!(text.as_ref(), "my-server");
        });

        terminal_view.update(cx, |view, cx| {
            view.set_custom_title(None, cx);
            let text = view.tab_content_text(0, cx);
            assert_ne!(text.as_ref(), "my-server");
        });
    }

    async fn draw_standalone_terminal(
        output: &[u8],
        cx: &mut TestAppContext,
    ) -> (gpui::Bounds<Pixels>, gpui::Size<Pixels>) {
        let (project, workspace) = init_test(cx).await;
        let terminal = cx.new(|cx| {
            terminal::TerminalBuilder::new_display_only(
                CursorShape::default(),
                terminal::terminal_settings::AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        terminal.update(cx, |terminal, cx| {
            terminal.write_output(output, cx);
        });

        let (terminal_view, cx) = cx.add_window_view(|window, cx| {
            TerminalView::new(
                terminal.clone(),
                workspace.downgrade(),
                None,
                project.downgrade(),
                window,
                cx,
            )
        });

        let draw_size = gpui::size(px(400.), px(201.));
        cx.simulate_resize(draw_size);
        cx.draw(gpui::Point::default(), draw_size, |_, _| {
            terminal_view.clone().into_any_element()
        });
        cx.run_until_parked();
        cx.draw(gpui::Point::default(), draw_size, |_, _| {
            terminal_view.clone().into_any_element()
        });

        let bounds = terminal.read_with(cx, |terminal, _| {
            terminal.last_content().terminal_bounds.bounds
        });
        (bounds, draw_size)
    }

    #[gpui::test]
    async fn test_short_standalone_terminal_stays_top_anchored_on_resize(cx: &mut TestAppContext) {
        let (bounds, _) = draw_standalone_terminal(b"$ ", cx).await;
        assert_eq!(bounds.origin.y, px(0.));
    }

    #[gpui::test]
    async fn test_full_standalone_terminal_stays_bottom_anchored_on_resize(
        cx: &mut TestAppContext,
    ) {
        let (bounds, draw_size) = draw_standalone_terminal(
            b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
            cx,
        )
        .await;
        assert!(bounds.origin.y > px(0.));
        assert_eq!(bounds.bottom(), draw_size.height);
    }

    #[gpui::test]
    async fn test_short_alt_screen_stays_bottom_anchored_on_resize(cx: &mut TestAppContext) {
        let (bounds, draw_size) = draw_standalone_terminal(b"\x1b[?1049h$ ", cx).await;
        assert!(bounds.origin.y > px(0.));
        assert_eq!(bounds.bottom(), draw_size.height);
    }

    #[gpui::test]
    async fn test_inline_terminal_displays_all_of_its_lines(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;
        let terminal = cx.new(|cx| {
            terminal::TerminalBuilder::new_display_only(
                CursorShape::default(),
                terminal::terminal_settings::AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        let (terminal_view, cx) = cx.add_window_view(|window, cx| {
            let mut terminal_view = TerminalView::new(
                terminal.clone(),
                workspace.downgrade(),
                None,
                project.downgrade(),
                window,
                cx,
            );
            terminal_view.set_embedded_mode(None, cx);
            terminal_view
        });

        for _ in 1..=20 {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"line\n", cx);
            });
            cx.draw(
                gpui::Point::default(),
                gpui::size(px(400.), px(100.)),
                |_, _| terminal_view.clone().into_any_element(),
            );
            terminal.read_with(cx, |terminal, _| {
                assert_eq!(terminal.viewport_lines(), terminal.total_lines());
            })
        }
    }

    #[gpui::test]
    async fn test_inline_terminal_shrinks_after_clear(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;
        let terminal = cx.new(|cx| {
            terminal::TerminalBuilder::new_display_only(
                CursorShape::default(),
                terminal::terminal_settings::AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        let (terminal_view, cx) = cx.add_window_view(|window, cx| {
            let mut terminal_view = TerminalView::new(
                terminal.clone(),
                workspace.downgrade(),
                None,
                project.downgrade(),
                window,
                cx,
            );
            terminal_view.set_embedded_mode(None, cx);
            terminal_view
        });

        for _ in 1..=20 {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"line\n", cx);
            });
            cx.draw(
                gpui::Point::default(),
                gpui::size(px(400.), px(100.)),
                |_, _| terminal_view.clone().into_any_element(),
            );
        }
        terminal.read_with(cx, |terminal, _| {
            assert_eq!(terminal.total_lines(), 21);
        });

        terminal.update(cx, |terminal, _| terminal.clear());
        for _ in 1..=2 {
            cx.draw(
                gpui::Point::default(),
                gpui::size(px(400.), px(100.)),
                |_, _| terminal_view.clone().into_any_element(),
            );
        }
        terminal.read_with(cx, |terminal, _| {
            assert_eq!(terminal.total_lines(), 1);
            assert_eq!(terminal.viewport_lines(), 1);
        });
    }

    #[gpui::test]
    async fn test_tab_content_shows_terminal_title_when_custom_title_directly_set_empty(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();

        let (project, workspace) = init_test(cx).await;

        let terminal = project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx))
            .await
            .unwrap();

        let terminal_view = cx
            .add_window(|window, cx| {
                TerminalView::new(
                    terminal,
                    workspace.downgrade(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            })
            .root(cx)
            .unwrap();

        terminal_view.update(cx, |view, cx| {
            view.custom_title = Some("".to_string());
            let text = view.tab_content_text(0, cx);
            assert!(
                !text.is_empty(),
                "Tab should show terminal title, not empty string; got: '{}'",
                text
            );
        });

        terminal_view.update(cx, |view, cx| {
            view.custom_title = Some("   ".to_string());
            let text = view.tab_content_text(0, cx);
            assert!(
                !text.is_empty() && text.as_ref() != "   ",
                "Tab should show terminal title, not whitespace; got: '{}'",
                text
            );
        });
    }
    #[gpui::test]
    async fn terminal_creation_resolves_launch_modes_nested_roots_and_outside(
        cx: &mut TestAppContext,
    ) {
        let (project, workspace, window_handle) = init_test_with_window(cx).await;
        let root_a = PathBuf::from(util::path!("/creation-a"));
        let root_b = PathBuf::from(util::path!("/creation-b"));
        let (worktree_a, _) = create_folder_wt(project.clone(), &root_a, cx).await;
        let (_worktree_b, _) = create_folder_wt(project.clone(), &root_b, cx).await;
        let file = create_file_in_worktree(worktree_a.clone(), "src/main.rs", cx).await;
        insert_active_entry_for(worktree_a.clone(), file.clone(), project.clone(), cx);
        for mode in [
            WorkingDirectory::CurrentProjectDirectory,
            WorkingDirectory::FirstProjectDirectory,
            WorkingDirectory::CurrentFileDirectory,
        ] {
            cx.update(|cx| {
                let mut settings = TerminalSettings::get_global(cx).clone();
                settings.working_directory = mode.clone();
                TerminalSettings::override_global(settings, cx);
            });
            insert_active_entry_for(worktree_a.clone(), file.clone(), project.clone(), cx);
            let launch = workspace.read_with(cx, |workspace, cx| {
                default_working_directory(workspace, cx).unwrap()
            });
            let expected_launch = match mode {
                WorkingDirectory::CurrentFileDirectory => root_a.join("src"),
                WorkingDirectory::FirstProjectDirectory => {
                    workspace.read_with(cx, |workspace, cx| {
                        workspace
                            .worktrees(cx)
                            .next()
                            .unwrap()
                            .read(cx)
                            .abs_path()
                            .to_path_buf()
                    })
                }
                _ => root_a.clone(),
            };
            assert_eq!(launch, expected_launch);
            let expected_root = project.read_with(cx, |project, cx| {
                let path = project.find_project_path(&launch, cx).unwrap();
                project
                    .worktree_for_id(path.worktree_id, cx)
                    .unwrap()
                    .read(cx)
                    .abs_path()
                    .to_path_buf()
            });
            let (_, _, view) =
                add_terminal_with_launch_directory(&project, window_handle, launch, cx);
            assert_eq!(
                view.read_with(cx, |view, cx| view.project_worktree_root(cx)),
                Some(expected_root)
            );
        }
        let explicit_directory = std::env::current_dir().unwrap();
        create_folder_wt(project.clone(), &explicit_directory, cx).await;
        for mode in [
            WorkingDirectory::Always {
                directory: explicit_directory.to_string_lossy().into_owned(),
            },
            WorkingDirectory::AlwaysHome,
        ] {
            cx.update(|cx| {
                let mut settings = TerminalSettings::get_global(cx).clone();
                settings.working_directory = mode;
                TerminalSettings::override_global(settings, cx);
            });
            let launch = workspace.read_with(cx, |workspace, cx| {
                default_working_directory(workspace, cx).unwrap()
            });
            let expected_root = project.read_with(cx, |project, cx| {
                project_worktree_root_for_directory(project, Some(&launch), cx)
            });
            let (_, _, view) =
                add_terminal_with_launch_directory(&project, window_handle, launch, cx);
            assert_eq!(
                view.read_with(cx, |view, cx| view.project_worktree_root(cx)),
                expected_root
            );
        }
        let nested = root_a.join("nested");
        create_folder_wt(project.clone(), &nested, cx).await;
        let launch = nested.join("src");
        let expected_root = project.read_with(cx, |project, cx| {
            let path = project.find_project_path(&launch, cx).unwrap();
            project
                .worktree_for_id(path.worktree_id, cx)
                .unwrap()
                .read(cx)
                .abs_path()
                .to_path_buf()
        });
        let (_, terminal, view) =
            add_terminal_with_launch_directory(&project, window_handle, launch.clone(), cx);
        assert_eq!(
            view.read_with(cx, |view, cx| view.project_worktree_root(cx)),
            Some(expected_root)
        );
        let resume_directory = root_a.join("exact-session-folder");
        view.update(cx, |view, _| {
            view.resumable_cli_binding = ResumableCliBinding {
                pending_codex_cwd: None,
                revision: 1,
                session: Some(
                    ResumableCliSession::new(
                        ResumableCliProvider::Hermes,
                        "20260927_040017_ab12cd".into(),
                        resume_directory.clone(),
                        ResumableCliEvidence::ProviderEvent,
                    )
                    .unwrap(),
                ),
            }
        });
        terminal.update(cx, |terminal, _| {
            terminal
                .set_working_directory_for_test(Some(PathBuf::from(util::path!("/hermes-temp"))))
        });
        assert_eq!(
            view.read_with(cx, |view, cx| view.active_project_path(cx))
                .unwrap()
                .path,
            RelPath::empty_arc()
        );
        assert_eq!(
            view.read_with(cx, |view, _| view
                .resumable_cli_binding
                .session()
                .unwrap()
                .resume_cwd()
                .to_path_buf()),
            resume_directory
        );
        let (_, outside_terminal, outside_view) = add_terminal_with_launch_directory(
            &project,
            window_handle,
            PathBuf::from(util::path!("/outside-creation")),
            cx,
        );
        outside_terminal.update(cx, |terminal, _| {
            terminal.set_working_directory_for_test(Some(root_b))
        });
        assert_eq!(
            outside_view.read_with(cx, |view, cx| view.active_project_path(cx)),
            None
        );
        assert_eq!(
            outside_view.read_with(cx, |view, _| view.project_worktree_association.clone()),
            ProjectWorktreeAssociation::Recorded(None)
        );
    }

    #[gpui::test]
    async fn terminal_restoration_records_legacy_ai_association_only_once(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let (project, workspace, window_handle) = init_test_with_window(cx).await;
        let root = PathBuf::from(util::path!("/legacy-association"));
        let (worktree, _) = create_folder_wt(project.clone(), &root, cx).await;
        let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
        let database = cx.update(|cx| TerminalDb::global(cx));
        let workspace_id = cx
            .update(|cx| workspace::WorkspaceDb::global(cx))
            .next_id()
            .await
            .unwrap();
        workspace.update(cx, |workspace, _| {
            workspace.set_database_id_for_test(workspace_id)
        });
        let launch_directory = std::env::current_dir().unwrap();
        for (index, association, has_session, expected) in [
            (
                0,
                ProjectWorktreeAssociation::Unknown,
                true,
                ProjectWorktreeAssociation::Recorded(Some(root.clone())),
            ),
            (
                1,
                ProjectWorktreeAssociation::Unknown,
                false,
                ProjectWorktreeAssociation::Unknown,
            ),
            (
                2,
                ProjectWorktreeAssociation::Recorded(None),
                true,
                ProjectWorktreeAssociation::Recorded(None),
            ),
            (
                3,
                ProjectWorktreeAssociation::Recorded(Some(root.join("missing"))),
                true,
                ProjectWorktreeAssociation::Recorded(Some(root.join("missing"))),
            ),
            (
                4,
                ProjectWorktreeAssociation::Recorded(Some(PathBuf::from("relative"))),
                true,
                ProjectWorktreeAssociation::Recorded(Some(PathBuf::from("relative"))),
            ),
            (
                5,
                ProjectWorktreeAssociation::Unknown,
                true,
                ProjectWorktreeAssociation::Recorded(None),
            ),
        ] {
            let item_id = 2200 + index;
            let persistent_id = PersistentTerminalId::legacy(workspace_id, item_id);
            database
                .save_snapshot_with_association(
                    item_id,
                    workspace_id,
                    Some(launch_directory.clone()),
                    Some("restored title".into()),
                    association,
                )
                .await
                .unwrap();
            let session = ResumableCliSession::new(
                ResumableCliProvider::Codex,
                "0199a4f5-1fe0-7000-8000-000000000022".into(),
                if index == 5 {
                    PathBuf::from(util::path!("/unmatched-legacy-ai/session"))
                } else {
                    root.join("exact-resume-folder")
                },
                ResumableCliEvidence::ProviderEvent,
            )
            .unwrap();
            if has_session {
                database
                    .compare_and_set_resumable_cli_session(
                        item_id,
                        workspace_id,
                        0,
                        Some(session.clone()),
                    )
                    .await
                    .unwrap();
            }
            let view = window_handle
                .update(cx, |_, window, cx| {
                    TerminalView::deserialize(
                        project.clone(),
                        workspace.downgrade(),
                        workspace_id,
                        item_id,
                        window,
                        cx,
                    )
                })
                .unwrap()
                .await
                .unwrap();
            view.read_with(cx, |view, cx| {
                assert_eq!(view.project_worktree_association, expected);
                assert_eq!(
                    view.project_worktree_root(cx),
                    if index == 0 { Some(root.clone()) } else { None }
                );
                assert_eq!(view.custom_title.as_deref(), Some("restored title"));
                assert_eq!(
                    view.terminal().read(cx).initial_working_directory(),
                    Some(launch_directory.as_path())
                );
                assert_eq!(
                    view.resumable_cli_binding.session(),
                    if has_session { Some(&session) } else { None }
                );
            });
            assert_eq!(
                database
                    .persistent_project_worktree_association(&persistent_id, workspace_id)
                    .unwrap(),
                expected
            );
        }
        create_folder_wt(project.clone(), util::path!("/unmatched-legacy-ai"), cx).await;
        let unassociated_id = PersistentTerminalId::legacy(workspace_id, 2205);
        let unassociated = window_handle
            .update(cx, |_, window, cx| {
                TerminalView::deserialize_with_persistent_item_id(
                    project.clone(),
                    workspace.downgrade(),
                    workspace_id,
                    2205,
                    Some(unassociated_id.as_persistent_item_id()),
                    window,
                    cx,
                )
            })
            .unwrap()
            .await
            .unwrap();
        assert_eq!(
            unassociated.read_with(cx, |view, cx| view.project_worktree_root(cx)),
            None
        );
        assert_eq!(
            database
                .persistent_project_worktree_association(&unassociated_id, workspace_id)
                .unwrap(),
            ProjectWorktreeAssociation::Recorded(None)
        );
        project.update(cx, |project, cx| project.remove_worktree(worktree_id, cx));
        let persistent_id = PersistentTerminalId::legacy(workspace_id, 2200);
        let view = window_handle
            .update(cx, |_, window, cx| {
                TerminalView::deserialize_with_persistent_item_id(
                    project.clone(),
                    workspace.downgrade(),
                    workspace_id,
                    2200,
                    Some(persistent_id.as_persistent_item_id()),
                    window,
                    cx,
                )
            })
            .unwrap()
            .await
            .unwrap();
        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.project_worktree_association,
                ProjectWorktreeAssociation::Recorded(Some(root.clone()))
            );
            assert_eq!(view.project_worktree_root(cx), None);
            assert_eq!(
                view.resumable_cli_binding.session().unwrap().resume_cwd(),
                root.join("exact-resume-folder")
            );
        });
    }
}
