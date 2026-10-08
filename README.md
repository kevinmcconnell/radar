# Radar

See how busy your machine has been, and why.

![Radar showing CPU, temperature, memory and network charts and top processes](.github/assets/screenshot.png)

Radar is a small system activity recorder and viewer for a single-user Linux
desktop. It has two parts:

- **`radar-collect`** samples CPU, temperatures, memory, network, disk and
  per-process CPU time every five seconds into a SQLite database. It runs as
  a systemd user service, reads only `/proc`, `/sys` and the local logs of
  AI coding agents, and keeps a rolling window of five days.
- **`radar`** is a GTK 4 / libadwaita app. Pick a time range and see charts of
  that activity, plus the processes that used the most CPU in that range.

On Omarchy, the app follows the active theme. Elsewhere it uses the Adwaita
look and the desktop's light or dark preference.

## Building

Radar is written in Rust and needs the `gtk4`, `libadwaita` and `sqlite`
libraries plus `pkgconf`. On Arch:

```
sudo pacman -S --needed rust pkgconf gtk4 libadwaita sqlite
```

Then:

```
make install
```

This builds in release mode, installs both binaries to `~/.local/bin`,
installs the desktop file, icon and user service, starts the collector, and
sets up the [Claude Code status line](#ai-agents).
Run `make uninstall` to remove it all again.

## Data

The database lives at `~/.local/share/radar/radar.db`. Run
`radar-collect --help` for the sampling interval and retention flags, and
`radar --help` for the viewer's options.

## AI agents

Radar can chart how much of your Codex and Claude Code rate limits remains,
and the tokens each agent is consuming. Neither needs network access or
credentials. Both cards appear once an agent has reported something.

**Codex** needs no setup. The collector tails the session logs under
`~/.codex/sessions` (or `$CODEX_HOME/sessions`), which carry a quota snapshot
and a token count with every response.

**Claude Code** does not persist its quota anywhere, but it pipes the data to a
status line command after every assistant message. `make install` puts
`radar-claude-statusline` in `~/.local/bin`; it saves that JSON under
`~/.local/state/radar/claude/`, one line per update, and prints nothing.

`make install` also runs `radar configure claude`, which sets the status line
in `~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR/settings.json`):

```json
{
  "statusLine": {
    "type": "command",
    "command": "radar-claude-statusline"
  }
}
```

If the helper's directory is not on your `PATH`, it writes the helper's full
path instead. It is safe to run again: it adds only that key, leaves the rest
of the file as it was, and does nothing if it is already set. If you already
have a different status line it leaves it alone and warns; add a line to your
script that pipes its input through `radar-claude-statusline` as well.

`make uninstall` runs `radar remove claude`, which takes the key out again if
it still runs only `radar-claude-statusline`, and leaves any other status line
alone.

The rate-limit fields appear only on claude.ai Pro and Max subscriptions.

## Releasing

The version is set in one place, `[workspace.package]` in `Cargo.toml`.

1. Set the version in `Cargo.toml`, then run `cargo build` to update
   `Cargo.lock`.
2. Commit the two files as "Release radar 0.2.0".
3. Tag the commit `v0.2.0` and push the commit and the tag.

CI checks that the tag matches the version, runs the checks, and publishes
the GitHub release.

## License

MIT. See [LICENSE](LICENSE).
