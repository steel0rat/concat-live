# Concat Live for AI agents (MCP)

Concat ships an [MCP](https://modelcontextprotocol.io) server inside the app. An agent - Claude Code, Claude Desktop, or any other MCP client - starts it as `concat --mcp` and can then create projects, import media, cut, add titles and captions, keyframe, apply effects, look at frames and export, with the same edit commands the window uses. Nothing extra to install.

## Set it up

Start this fork's desktop window, then point the client at its binary with `--mcp window`. Check that `concat_version` reports `mcp.mode: window` and includes `live` in `capabilities`.

| System | Command |
| --- | --- |
| macOS | `/Applications/Concat Live.app/Contents/MacOS/concat` |
| Windows (installed for you) | `%LOCALAPPDATA%\Programs\Concat\concat.exe` |
| Windows (installed for everyone, or the .msi) | `C:\Program Files\Concat\concat.exe` |
| Linux (.deb, .rpm, Arch) | `/usr/bin/concat` |
| Linux (AppImage) | the `.AppImage` file itself |

**Claude Code:**

```sh
claude mcp add concat -- "/Applications/Concat Live.app/Contents/MacOS/concat" --mcp window
```

**Claude Desktop** (Settings > Developer > Edit Config, `claude_desktop_config.json`), and most other clients, which take the same shape:

```json
{
  "mcpServers": {
    "concat": {
      "command": "/Applications/Concat Live.app/Contents/MacOS/concat",
      "args": ["--mcp", "window"]
    }
  }
}
```

On Windows, write the path with doubled backslashes in JSON: `"C:\\Program Files\\Concat\\concat.exe"`.

## How it reaches Concat

`concat --mcp` works one of two ways, and the `concat_version` tool says which (`mcp.mode`):

- **Through the live window** (`window`, capability `live`). Desktop launches of this fork enable local Automation on a random loopback port without changing saved preferences. Its private `automation.json` lets MCP discover the window. Project requests run on the GUI event loop against its sole session: an agent edit appears on the timeline, GUI undo undoes it, and API reads see GUI edits. Export and preview use a detached snapshot of current unsaved state; exports still share the window's slot.
- **On its own** (`headless`). Otherwise the MCP server runs Concat's engine itself, with no window. Nothing has to be open. Every Concat process locks the project folders it has open (`concat.lock`, let go on close or when the process dies), so a project open in the window is refused here with a note to turn on Automation and use `--mcp window`, and the window refuses a project an agent has open.

Choose explicitly with `--mcp window` (fail rather than run on its own) or `--mcp headless`; the default is `--mcp auto`. To reach a server elsewhere, set `CONCAT_API_ADDRESS` (e.g. `127.0.0.1:7420`) and `CONCAT_API_TOKEN` in the client's `env` for the server.

Either way the agent writes files - projects, exports, frames - only under your home folder. Set `CONCAT_MCP_ROOTS` (a path list, `:`-separated, `;` on Windows) to allow other folders. In this fork's live mode, pass it to the GUI process at launch; changing only the MCP client's environment does not change a running window's write roots.

Live mode is implemented in this fork; upstream v0.2.10 does not provide it.
`project_create` and `project_open` show the project in the window. Requests
name an absolute project directory and are refused when it is not the visible
one. A queued call is also checked against the window's project epoch.
Finish an active drag or confirmation before requesting an agent edit.

Call `project_save` before switching projects. A live close requires `save: true`. `save: false` is always refused,
even when the project looks clean, so an in-flight autosave cannot cause
the sole visible session to be discarded. The save reply follows the
ordered disk write; an older save cannot clear a later edit's dirty state.

A mutation is never automatically replayed after a lost transport response.
Its outcome can be unknown: inspect the timeline before deciding what to do
next. Read-only requests may reconnect and retry.

## What the agent can do

Tools are named for what they do, and their argument schemas are generated from the Concat API's own types, so they carry the same limits and descriptions the engine does.

| Area | Tools |
| --- | --- |
| Projects | `project_list`, `project_create`, `project_open`, `project_save`, `project_close`, `project_set_video` |
| Reading | `timeline_read` (compact: lanes, clips in time order, media, ids), `project_get` (everything), `concat_version` |
| Media | `media_probe`, `media_import` |
| Editing | `clip_add`, `clips_add_sequence`, `clip_trim`, `clips_split`, `clips_move`, `clips_remove`, `clip_update`, `clip_transform`, `clip_speed`, `clip_frame`, `track_add` |
| Picture | `clip_mask`, `clip_stabilize`, `timeline_background` |
| Titles and captions | `title_add`, `title_style_words` (some words in another colour, weight, size...), `title_clear_word_styles`, `captions_add` |
| Keyframes and effects | `keyframe_set`, `keyframe_clear`, `effects_list`, `effect_layer_add`, `effect_key_set` |
| Anything else | `edit_apply` (any edit command, or a batch of them as one undo step), `edit_undo`, `edit_redo` |
| Templates | `template_list`, `template_instantiate` |
| Export | `export_start`, `export_status`, `export_wait` (with progress), `export_cancel` |
| Looking | `preview_frame` (the composited frame as an image) |

Edits stay in memory until `project_save`, as in the window. Times are seconds; a keyframe's position is a fraction of its clip (0 to 1); sizes and offsets are fractions of the frame.

## Protocol

MCP over stdio, newline-delimited JSON-RPC 2.0. Both protocol eras are served: the `initialize` handshake (2025-11-25, 2025-06-18, 2025-03-26, 2024-11-05) and the per-request `_meta` of 2026-07-28 with `server/discover`. `ping`, `tools/list`, `tools/call`, progress notifications for `export_wait`, and `notifications/cancelled` are supported. Standard output carries only protocol; the server logs to standard error.

For scripts that do not speak MCP, the same API is served as JSON-RPC lines on a socket from Settings > Automation; see [the developer docs](https://concat.standalones.org/docs).
