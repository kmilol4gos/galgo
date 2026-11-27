# galgo

**A fast, good-looking terminal for macOS.** Open a shell, organize your work into workspaces and tabs, and split the screen when you need more room. You do not need to use coding agents to use galgo.

Galgo is alpha software. Agent orchestration is optional and experimental; it has not been validated with real agents as a complete workflow. The [Spanish user guide](docs/guia-uso.md) covers the wider project, including the experimental tools.

## Install from source

You need macOS, [Rust and Cargo](https://rustup.rs/), Python 3, Swift, and the macOS command-line tools. From a checkout:

```sh
git clone https://github.com/kmilol4gos/galgo.git
cd galgo
scripts/install.sh
export PATH="$HOME/.galgo/bin:$PATH"
scripts/make-app.sh
open galgo.app
```

The installer builds and installs `galgo`, `galgod`, `galgo-mcp`, and `galgo-gui` in `~/.galgo/bin`. It does not edit your shell configuration; add the PATH line yourself to make it permanent. `make-app.sh` bundles all four binaries into a standalone `galgo.app`, which you can move to Applications. It will not overwrite an existing app.

`PREFIX` selects the **binary directory itself**. Use the same value when building the app:

```sh
PREFIX="$HOME/bin" scripts/install.sh
PREFIX="$HOME/bin" scripts/make-app.sh --output "$HOME/Applications/galgo.app"
```

See `scripts/install.sh --help` and `scripts/make-app.sh --help` for options. The app connects to a background daemon or starts one. If an older daemon is already running, it keeps its old version and environment; save your work before `galgo stop`, which closes its panels.

## Keyboard shortcuts

On macOS, with no dialog taking keyboard focus:

| Shortcut | Action |
| --- | --- |
| ⌘ T | New tab in the current workspace |
| ⌘ W | Close focused panel (may ask for confirmation) |
| ⌘ D / ⌘ ⇧ D | Split right / down |
| ⌘ ⌥ arrow | Focus a neighboring split |
| ⌘ ⇧ Return | Maximize / restore the focused split |
| ⌘ [ / ⌘ ] | Previous / next tab |
| ⌘ 1–9 | Select workspace |
| ⌘ ⇧ N | New named workspace |
| ⌘ ⇧ P | Command palette |
| ⌘ K | Clear screen |
| ⌘ F | Search terminal |
| ⌘ B | Toggle sidebar |
| ⌘ , | Settings |
| ⌘ C / ⌘ V | Copy selection / paste |

## Optional agent orchestration

Galgo can coordinate agent panels through its CLI, hooks, and MCP server. This is **experimental and has not been validated as an end-to-end workflow with real agents**; do not rely on it for unattended work. Hooks are opt-in and only report from galgo panels. See the [Spanish guide](docs/guia-uso.md) and [hook notes](docs/hooks-agents.md) for setup, coverage, and limitations. OpenCode hooks are not implemented.

## Known limitations

- Only macOS has been tested; Linux is not verified.
- The app is unsigned and not notarized. Gatekeeper may block a downloaded build; review macOS security prompts before opening it.
- The repository vendors patched copies of `iced_winit` and `vt100` under `vendor/`.
- The app launcher invokes a login shell to obtain PATH. A daemon that was already running retains its existing environment.
- Release archives target the architecture named in the archive, not a universal binary. See [release instructions](docs/release.md).

## License

MIT or Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
