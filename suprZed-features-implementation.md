# suprZed feature implementation

This page maps the fork's features to their main source files.
Use [suprZed-features.md](suprZed-features.md) for usage instructions.
Use the [build guide](suprzed-docs/building.md) to build the app.

The files below are starting points for code review and upstream merges.
They are not a list of every changed file. Much of each file is upstream Zed code.
This map describes the current implementation. It does not establish that every manual case has been tested.

## Agent terminals and session restoration

| Area | Main files | Changes |
| --- | --- | --- |
| Agent CLI terminals in centre panes | [terminal_view.rs](crates/terminal_view/src/terminal_view.rs), [pane.rs](crates/workspace/src/pane.rs) | Integrates agent actions and session controls with centre terminal tabs |
| Provider launch and capture | [resumable_cli.rs](crates/terminal_view/src/resumable_cli.rs) | Launches supported providers, captures session identity and prepares resume commands |
| Saved terminal sessions | [terminal persistence](crates/terminal_view/src/persistence.rs), [workspace model](crates/workspace/src/persistence/model.rs) | Stores persistent terminal identity and provider bindings for restoration |
| Agent detection | [pty_info.rs](crates/terminal/src/pty_info.rs), [terminal.rs](crates/terminal/src/terminal.rs) | Provides process information and terminal events used by agent detection |
| Threads entries and worktree groups | [sidebar.rs](crates/sidebar/src/sidebar.rs), [thread_switcher.rs](crates/sidebar/src/thread_switcher.rs) | Lists recognised centre terminals and returns to their owning panes |
| Completion and attention alerts | [terminal_view.rs](crates/terminal_view/src/terminal_view.rs), [sidebar.rs](crates/sidebar/src/sidebar.rs), [Claude bell helper](script/claude-terminal-bell.ps1) | Handles terminal Bell events and focus acknowledgement; supplies Claude hook output |
| Centre and dock routing | [terminal_panel.rs](crates/terminal_view/src/terminal_panel.rs), [pane.rs](crates/workspace/src/pane.rs), [workspace.rs](crates/workspace/src/workspace.rs) | Moves terminal tabs between hosts and preserves their saved state |

A provider session binding is separate from a terminal title or process name.
It records the provider, session ID and resume directory.
Stale snapshots cannot replace a newer binding or restore a cleared one.
Session claims prevent two active terminals from owning the same session.

Codex and Claude keep identity observers for the terminal's lifetime.
Copilot capture is bounded. Hermes capture ends after its initial callback.
Hermes does not continuously track later conversation switches.
Codex saves a conversation selected with `/resume` after the next prompt.

Provider hooks require the provider's own trust and configuration steps.
Different app paths can change the shared Codex hook command.
The exact resume directory is separate from the terminal's creation project.

## Zed Agent and ACP centre tabs

The main files are [thread_item.rs](crates/agent_ui/src/thread_item.rs),
[agent_panel.rs](crates/agent_ui/src/agent_panel.rs) and
[conversation_view.rs](crates/agent_ui/src/conversation_view.rs).
Action registration is in [agent_ui.rs](crates/agent_ui/src/agent_ui.rs).
Thread metadata is handled by [thread_metadata_store.rs](crates/agent_ui/src/thread_metadata_store.rs).

A centre thread owns a conversation view. Threads navigation reuses an existing centre item.
These items are separate from terminal-hosted agent sessions.
Centre ACP tabs do not yet reopen after an editor restart.

## Projects, folders and themes

| Feature | Main files | Changes |
| --- | --- | --- |
| Project association | [project terminals](crates/project/src/terminals.rs), [terminal_view.rs](crates/terminal_view/src/terminal_view.rs), [terminal persistence](crates/terminal_view/src/persistence.rs), [thread_item.rs](crates/agent_ui/src/thread_item.rs), [sidebar.rs](crates/sidebar/src/sidebar.rs) | Records the terminal's creation worktree and uses it for workspace context and Threads grouping |
| Quick Folders | [project_panel.rs](crates/project_panel/src/project_panel.rs), [workspace persistence](crates/workspace/src/persistence.rs) | Adds pinned and recent folders with saved workspace state |
| Per-project themes | [theme_selector.rs](crates/theme_selector/src/theme_selector.rs), [theme_settings.rs](crates/theme_settings/src/theme_settings.rs), [workspace.rs](crates/workspace/src/workspace.rs), [workspace persistence](crates/workspace/src/persistence.rs) | Saves project theme assignments and handles preview, cancellation and fallback |
| File-only recent navigation | [item.rs](crates/workspace/src/item.rs), [workspace.rs](crates/workspace/src/workspace.rs), [terminal_view.rs](crates/terminal_view/src/terminal_view.rs), [thread_item.rs](crates/agent_ui/src/thread_item.rs) | Excludes terminal and agent project paths from file navigation history |

A terminal keeps its creation project after `cd`, pane moves and restart.
Focusing it can select that project's Git repository.
It does not select another project theme.
Native thread association uses conversation working directories.

Keep database migrations in their tested order when merging upstream changes.
Workspace persistence includes Quick Folders, project themes and persistent item IDs.
Terminal persistence includes provider bindings and the recorded worktree root.
A schema change needs restoration tests as well as a successful build.

## Appearance and account identity

| Feature | Main files | Changes |
| --- | --- | --- |
| Agent tab and pane accents | [pane.rs](crates/workspace/src/pane.rs), [pane_group.rs](crates/workspace/src/pane_group.rs), [platform_title_bar.rs](crates/platform_title_bar/src/platform_title_bar.rs) | Adds agent markers, centre-pane accents and the dark Windows title bar |
| Copilot Chat account identity | [copilot_chat.rs](crates/copilot_chat/src/copilot_chat.rs), [sign_in.rs](crates/copilot_ui/src/sign_in.rs) | Loads the account name and shows it in the sign-in interface |
| Window identity | [title_bar.rs](crates/title_bar/src/title_bar.rs), [workspace.rs](crates/workspace/src/workspace.rs) | Displays suprZed and the process ID in the title surfaces |

Copilot Chat and Copilot CLI have separate sign-in flows.
The account label does not establish acceptance of every credential or enterprise case.
Window identity is display information. It does not isolate profiles or taskbar groups.

## Build versions, icons and build speed

Version parsing is in [suprzed_version.rs](crates/release_channel/src/suprzed_version.rs).
Build metadata is prepared by [build_version.rs](crates/zed/build_version.rs).
The app and CLI use [app build.rs](crates/zed/build.rs) and [CLI build.rs](crates/cli/build.rs).
[SUPRZED_VERSION](SUPRZED_VERSION) provides the fork version baseline.

Windows resources are compiled by [windows_resources.rs](crates/windows_resources/src/windows_resources.rs).
`SUPRZED_WINDOWS_ICON` chooses the main or debug icon independently of the release channel.
The build scripts track the selector and selected icon for cache invalidation.
The icon files are in [Windows resources](crates/zed/resources/windows/).
Main and debug can still share a Windows taskbar identity.

Build profiles are defined in [Cargo.toml](Cargo.toml).
The `release-fast` profile reduces optimisation work for quicker local builds.
It is separate from the standard release profile.
[build_conpty.rs](crates/zed/build_conpty.rs) caches Windows console dependencies and avoids unnecessary file copies.
These changes do not establish a fixed build-time improvement on every machine.

## Regression tests and upstream updates

Start with these test locations:

- [TerminalView](crates/terminal_view/src/terminal_view.rs), [provider capture](crates/terminal_view/src/resumable_cli.rs) and [terminal persistence](crates/terminal_view/src/persistence.rs).
- [Sidebar tests](crates/sidebar/src/sidebar_tests.rs) for agent entries, grouping and Bell handling.
- [Project Panel tests](crates/project_panel/src/project_panel_tests.rs) for Quick Folders and scrolling.
- [Theme selector](crates/theme_selector/src/theme_selector.rs) and [workspace](crates/workspace/src/workspace.rs) for theme and navigation behaviour.
- [Copilot Chat](crates/copilot_chat/src/copilot_chat.rs) for account discovery and credential replacement.
- [Windows resources](crates/windows_resources/src/windows_resources.rs) for icon selection.

Run full affected library suites after an upstream merge. Check test targets and formatting too.
Windows resource tests need an explicit `cargo test -p windows_resources --lib` command.
They are not included by default test discovery.

Automated provider fixtures do not establish live CLI compatibility.
Test session restoration with installed providers and a backed-up profile.
Check pane routing, notifications and two-project Git selection after restart.
The fork currently supports Windows only. Linux and macOS are outside the supported scope.

Repository policy is in [CONTRIBUTING.md](CONTRIBUTING.md).
The fork's issue forms are in [.github/ISSUE_TEMPLATE](.github/ISSUE_TEMPLATE/).
They are separate from the upstream contribution guide.
