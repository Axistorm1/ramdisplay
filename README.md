# ramdisplay

A high-performance desktop widget written in Rust that visualises live system
memory pressure as a full-screen **Matrix-style "digital rain"** — rendered
**on the Wayland background layer**, above your wallpaper and strictly behind
every normal window.

<br>

## How it satisfies the brief

| Requirement | Implementation |
|---|---|
| **Layer shell / background layer** | `gtk4-layer-shell` places the window on `Layer::Background`. It sits above the wallpaper and below all regular windows, reserving no space (`exclusive_zone = 0`). |
| **True RGBA transparency** | The surface is ARGB (Wayland), the GTK window background is made fully transparent via CSS, and each frame starts with `cairo` `Operator::Clear`, so the wallpaper shows through. |
| **Input pass-through** | An **empty `GdkSurface` input region** is installed, so every pointer event falls through to whatever is behind. `KeyboardMode::None` means it never takes keyboard focus. |
| **No decorations / no taskbar / no focus steal** | Layer-shell surfaces have no decorations or taskbar entry by default; `set_decorated(false)` and `KeybordMode::None` make the intent explicit. |
| **Efficient metrics** | A dedicated `std::thread` polls `/proc/meminfo` (RAM, swap, cache), parsing into a reused buffer with **no per-sample allocation**, and publishes values through lock-free `AtomicU64`s. |
| **Smooth animation** | Rendering is driven by GTK's frame clock (`add_tick_callback`), tied to the compositor's v-sync, with an explicit frame-rate cap. Cairo + monospace glyphs + additive bloom. |
| **Dynamic reactivity** | Fall speed, trail length, stream **density**, head **glow** and the **colour gradient** (green → amber → red) scale continuously with exponentially smoothed RAM / swap / cache fractions. |
| **Data-driven glyphs** | Streams render real data symbols — **hex**, **binary** or **decimal** digits (`--glyphs`) — never random text. |
| **Full-screen canvas** | The surface is anchored to all four edges, so the rain seamlessly covers the entire viewport (100vw × 100vh) and flows top-to-bottom without letterboxing. |

The effect is a grid of monospace glyph cells: each column is an independent
vertical stream of falling data with a bright head and a decaying trail. Every
visual property is a function of live memory pressure:

| Metric | Visual mapping |
|---|---|
| RAM usage (↑) | Faster fall speed, longer tails, more active columns (density), stronger bloom |
| RAM usage (↑) | Hue shifts green → yellow → red (cool & calm → warning) |
| Swap usage (↑) | Adds heat — pushes the palette toward red and lengthens trails |
| Page cache | Feeds a soft extra glow on the leading glyphs |

If `/proc/meminfo` is unavailable the reader thread transparently falls back to
a built-in simulator (also forceable with `--simulate`) that produces the same
continuously changing RAM figure.

<br>

## System dependencies

You need: a C toolchain, Rust (≥ 1.92), GTK 4 development headers, `pkg-config`,
and the **gtk4-layer-shell** library + headers. `gtk4-layer-shell` is a thin C
library that patches GTK4 window handling at runtime.

### Arch / Manjaro / EndeavourOS / CachyOS

```bash
sudo pacman -S --needed base-devel rust gtk4 gtk4-layer-shell pkgconf
```

### Fedora

```bash
sudo dnf install gcc gcc-c++ rust cargo gtk4-devel gtk4-layer-shell-devel \
                 pkgconf-pkg-config meson ninja-build git
```

### Debian / Ubuntu

Newer releases package it:

```bash
sudo apt install build-essential rustc cargo libgtk-4-dev \
                 libgtk4-layer-shell-dev pkg-config meson ninja-build git
```

On **Ubuntu 24.04 and older** `libgtk4-layer-shell-dev` is not available, so
build the (tiny) C library from source:

```bash
sudo apt install build-essential libgtk-4-dev pkg-config meson ninja-build git
git clone https://github.com/wmww/gtk4-layer-shell.git
cd gtk4-layer-shell
meson setup build --prefix=/usr/local -Dintrospection=false -Dvapi=false \
                  -Dexamples=false -Dtests=false
sudo ninja -C build install
sudo ldconfig
```

The bundled `install-deps.sh` does all of the above automatically:

```bash
./install-deps.sh
```

<br>

## Build & run

```bash
cargo build --release
./target/release/ramdisplay                  # full-screen digital rain (default)
./target/release/ramdisplay --glyphs binary  # use 0/1 streams
./target/release/ramdisplay --windowed --size 600   # compact widget instead
```

Copy the binary somewhere on your `PATH` to use it from your compositor config:

```bash
install -Dm755 target/release/ramdisplay ~/.cargo/bin/ramdisplay
```

### Options

```
-f, --fullscreen        Fill the whole monitor (100vw x 100vh) [default]
    --windowed          Run as a centered window instead (see --size)
    --size <px>         Window edge length in windowed mode [default: 380]
    --cell <px>         Rain glyph cell size [default: 18]
    --glyphs <set>      hex | binary | decimal [default: hex]
    --simulate          Force simulated RAM metrics (ignore /proc/meminfo)
    --fps <n>           Frame-rate cap [default: 60]
    --poll-ms <n>       /proc/meminfo polling interval [default: 500]
    --namespace <s>     Layer-shell namespace [default: ramdisplay]
    --monitor <n>       Place on monitor index n [default: compositor choice]
-h, --help              Print help
```

<br>

## Hyprland configuration

Add the following to `~/.config/hypr/hyprland.conf` (or source the provided
`hypr/ramdisplay.conf` from your `conf.d/` directory):

```ini
# Launch once when Hyprland starts.
exec-once = ~/.cargo/bin/ramdisplay --cell 18 --fps 60

# Optional layer rules. The app already does the right thing, but these make
# it explicit and add a frosted-glass option.
layerrule = ignorezero, ramdisplay   # pass clicks through transparent pixels
layerrule = noanim, ramdisplay       # no fade in/out on the background layer
# layerrule = blur, ramdisplay       # cosmetic: blur the wallpaper behind it
```

Reload with `hyprctl reload`. Verify the surface is registered:

```bash
hyprctl layers            # look for namespace "ramdisplay"
hyprctl monitor           # sanity-check output names
```

If you run multiple monitors and want the rain on the second one:

```ini
exec-once = ~/.cargo/bin/ramdisplay --monitor 1 --cell 18
```

<br>

## Autostart via systemd (alternative to `exec-once`)

For automatic restart on crash, install the provided user unit:

```bash
mkdir -p ~/.config/systemd/user
cp ramdisplay.service ~/.config/systemd/user/
# make sure the Wayland environment is visible to the user service manager
systemctl --user import-environment WAYLAND_DISPLAY XDG_RUNTIME_DIR
systemctl --user enable --now ramdisplay.service
systemctl --user status ramdisplay.service
```

<br>

## How it works

```
/proc/meminfo ──(std::thread, every poll-ms)──▶ Arc<SharedStats> (AtomicU64)
        │ unavailable? fall back to Simulator        │
        └────────────────────────────────────────────┘
                                                       ▼
                            GTK frame clock (add_tick_callback, v-sync)
                                                       │
                     update(): smooth metrics, advance streams, decay trails
                                                       │
                                                       ▼
                     draw(): glyph pass + additive bloom ──▶ ARGB surface
                                                       │
                         Layer::Background + empty input region (click-through)
```

Because drawing is tied to the frame clock, Hyprland stops sending frame
callbacks once the widget is fully occluded by opaque windows — the animation
*suspends* and the widget costs essentially **0 % CPU while hidden**, then
resumes without a visual jump when you return to an empty workspace.

<br>

## Performance tuning

* `--cell 24` (bigger glyphs, fewer of them) is the single best knob for CPU on
  HiDPI / 4K displays; `--cell 14` gives a denser look.
* `--fps 30` roughly halves GPU/CPU cost on battery; the rain still looks
  smooth.
* Because the canvas is full-screen, cost scales with pixel area — combine a
  larger `--cell` with a lower `--fps` on very large monitors.
* `--poll-ms 1000` halves `/proc/meminfo` reads (already negligible).
* The release profile already enables `lto`, `codegen-units = 1` and strips
  symbols.

<br>

## Troubleshooting

**The rain doesn't quite reach under my status bar.**
Layer surfaces fill the monitor minus any *exclusive zones* declared by panels
(e.g. waybar), so the rain covers the full usable viewport and stops where an
opaque panel begins. With no panels it covers 100% of the output.
Make sure your compositor is Wayland (not XWayland) and that a compositor with
per-pixel alpha is running. Check `echo $XDG_SESSION_TYPE` and
`echo $WAYLAND_DISPLAY`. Force GTK with `GDK_BACKEND=wayland ramdisplay`.

**Clicks are not passing through.**
The app installs an empty input region on realize/map and re-asserts it every
2 s. As a backup, add `layerrule = ignorezero, ramdisplay` to Hyprland.

**Nothing appears.**
`ramdisplay` requires a compositor implementing `wlr-layer-shell-unstable-v1`
(Hyprland, Sway, River, Wayfire, …). It cannot run under GNOME or X11. Run
`hyprctl layers` and confirm `ramdisplay` is listed on the `background` layer.

**Build error: `The system library gtk4-layer-shell-0 required by crate
gtk4-layer-shell-sys was not found`.**
Install the development package or build the C library from source (see above),
then rebuild. When building a from-source install into `~/.local`, point
`pkg-config` at it:

```bash
export PKG_CONFIG_PATH="$HOME/.local/lib/x86_64-linux-gnu/pkgconfig:$HOME/.local/lib/pkgconfig:$HOME/.local/share/pkgconfig"
```

<br>

## License

MIT
