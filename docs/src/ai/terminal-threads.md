---
title: Terminal Threads - Zed
description: Run agent CLIs and TUIs directly in terminal-backed threads in Zed.
---

# Terminal Threads

Terminal Threads are terminal-backed threads in the [Threads Sidebar](./parallel-agents.md#threads-sidebar). Use them when you want to run an agent CLI or TUI directly in Zed.

Terminal Threads are different from [External Agents](./external-agents.md). External Agents integrate with Zed through ACP and render as agent threads. Terminal Threads run the native command-line tool in a terminal that Zed organizes as a thread.

## What Zed Owns {#what-zed-owns}

Zed owns the thread surface:

- the terminal-backed thread in the Threads Sidebar
- thread grouping by project
- switching and organizing the terminal session alongside other threads

## What the CLI Owns {#what-the-cli-owns}

The CLI or TUI running inside the terminal owns its own:

- authentication
- model/provider configuration
- subscriptions or API keys
- tool configuration
- skills and instruction files
- MCP configuration

[Zed Agent profiles](./agent-profiles.md), Zed Agent tool permissions, Zed Skills, and Zed Agent MCP settings do not automatically apply to Terminal Threads.

## Opening a Terminal Thread {#opening-a-terminal-thread}

Open the new-thread menu from the [Agent Panel](./agent-panel.md) using the agent selector button on the left or the `+` icon in the top-right of the panel toolbar, then choose **Terminal**. The Terminal Thread opens in the panel body, just like switching to an agent thread.

You can open as many Terminal Threads as you like. Each gets its own entry in the Threads Sidebar.

## Running a Command Automatically {#terminal-thread-init-command}

If you always run the same CLI in Terminal Threads, set the `agent.terminal_init_command` setting to have Zed run a command automatically whenever Zed creates a Terminal Thread shell:

```json [settings]
{
  "agent": {
    "terminal_init_command": "claude"
  }
}
```

The command is sent to the shell as if you had typed it, so it is interpreted by your configured shell—including on Windows and in remote or WSL projects—and the terminal remains a regular interactive shell after the command exits. It runs when creating a new Terminal Thread and when recreating a saved Terminal Thread after reopening a project.

You can also configure this from the Settings UI under **AI**, via the "Terminal Thread Init Command" field.

## Terminal Thread Titles {#terminal-thread-titles}

The terminal title in the toolbar updates automatically to reflect the running shell or process. You can set a custom name by clicking the title or the pencil icon that appears on hover. In the Threads Sidebar, right-click a Terminal Thread and select **Rename Title**, or select it and press {#kb agent::RenameSelectedThread}.

To edit the title of the active Terminal Thread from the Agent Panel, custom-map {#action agent::RenameSelectedThread} in your `keymap.json`. Its default binding is scoped to the Threads Sidebar.

## Notifications {#terminal-thread-notifications}

When a background terminal produces a bell character, Zed can show an attention
indicator, a visual pop-up and an optional sound. Focusing the terminal content
acknowledges its tab indicator and Threads attention without requiring a
keypress. A Bell while the terminal already has focus in the active window does
not leave an unread tab indicator.

Threads also considers a terminal seen when its surface is visible in the active
window, even if keyboard focus is elsewhere. Opening the Threads list can
suppress pop-ups. Selecting a tab in an inactive window does not acknowledge it.

The same `agent.notify_when_agent_waiting` and `agent.play_sound_when_agent_done` settings apply.

The CLI must emit a terminal Bell for this route to work. Desktop notifications
and OSC notification sequences are not necessarily standalone Bell characters.
Zed does not infer response completion from the text printed by a CLI.

## Closing Terminal Threads {#closing-terminal-threads}

Unlike agent threads, Terminal Threads are closed rather than archived. They do not go to Thread History. To close one, hover over it in the Threads Sidebar and click the **×** button, or select it and press {#kb agent::ArchiveSelectedThread}.

## CLI/TUI Setup Notes {#cli-setup}

Some agent CLIs and TUIs can send terminal signals, such as bell notifications or title updates, that Zed uses to show useful context in the sidebar.

### Claude Code Notifications {#claude-code-notifications}

Claude Code's built-in notifications can use a terminal Bell. Set
`preferredNotifChannel` to `"terminal_bell"` in your Claude Code user settings:

```json
{
  "preferredNotifChannel": "terminal_bell"
}
```

You can also set this from within Claude Code by running `/config`, selecting `Local Notifications`, and choosing `Terminal Bell`.

> If you run Claude Code inside tmux, bell notifications may not reach the outer terminal unless passthrough is enabled. Add this to `~/.tmux.conf`:
>
> ```
> set -g allow-passthrough on
> ```

For more, see the [Claude Code documentation](https://code.claude.com/docs/en/terminal-config).

Claude's built-in idle notification can arrive around 60 seconds after a response
if you have not typed since. For an immediate completion Bell on Windows, copy
`script/claude-terminal-bell.ps1` from this repository to
`~/.claude/hooks/suprzed-terminal-bell.ps1`. The script returns
`{"terminalSequence":"\u0007"}`; Claude emits the Bell through its interactive
terminal. Writing a raw Bell to a hook's captured stdout does not do this.

Existing hook installations keep working under their previous filename. If you
rename an installed script, update its configured hook command to match.

Merge the following into your Claude settings, appending to existing hook arrays
and replacing the example path with your own absolute path. Preserve existing
hooks. Disable the built-in notification channel to avoid a delayed second Bell:

```json
{
  "preferredNotifChannel": "notifications_disabled",
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File C:/Users/YOUR_USER/.claude/hooks/suprzed-terminal-bell.ps1",
            "timeout": 5
          }
        ]
      }
    ]
  }
}
```

For immediate permission and failed-response alerts, append the same hook group
to `PermissionRequest` and `StopFailure`. These hooks only notify; they do not
approve permissions. To preserve other notification types, also add the group
to `Notification` with matcher `^(?!idle_prompt$|permission_prompt$).+`. This
excludes the delayed idle notification and the permission notification already
handled by `PermissionRequest`. Quote the script path inside the command if it
contains spaces. Restart Claude after changing its settings.

See the [Claude hook reference](https://code.claude.com/docs/en/hooks) for event
semantics and `terminalSequence` support in your installed version. The hook's
execution time is not a guarantee of total response-to-notification latency.

### Copilot CLI Notifications {#copilot-cli-notifications}

Copilot CLI's `beep` default depends on the installed version. To receive
terminal Bells, merge this preference into `~/.copilot/settings.json`, preserving
its other settings:

```json
{
  "beep": true
}
```

Use `copilot help config` to check the installed version's settings. Check for a
legacy `config.json` override as well, then restart Copilot. Its desktop
notification preferences are separate from `beep`.

### Hermes Notifications {#hermes-notifications}

Hermes completion and prompt Bells are disabled by default. In the active Hermes
profile's `config.yaml`, merge these settings into the existing `display` section:

```yaml
display:
  bell_on_complete: true
  bell_on_prompt: true
```

Use the same Hermes home and profile as the terminal session, then restart that
CLI to load its changed settings. Enabling notifications should not relocate its
session store. Zed does not change these provider preferences automatically.

### Amp Notifications {#amp-notifications}

Amp updates terminal titles automatically and can also notify you when it needs your attention. To enable notifications in Zed Terminal Threads, add `AMP_FORCE_BEL=1` to your terminal environment settings:

```json [settings]
{
  "terminal": {
    "env": {
      "AMP_FORCE_BEL": "1"
    }
  }
}
```

Restart Amp after adding the environment variable.

### OpenCode Notifications {#opencode-notifications}

OpenCode can update terminal titles automatically. For Zed notifications, add an OpenCode plugin that emits a terminal bell when OpenCode needs your attention.

Create `.opencode/plugins/zed-bell.js` in your project, or `~/.config/opencode/plugins/zed-bell.js` to use it globally:

```js
export const ZedBell = async () => {
  return {
    event: async ({ event }) => {
      if (process.env.OPENCODE_CLIENT === "acp") return;

      if (event.type === "session.idle" || event.type === "permission.asked") {
        process.stdout.write("\x07");
      }
    },
  };
};
```

Restart OpenCode after adding the plugin.

### Pi Notifications {#pi-notifications}

Pi can use an extension to emit a notification when it finishes a turn. Create `.pi/extensions/zed-bell.ts` in your project, or `~/.pi/agent/extensions/zed-bell.ts` to use it globally:

```ts
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
  pi.on("agent_end", async () => {
    process.stdout.write("\x07");
  });
}
```

Restart Pi after adding the extension, or run `/reload` if the extension is in one of Pi's auto-discovered extension locations.

### Codex Terminal Titles {#codex-terminal-titles}

Codex can update the terminal title as it works, which Zed uses to show useful context for Codex Terminal Threads in the sidebar, such as the project, current status, branch, model, or task progress.

To configure this from within Codex, run `/title` and use the picker to choose which fields appear and in what order. Codex saves the selection to `tui.terminal_title` in `~/.codex/config.toml`. You can also edit it directly:

```toml
[tui]
terminal_title = ["spinner", "project-name", "run-state", "thread-title"]
```

## Resumable Codex terminals in suprZed {#resumable-codex-terminals}

Create a resumable Codex terminal from the agent terminal menu. suprZed asks you
to review its identity hooks when they need installing or updating. Codex may
also ask you to trust those hooks.

A new terminal can wait for your first prompt without a capture timeout. If you
restart suprZed before a conversation has been captured, the terminal opens a
fresh Codex session. Once an identity is captured, it resumes that exact
conversation.

After switching conversations with Codex's `/resume`, send a prompt in the
selected conversation to update what suprZed saves. Opening another conversation
without sending a prompt keeps the previous saved association. The same rule
applies when you return to an earlier conversation. Prompt text is removed by
the identity helper before it sends a callback to suprZed. Capture stays with the
first hook-calling process that provides a valid callback. Its executable name
is not required to be `codex.exe`. After that capture, another
Codex process started inside or after that terminal does not replace its saved
conversation. Use **Start New AI Session** to launch a new tracked process.

When Codex supplies an ordered prompt submission ID, suprZed uses it to ignore
older prompts whose hooks arrive after a newer prompt. Codex versions that do not
supply that signal use hook invocation timing; that fallback cannot detect a
hook the provider dispatches late.

If the helper cannot identify the hook caller, suprZed rejects the callback
and reports a capture warning. The saved conversation stays unchanged.
Malformed callbacks also show this warning, once per launch. Dismissing it
keeps later failures from reopening it. Valid callbacks can still save the
conversation.

A saved conversation is kept if its resume fails. Use **Start New AI Session**
in the terminal's context menu to replace it explicitly, or **Forget AI Session** to stop saving it. Moving the terminal between the centre and the
dock keeps its saved conversation.

## Credentials and Remote Projects {#credentials-and-remote-projects}

Credentials come from the terminal session and the CLI/TUI running inside it.

In remote projects, the CLI may read the remote shell environment and remote config files. In local Terminal Threads, it reads the local shell environment and local config files. Zed does not copy [API keys from LLM provider settings](./use-api-access.md) into Terminal Threads.

## When to Use Terminal Threads {#when-to-use-terminal-threads}

Use Terminal Threads when:

- you want the tool's native CLI/TUI experience
- no ACP integration exists
- you want subscription behavior owned by the CLI
- you want the CLI to use its own native config files

For ACP-integrated agents, see [External Agents](./external-agents.md).
