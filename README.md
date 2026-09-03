# Xomposite

Xomposite is a modern, lightweight X11 compositor written in Rust from scratch. Developed and tested primarily with **XLibre** and **i3**, it replaces legacy compositor stacks with a clean, analytical GPU-driven rendering pipeline and strict transactional resource management.

> [!NOTE]
> Xomposite is a compositor, not a window manager. Window placement, tiling, and keyboard navigation remain the responsibility of i3 or another window manager.

## Why Xomposite?

- **Real-world stress test for XLibre:** Rather than relying on changelogs or synthetic tests, Xomposite was engineered to put the XLibre XServer fork through its paces under demanding desktop workloads using `XComposite`, `XDamage`, `DRI3`, and `Present`.
- **Written from scratch in Rust:** Free from 20+ years of accumulated legacy hacks found in traditional X11 compositors. Xomposite speaks directly to X11 protocols via `x11rb`, manages EGL contexts via `khronos-egl`, and renders directly in OpenGL—with zero intermediate bloated compositor frameworks.
- **Analytical GPU rendering:** Rounded corners, window borders, drop shadows, and background blurs are evaluated analytically inside GL shaders rather than using pre-rendered sprite slices or CPU blits.
- **Transactional scene lifecycle & early obsolescence checks:** Window hierarchy updates and pixel damage are staged transactionally. By detecting obsolete geometry before expensive resource acquisition, Xomposite eliminates stale-candidate backpressure and stutter during rapid floating window resizes.
- **Predictable, application-driven blur:** Background blur is strictly opt-in via the standard `_KDE_NET_WM_BLUR_BEHIND_REGION` property. The compositor never guesses or forces blur based on opacity heuristics.

## Current status

The current baseline is usable as the author's daily X11 compositor and has passed automated
validation, human configuration acceptance, and human i3 startup acceptance. It is now
entering multi-day daily-driver soak. Xomposite remains experimental and under active
development; it is not presented as production-ready or universally compatible with every
X11 server, application, or visual configuration.

## Implemented features

- X11 Composite ownership and automatic root discovery from the selected X11 screen.
- Normal startup without a manually supplied root XID.
- XDamage-driven scene updates with EGL/OpenGL rendering.
- Rounded corners, configurable borders, shadows, and opacity configuration.
- Background blur for application blur requests.
- Sparse startup configuration at `~/.config/xomposite/xomposite.conf`.
- Startup through i3 using a normal `exec` entry.
- Transactional scene-candidate publication with bounded stale-candidate handling.

The implementation is still being validated against the author's XLibre+i3 session and
should be treated accordingly.

## Prerequisites & Installation

### System Requirements

- **Linux** running an X11 server with DRI3 and EGL support (e.g., **XLibre** >= 25.1 or modern Xorg).
- **Rust toolchain:** Latest stable Rust/Cargo (2024 edition support).
- **System development libraries:**
  - `libxcb` and X11 development headers (including `composite`, `damage`, `dri3`, `present`, `randr`, `xfixes`).
  - `libEGL` and `libGL` (Mesa / GPU vendor drivers).

#### Installing System Dependencies

On Arch Linux / Artix Linux:
```bash
sudo pacman -S base-devel rust libxcb libx11 mesa
```

On Debian / Ubuntu:
```bash
sudo apt install build-essential cargo libxcb1-dev libxcb-composite0-dev libxcb-damage0-dev libxcb-dri3-dev libxcb-present-dev libxcb-randr0-dev libxcb-xfixes0-dev libegl1-mesa-dev libgl1-mesa-dev
```

### Building

To build the development binary:

```bash
cargo build --locked
```

To build an optimized release binary for daily use:

```bash
cargo build --release --locked
```

The resulting executables are:
- Development: `target/debug/xlibre-compositor-poc`
- Release: `target/release/xlibre-compositor-poc`

### Installing

To install the binary directly to your Cargo binary path (`~/.cargo/bin`):

```bash
cargo install --path . --locked
```

## Running

Normal startup discovers the X11 root automatically:

```bash
./target/debug/xlibre-compositor-poc
```

An explicit configuration file can be selected with either form:

```bash
./target/debug/xlibre-compositor-poc --config /path/to/xomposite.conf
./target/debug/xlibre-compositor-poc --config=/path/to/xomposite.conf
```

The old `--compositor-scene-x11-probe ROOT_XID` option remains an optional development
diagnostic override; it is not required for normal operation.

## Configuration

Configuration selection precedence is:

```text
--config PATH
    > $XDG_CONFIG_HOME/xomposite/xomposite.conf
    > $HOME/.config/xomposite/xomposite.conf
    > built-in defaults
```

An absent implicit configuration file is valid and uses built-in defaults. An explicitly
requested missing file, or an existing invalid file, fails clearly during startup. The file
uses sparse overrides: omitted keys inherit built-in defaults.

A minimal daily-driver configuration is:

```toml
[global]
corner.radius = 16
border.width = 2
border.inactive_color = 555555
border.focused_color = 4C7899
border.urgent_color = FF3030
blur.enabled = true
```

The current parser also accepts global `shadow.*` and `opacity.*` settings. Window rules are
parsed and validated but are not yet applied to live windows.

## Blur request semantics

`blur.enabled` is compositor permission, not an instruction to blur every transparent window.
Xomposite never infers blur from transparency and never synthesizes an application request
because `blur.enabled = true`.

```text
application does not request blur                  -> no blur
application requests blur + blur.enabled = true   -> request may be honored
application requests blur + blur.enabled = false  -> request is denied
```

Applications request their own blur through the X11 property
`_KDE_NET_WM_BLUR_BEHIND_REGION`. The current implementation supports the following request
shapes:

- an empty property or one zero-size rectangle requests full-window blur;
- one or more `x, y, width, height` rectangles request blur for those client-local regions.

For example, Ghostty is a tested application-request case. A property-absent application
does not receive blur merely because it is transparent or because global permission is on.

### Taskbar or panel example

A custom X11 taskbar or panel can choose its own transparency and place
`_KDE_NET_WM_BLUR_BEHIND_REGION` on its own X11 window. The application makes the request;
the compositor decides whether to honor it according to global permission. No compositor
class-matching or application-specific hardcode is required.

## Starting with i3

After building, an i3 configuration can start the compositor once per i3 session. Use `exec`,
not `exec_always`, so an i3 configuration reload does not intentionally spawn a duplicate
compositor:

```i3
exec --no-startup-id sh -c 'mkdir -p "$HOME/.local/state/xomposite" && exec /absolute/path/to/xlibre-compositor-poc >> "$HOME/.local/state/xomposite/xomposite.log" 2>&1'
```

Replace the executable placeholder with the desired absolute path. When launched inside the
graphical i3 session, the inherited `DISPLAY` and `XAUTHORITY` environment is normally used;
the compositor does not require user-specific paths in its configuration.

## Logging and troubleshooting

For a development run, create a persistent log directory and capture both output streams:

```bash
mkdir -p ~/.local/state/xomposite
./target/debug/xlibre-compositor-poc \
  >> ~/.local/state/xomposite/xomposite.log 2>&1
```

Development builds may be verbose. If startup fails, inspect the log for configuration
errors, X11 compositor-selection ownership errors, or scene-candidate lifecycle messages.
Starting a second compositor while another owns the compositor selection should fail cleanly.

## Current limitations

- Multi-monitor and dual-monitor behavior still needs dedicated validation and support work.
- Configuration is loaded at startup; live reload is not implemented.
- Window rules are parsed and stored, but runtime rule application is deferred.
- Transitions and animations are not implemented.
- The current workflow uses a development binary; stable installation and packaging are future
  work.
- Logging is functional but does not yet provide a polished structured-level interface.

## Development and lifecycle notes

Scene candidates are built and published transactionally: the current live scene remains
authoritative until a replacement succeeds. Startup also handles the period before the first
scene has been published while hierarchy changes are still arriving. XDamage, named-pixmap,
and EGL resources use ownership-aware cleanup, and stale candidates are bounded rather than
published speculatively.

## Roadmap

1. Multi-day daily-driver soak.
2. Multi-monitor validation and support.
3. Transitions and animations.
4. Improved logging and a stable installation workflow.
5. Live configuration reload and runtime window-rule integration.

## Contributing

Contributions, issues, and discussions are welcome!

### Development Principles

- **Design & Audit First:** Architecture decisions, protocol handshakes, and lifecycle changes should be documented and audited before landing invasive patches.
- **Strict RAII & Zero Leaks:** Manual leak workarounds (such as `std::mem::forget`) are prohibited. All GL, EGL, and X11 resources use deterministic ownership-transfer semantics.
- **Automated Verification:** Always ensure the test suite passes before submitting changes:
  ```bash
  cargo test --locked
  ```
- **Target Environment:** When reporting bugs or proposing features, please specify your X server version (`XLibre` or `Xorg`), GPU driver, and window manager.
