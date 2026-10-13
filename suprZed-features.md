# suprZed features

These are the implemented features, with the main agent features first.

Each feature has a short name, tags and an overview. Usage notes explain where to find it or what setup it needs. Centre
panes are the main tab area where files normally open. Agent CLI terminals run command-line agents in Zed's terminals.

## Agent CLI terminals in centre panes

**Tags:** Agents, Terminals, Panes

**Overview:** Work with several Agent CLI terminals beside code or beside each other in split panes. Each agent uses its
own command-line interface. Zed already supports terminals in centre panes. suprZed identifies the agents, lists them
in Threads and saves their conversations so they can reopen after a restart.

**Usage:** Create an Agent CLI terminal from the pane's **+** menu or the command palette. The **New Resumable ...
Terminal** actions support Codex, Claude, Copilot CLI and Hermes on Windows. Use Zed's split-pane controls to arrange
the terminals alongside files.

With the default Windows keymap, **Ctrl+Shift+P** or **F1** opens the command palette. Search for **New Resumable** to
find the agent terminal actions.

## Resumable Agent CLI terminals

**Tags:** Agents, Terminals, Session restoration

**Overview:** suprZed records the exact conversation started by a dedicated Agent CLI terminal. When the workspace is
restored, it can reopen that conversation using its saved session ID and settings. When suprZed recognises an agent in
a centre terminal without a custom title, it gives it a name such as `codex-1`, `claude-1`, `copilot-1` and `hermes-1`.
You can rename them for the task they are working on. The renamed terminal still reopens the same conversation.

**Usage:** On Windows, choose the action for your provider:

| Provider    | Action                             | Setup                                                                                                                          |
| ----------- | ---------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| Codex       | **New Resumable Codex Terminal**   | Install Codex and sign in. Follow any editor instructions for saving the conversation ID.                                      |
| Claude      | **New Resumable Claude Terminal**  | Install Claude CLI and sign in. Complete any trust prompts so suprZed can save the conversation ID.                           |
| Copilot CLI | **New Resumable Copilot Terminal** | Install Copilot CLI and sign in. Complete any plugin trust prompts so suprZed can save the conversation ID.                   |
| Hermes      | **New Resumable Hermes Terminal**  | Install and configure Hermes. Follow any hook setup instructions. Use the same Hermes profile when reopening the conversation. |

Reopening a conversation requires a supported agent version, a saved session ID and the agent's conversation history. It
reopens the conversation; it does not continue a task interrupted by closing the editor. Copilot CLI has its own
sign-in, separate from Copilot Chat inside Zed.

Codex can wait for your first prompt without a session-capture timeout. If you restart before a conversation has been
saved, the terminal starts a fresh Codex session. After choosing another conversation with `/resume`, send a prompt to
save that choice for the next restart. Choosing a conversation without sending a prompt keeps the previous saved choice.

If a saved conversation cannot be resumed, its association is kept. Use **Start New AI Session** in the terminal's
context menu to replace it, or **Forget AI Session** to stop saving it.

## Agent CLI terminals in Threads

**Tags:** Agents, Terminals, Navigation

**Overview:** The Threads sidebar lists recognised running Agent CLI terminals from centre panes alongside Zed Agent and
ACP conversations. Entries show the agent and terminal name. Selecting an entry takes you to that terminal and pane.

**Usage:** Open an Agent CLI terminal in a centre pane, then use Threads to switch back to it. Rename the terminal to
give its tab and Threads entry a useful task label. When the agent exits to the shell, its Threads entry is removed and
the terminal tab remains open.

## Agent CLI terminals across worktrees

**Tags:** Agents, Projects, Worktrees, Navigation

**Overview:** suprZed includes centre-pane Agent CLI terminals in the existing Threads project and worktree groups.
Agents from different open worktrees can appear in the same Threads sidebar. Selecting an entry takes you to the agent's
workspace and pane. Each terminal remembers the project it was created in, including after directory changes and an
editor restart.

Zed already supports Threads across projects and worktrees. The fork adds centre-pane Agent CLI terminals to that
navigation and grouping.

**Usage:** Create each Agent CLI terminal in the project you want it to belong to. Use Threads to return to agents
working in other open worktrees. A shell `cd` changes where commands run. The terminal still belongs to its original
project.

## Thread alerts for completed tasks

**Tags:** Agents, Notifications, Terminals

**Overview:** Agent CLI terminals can alert you when a task finishes or the agent needs input. When the agent sends a
terminal bell signal, its tab shows an alert. Its Threads entry also shows an alert if the terminal is hidden or its
window is inactive. Click into the terminal to clear both markers. No keypress is needed. A terminal visible in another
split of the active window does not show a Threads alert just because you are working in a different split.

**Usage:** Enable bell signals in the agent's settings. For Copilot CLI, enable `beep`. For Hermes, enable
`display.bell_on_complete` and `display.bell_on_prompt` in the profile you use. Claude can use the fork's
`script/claude-terminal-bell.ps1` helper with `Stop`, `StopFailure` and `PermissionRequest` hooks. The helper returns a
bell signal in the format Claude expects. Disable Claude's built-in notifications and exclude `idle_prompt` and
`permission_prompt` from any additional `Notification` bell hook to avoid delayed or duplicate alerts. Keep your
existing hooks when adding these settings. Configure alerts on each machine where you use the agents.

## Zed Agent and ACP conversations in centre panes

**Tags:** Agents, ACP, Panes

**Overview:** Zed Agent and external agents configured through ACP can open as ordinary centre tabs. Conversations can
sit beside files or beside other conversations in split panes. Threads returns to an existing centre conversation
instead of opening a second copy of it.

**Usage:** Choose **New Agent Thread** from the pane's **+** menu.
In the command palette, use **agent: new center thread**. ACP means Agent Client Protocol.
These agents use Zed's conversation UI. Centre ACP tabs do not yet reopen after an editor restart.

## Session controls

**Tags:** Agents, Terminals, Session restoration

**Overview:** You can reopen a saved conversation, start a new one or remove the terminal's link to a conversation. When
you start a new conversation, suprZed keeps the old link until it has saved the new session ID. Forgetting a session
removes the link from the terminal and keeps the agent's conversation history. If the session is already active in
another terminal, suprZed asks before opening it again.

**Usage:** With the terminal active, use **Resume AI Session**, **Start New AI Session** or **Forget AI Session** from
its menu or the command palette.

## Terminals in centre panes and the dock

**Tags:** Terminals, Panes, Layout, Session restoration

**Overview:** A terminal can move between the main tab area and the Terminal Panel. It keeps its title, working
directory and saved conversation when you move it or restart the editor. Terminal commands use the focused centre
terminal when you are working in it. The Terminal Panel accepts terminal tabs. File tabs and Zed Agent or ACP
conversation tabs stay in the centre panes.

**Usage:** Drag a terminal tab between the centre and the Terminal Panel. The dock is the panel area at the bottom or
side of the workspace; it contains tools such as the Terminal Panel. Moving a terminal keeps its process running.

## Project selection for terminals and agents

**Tags:** Projects, Git, Agents, Terminals

**Overview:** A new terminal remembers the project containing the folder it starts in. It keeps that project after `cd`,
pane moves and an editor restart. Focusing the terminal can select that project's repository in the window's Git tools.
Threads groups the terminal under the same project. Zed Agent and ACP threads use their conversation working directories
to find an open project.

**Usage:** Create a terminal in the project you want it to belong to. Changing the shell directory changes where
commands run. The terminal still belongs to its original project. If that project folder is no longer open, the terminal
has no active project. Switching to an agent or terminal keeps the current project theme.

## Quick Folders

**Tags:** Projects, Navigation

**Overview:** The Project Panel has a Quick Folders section with pinned and recent folder entries. Clicking an entry
reveals that folder in the file tree. The list is saved per workspace. You can scroll it when there are more entries
than fit in the available space.

**Usage:** Right-click a folder in the Project Panel and choose **Pin to Quick Folders**. Use **Unpin from Quick
Folders** to remove its pin. An unpinned folder may still appear as a recent folder. Hover over an entry to see its full
path.

## Per-project themes

**Tags:** Projects, Appearance

**Overview:** A project can have its own saved colour theme. Opening its workspace applies that theme. In a workspace
with several project folders, selecting a file from another folder can select that folder's theme. Focusing an agent or
changing a terminal's directory leaves the selected project theme in place. Windows opened for the same project and root
folder share the saved theme.

**Usage:** Open the theme selector, preview a theme and confirm it to save the project's theme. Press **Escape** before
confirming to return to the previous theme. If the saved theme is unavailable, Zed uses your global theme. It keeps the
saved choice and can apply it when that theme becomes available again.

To use another project's theme in a separate window, press **Ctrl+Shift+N** with the default Windows keymap and open the
project there.

## Agent and pane accents

**Tags:** Agents, Appearance, Panes

**Overview:** Native agent tabs and recognised Agent CLI terminal tabs have a thin muted teal marker. Centre panes have
a teal line along their top edge, and horizontal dividers between vertically stacked centre panes use the same colour.
Dark themes on Windows also have a neutral dark title bar. Unsaved changes and conflicts still show their usual tab
indicators in place of the agent marker.

**Usage:** The accents appear automatically. All recognised agents use the same accent colour.

## File-only recent navigation

**Tags:** Navigation, Projects, Terminals

**Overview:** Terminal project paths stay out of recent-file history and pane navigation history. Changing or focusing a
terminal does not add its directory to the file list. Older directory entries are filtered from recent-file results.
Editor file navigation continues to work normally.

**Usage:** Use Zed's recent-file picker as usual. Its entries are files rather than terminal working directories.

## Copilot Chat account identity

**Tags:** Accounts, Copilot Chat, Agents

**Overview:** Zed's Copilot Chat sign-in UI shows which account is signed in when the account name is available.
Changing credentials clears the old account name. This display refers to Copilot Chat inside Zed. Copilot CLI terminals
have their own sign-in.

**Usage:** Look for **Authorized as ...** in the Copilot Chat sign-in UI after signing in.

## Window identity

**Tags:** Windows, Appearance

**Overview:** The title bar and OS window title identify the fork as `suprZed` and show the process ID. This helps you
tell separate running copies apart during development or testing.

**Usage:** Windows in one process share its process ID. Separate processes have different IDs.
