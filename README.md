# pipeweaver-lv2ui

Open the native UI of an LV2 filter that is running inside
[Pipeweaver](https://github.com/pipeweaver/pipeweaver), and edit it live.

Pipeweaver hosts LV2 plugins in its daemon and configures them through its own
web pages. This tool opens the plugin's real UI in a window instead. Controls
you move are applied to the running filter immediately, changes made in
Pipeweaver's web UI move the knobs here, and meters and graphs are live.

It is a separate program that talks to Pipeweaver over its normal websocket
API. Nothing in Pipeweaver is modified. It is an unofficial companion, not
part of or affiliated with the Pipeweaver project.

## AI-generated: use with care

This tool was written by an AI coding assistant (Claude) under the author's
direction. The author tested that it works but has not reviewed the code line
by line. Treat it accordingly:

- It has only been run on one machine, against LSP plugins.
- It is mostly `unsafe` Rust talking to C libraries, and it loads and runs
  plugin code inside its own process. Bugs here can crash it.
- It changes settings on filters in your live audio chain. Try it on a setup
  you can afford to have misbehave before relying on it.
- It comes with no warranty, as the license says.

Bug reports are welcome. Read the code before trusting it with anything that
matters.

## Requirements

- Pipeweaver running, with its web server on `localhost:14565`
- The plugin installed as LV2 with an X11 UI (LSP plugins: `lsp-plugins-lv2`)
- An X server. On Wayland that means XWayland, which KDE and GNOME run by default.
- `pw-metadata` (part of PipeWire's tools), used to read the graph's sample rate

## Building

You need Rust 1.88 or newer (via [rustup](https://rustup.rs) or your
distribution), and the libraries and tools the build links against and
generates bindings from: `lilv`, `pipewire`, `clang` and `pkg-config`.

```sh
# Arch, CachyOS
sudo pacman -S --needed rust lilv pipewire clang pkgconf

# Debian, Ubuntu
sudo apt install cargo liblilv-dev libpipewire-0.3-dev libclang-dev pkg-config

# Fedora
sudo dnf install cargo lilv-devel pipewire-devel clang-devel pkgconf-pkg-config
```

Then, in this directory:

```sh
cargo build --release
```

The program is `target/release/pipeweaver-lv2ui`. Run it from there, or
install it to `~/.cargo/bin` with:

```sh
cargo install --path .
```

## Usage

```sh
pipeweaver-lv2ui                  # list the filters and ask which one to open, until you answer q
pipeweaver-lv2ui list
pipeweaver-lv2ui open 2           # by number from `list`
pipeweaver-lv2ui open compressor  # by part of the device or plugin name
pipeweaver-lv2ui open 01M3ZB9YNC3KTK2BTAWWD8Q1S4
```

Close the plugin window (or press Ctrl-C) to finish. Run without arguments,
closing a window brings the list back so you can open another; `q` quits.
Several filters can be open at once, one process each.

Only LV2 filters are listed. Pipeweaver's built-in ones (noise suppression,
bleep) have no LV2 UI, so they are named under the list but can't be opened.

Options for `open`:

- `--no-tap`: don't feed live audio to the UI. Controls still work; meters and graphs stay still.
- `--no-instance-access`: don't give the UI direct access to the plugin instance, for UIs that misbehave with it.
- `--url ws://host:port/api/websocket`: a daemon somewhere other than the default.

## How it works

Pipeweaver's daemon only exposes a plugin's control ports. It has no channel
for the meter and graph data a native UI wants, and no way to host a UI. So
this tool runs a second copy of the plugin, the *shadow*, and attaches the UI
to that:

```
 native UI  <──────────────>  shadow plugin instance
     │                         (a PipeWire filter node; hears the same audio
     │ control changes          as the real filter, output goes nowhere)
     ▼
 Pipeweaver daemon ──> the real filter, which is what you actually hear
     ▲
     └ changes made in the web UI come back and move the knobs here
```

- The shadow's inputs are linked to whatever feeds the real filter, so it
  sees the same signal with the same settings and its meters and graphs match.
  The links are owned by this process, and PipeWire removes them when it exits.
- The shadow node has no media class and does not autoconnect, so neither the
  session manager nor Pipeweaver's application list picks it up.

## Limitations

- Only plugins with an X11 UI are supported, and only LSP's have been tested.
- Only what Pipeweaver stores is saved: control port values. Anything else a
  plugin keeps in its own state (file paths and other non-control settings)
  is not carried over to the real filter.
- Meters and graphs come from the shadow, not from the real instance. They
  match because the input and settings match, but history-dependent displays
  start fresh each time you open the window.
- The plugin runs twice while its window is open.
- If the same plugin is on one device more than once, the audio tap picks the
  matching node by its position; controls always go to the right filter.

## License

MIT, see [LICENSE](LICENSE).

Because the code is AI-generated, some or all of it may not be protected by
copyright in some countries, in which case you need no license to use it
there. Where copyright does apply, the MIT license is the grant.

The shadow's PipeWire filter node is set up the way Pipeweaver sets up its own
filters, with its source as the reference. Pipeweaver is MIT licensed,
copyright 2025 Craig McLure.
