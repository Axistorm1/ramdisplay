//! # ramdisplay
//!
//! A high-performance, click-through Wayland desktop widget for wlroots-style
//! compositors (Hyprland, Sway, ...) that renders an abstract, animated
//! visualization of live memory pressure on the **background** layer.
//!
//! ## Architecture
//!
//! * **Layer shell** — the window is a `zwlr_layer_surface_v1` on
//!   [`Layer::Background`]: above the wallpaper, strictly below every normal
//!   application window. `KeyboardMode::None` means it never takes focus.
//! * **Input pass-through** — an *empty* input region is installed on the
//!   `GdkSurface`, so every pointer event falls straight through to whatever is
//!   behind the widget. The widget is drawn on the background layer and is not
//!   focusable.
//! * **Zero-impact data path** — `/proc/meminfo` is polled by a dedicated OS
//!   thread. Values are published through lock-free `AtomicU64`s, so the UI
//!   thread never blocks and never allocates to read metrics.
//! * **Frame-clock rendering** — drawing is driven by GTK's `add_tick_callback`,
//!   which is tied to the compositor's v-sync. When the surface is fully
//!   occluded the compositor stops sending frame callbacks, so the widget costs
//!   ~0 % CPU while hidden and animates smoothly when visible.
//! * **Reactive visuals** — animation speed, particle density, glow and the
//!   colour gradient (cool cyan → green → amber → red) are continuously driven
//!   by exponentially smoothed RAM / swap / cache fractions.
//!
//! Run `ramdisplay --help` for options.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk::gdk;
use gtk::gdk::cairo::{Context, FontSlant, FontWeight, Operator, RadialGradient, Region};
use gtk::glib;
use gtk::prelude::*;
use gtk::{Application, ApplicationWindow, CssProvider, DrawingArea};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

const TAU: f64 = std::f64::consts::TAU;

/// Monospace glyphs used for the "digital rain". Pre-selected as `&str` so the
/// hot render loop never allocates when drawing a character.
const HEX_DIGITS: [&str; 16] = [
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "A", "B", "C", "D", "E", "F",
];
const DEC_DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];

// ---------------------------------------------------------------------------
// /proc/meminfo
// ---------------------------------------------------------------------------

/// A parsed snapshot of `/proc/meminfo` (all values in kibibytes).
#[derive(Clone, Copy, Default)]
struct MemSnapshot {
    total_kb: u64,
    available_kb: u64,
    buffers_kb: u64,
    cached_kb: u64,
    sreclaimable_kb: u64,
    swap_total_kb: u64,
    swap_free_kb: u64,
}

impl MemSnapshot {
    /// Fraction of physical RAM in use. `MemAvailable` is the kernel's own
    /// estimate of how much memory can be handed out without swapping, so it is
    /// far more accurate than `MemFree` for a "how loaded am I?" indicator.
    fn ram_used(&self) -> f64 {
        if self.total_kb == 0 {
            0.0
        } else {
            self.total_kb.saturating_sub(self.available_kb) as f64 / self.total_kb as f64
        }
    }

    /// Fraction of swap in use (0 when no swap is configured).
    fn swap_used(&self) -> f64 {
        if self.swap_total_kb == 0 {
            0.0
        } else {
            self.swap_total_kb.saturating_sub(self.swap_free_kb) as f64 / self.swap_total_kb as f64
        }
    }

    /// Fraction of RAM held as reclaimable page cache / buffers.
    fn cache_used(&self) -> f64 {
        if self.total_kb == 0 {
            0.0
        } else {
            (self.buffers_kb + self.cached_kb + self.sreclaimable_kb) as f64 / self.total_kb as f64
        }
    }
}

/// Parse `/proc/meminfo` into `buf` (reused across calls to avoid allocation).
fn read_meminfo(buf: &mut String) -> Option<MemSnapshot> {
    use std::io::Read;

    buf.clear();
    let mut file = std::fs::File::open("/proc/meminfo").ok()?;
    file.read_to_string(buf).ok()?;

    let mut snap = MemSnapshot::default();
    for line in buf.lines() {
        // Lines look like: `MemTotal:       16318744 kB`
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(value) = rest.split_whitespace().next() else {
            continue;
        };
        let Ok(value) = value.parse::<u64>() else {
            continue;
        };
        match key {
            "MemTotal" => snap.total_kb = value,
            "MemAvailable" => snap.available_kb = value,
            "Buffers" => snap.buffers_kb = value,
            "Cached" => snap.cached_kb = value,
            "SReclaimable" => snap.sreclaimable_kb = value,
            "SwapTotal" => snap.swap_total_kb = value,
            "SwapFree" => snap.swap_free_kb = value,
            _ => {}
        }
    }

    // MemAvailable is absent on very old kernels; degrade gracefully.
    if snap.available_kb == 0 {
        snap.available_kb = snap.total_kb.saturating_sub(snap.cached_kb);
    }
    Some(snap)
}

/// Lock-free publish/read slot shared between the reader thread and the UI.
#[derive(Default)]
struct SharedStats {
    total: AtomicU64,
    available: AtomicU64,
    buffers: AtomicU64,
    cached: AtomicU64,
    sreclaimable: AtomicU64,
    swap_total: AtomicU64,
    swap_free: AtomicU64,
}

impl SharedStats {
    fn store(&self, s: &MemSnapshot) {
        let set = |a: &AtomicU64, v: u64| a.store(v, Ordering::Relaxed);
        set(&self.total, s.total_kb);
        set(&self.available, s.available_kb);
        set(&self.buffers, s.buffers_kb);
        set(&self.cached, s.cached_kb);
        set(&self.sreclaimable, s.sreclaimable_kb);
        set(&self.swap_total, s.swap_total_kb);
        set(&self.swap_free, s.swap_free_kb);
    }

    fn load(&self) -> MemSnapshot {
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        MemSnapshot {
            total_kb: get(&self.total),
            available_kb: get(&self.available),
            buffers_kb: get(&self.buffers),
            cached_kb: get(&self.cached),
            sreclaimable_kb: get(&self.sreclaimable),
            swap_total_kb: get(&self.swap_total),
            swap_free_kb: get(&self.swap_free),
        }
    }
}

/// Fallback metrics source used when `/proc/meminfo` cannot be read (non-Linux
/// host, restricted sandbox, or `--simulate`). It is a smooth, mean-reverting
/// random walk that occasionally jumps to a new "pressure target", producing a
/// plausible, continuously changing RAM figure in the exact same
/// [`MemSnapshot`] shape as the real reader.
struct Simulator {
    level: f64,
    target: f64,
    seed: u64,
}

impl Simulator {
    fn new() -> Self {
        Simulator {
            level: 0.42,
            target: 0.55,
            seed: 0xD1CE_5A1D_0000_0001,
        }
    }

    fn step(&mut self, dt: f64) -> MemSnapshot {
        // Occasionally pick a new target so the rain visibly "breathes".
        if rand01(&mut self.seed) < dt * 0.15 {
            self.target = 0.25 + rand01(&mut self.seed) * 0.65;
        }
        // Mean-reverting walk around that target.
        self.level +=
            (self.target - self.level) * dt * 0.6 + (rand01(&mut self.seed) - 0.5) * dt * 0.25;
        self.level = self.level.clamp(0.08, 0.97);

        // Synthesize a plausible machine so the derived fractions match here.
        let total_kb: u64 = 16 * 1024 * 1024; // 16 GiB
        let available_kb = (total_kb as f64 * (1.0 - self.level)) as u64;
        let cached_kb = (total_kb as f64 * 0.18) as u64;
        let swap_total_kb: u64 = 8 * 1024 * 1024;
        let swap_free_kb = (swap_total_kb as f64 * (1.0 - self.level * 0.5)) as u64;
        MemSnapshot {
            total_kb,
            available_kb,
            buffers_kb: 0,
            cached_kb,
            sreclaimable_kb: 0,
            swap_total_kb,
            swap_free_kb,
        }
    }
}

/// Spawn the metrics reader on its own OS thread. Real `/proc/meminfo` data is
/// preferred; if it is unavailable (or `force_simulate` is set) the thread
/// transparently falls back to [`Simulator`].
fn spawn_reader(stats: Arc<SharedStats>, interval: Duration, force_simulate: bool) {
    std::thread::Builder::new()
        .name("meminfo-reader".into())
        .spawn(move || {
            let mut buf = String::with_capacity(4096);
            let mut sim = Simulator::new();
            let mut warned = false;
            loop {
                let real = if force_simulate {
                    None
                } else {
                    read_meminfo(&mut buf)
                };
                match real {
                    Some(snapshot) => stats.store(&snapshot),
                    None => {
                        if !warned {
                            if !force_simulate {
                                eprintln!(
                                    "ramdisplay: /proc/meminfo unavailable — \
                                     falling back to simulated RAM metrics"
                                );
                            }
                            warned = true;
                        }
                        let snapshot = sim.step(interval.as_secs_f64());
                        stats.store(&snapshot);
                    }
                }
                std::thread::sleep(interval);
            }
        })
        .expect("failed to spawn the /proc/meminfo reader thread");
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

struct Opts {
    /// Full-screen (100vw × 100vh) canvas by default; `--windowed` opts out.
    fullscreen: bool,
    /// Edge length for windowed mode.
    size: i32,
    /// Glyph cell size in logical pixels.
    cell: f64,
    /// Which data-like alphabet the rain renders.
    glyphs: GlyphSet,
    fps: f64,
    poll_ms: u64,
    namespace: String,
    monitor: Option<i32>,
    /// Force simulated metrics (ignore `/proc/meminfo`).
    simulate: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            fullscreen: true,
            size: 380,
            cell: 18.0,
            glyphs: GlyphSet::Hex,
            fps: 60.0,
            poll_ms: 500,
            namespace: "ramdisplay".to_string(),
            monitor: None,
            simulate: false,
        }
    }
}

fn print_help() {
    println!(
        "ramdisplay — memory-pressure \"digital rain\" for Wayland layer shells\n\
         \n\
         USAGE:\n    ramdisplay [OPTIONS]\n\
         \n\
         OPTIONS:\n\
         \x20   -f, --fullscreen        Fill the whole monitor (100vw x 100vh) [default]\n\
         \x20       --windowed          Run as a centered window instead (see --size)\n\
         \x20       --size <px>         Window edge length in windowed mode [default: 380]\n\
         \x20       --cell <px>         Rain glyph cell size [default: 18]\n\
         \x20       --glyphs <set>      hex | binary | decimal [default: hex]\n\
         \x20       --simulate          Force simulated RAM metrics (ignore /proc/meminfo)\n\
         \x20       --fps <n>           Frame-rate cap [default: 60]\n\
         \x20       --poll-ms <n>       /proc/meminfo polling interval [default: 500]\n\
         \x20       --namespace <s>     Layer-shell namespace [default: ramdisplay]\n\
         \x20       --monitor <n>       Place on monitor index n [default: compositor choice]\n\
         \x20   -h, --help              Print this help"
    );
}

fn parse_args() -> (Opts, bool) {
    let mut opts = Opts::default();
    let mut help = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-f" | "--fullscreen" => opts.fullscreen = true,
            "--windowed" | "--no-fullscreen" => opts.fullscreen = false,
            "--size" => {
                if let Some(v) = args.next().and_then(|v| v.parse().ok()) {
                    opts.size = v;
                }
            }
            "--cell" => {
                if let Some(v) = args.next().and_then(|v| v.parse().ok()) {
                    opts.cell = v;
                }
            }
            "--glyphs" => {
                if let Some(v) = args.next() {
                    match GlyphSet::parse(&v) {
                        Some(set) => opts.glyphs = set,
                        None => eprintln!(
                            "ramdisplay: unknown glyph set `{v}` (try hex, binary or decimal)"
                        ),
                    }
                }
            }
            "--simulate" => opts.simulate = true,
            "--fps" => {
                if let Some(v) = args.next().and_then(|v| v.parse().ok()) {
                    opts.fps = v;
                }
            }
            "--poll-ms" => {
                if let Some(v) = args.next().and_then(|v| v.parse().ok()) {
                    opts.poll_ms = v;
                }
            }
            "--namespace" => {
                if let Some(v) = args.next() {
                    opts.namespace = v;
                }
            }
            "--monitor" => {
                if let Some(v) = args.next().and_then(|v| v.parse().ok()) {
                    opts.monitor = Some(v);
                }
            }
            "-h" | "--help" => help = true,
            other => eprintln!("ramdisplay: ignoring unknown argument `{other}`"),
        }
    }
    (opts, help)
}

// ---------------------------------------------------------------------------
// Colour helpers
// ---------------------------------------------------------------------------

/// Tiny xorshift64* PRNG → uniform `f64` in `[0, 1)`. Keeps the animation
/// deterministic and free of dependencies.
fn rand01(state: &mut u64) -> f64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    ((x >> 11) as f64) / ((1u64 << 53) as f64)
}

fn hsv_to_rgb(h: f64, s: f64, v: f64) -> (f64, f64, f64) {
    let h = (h / 60.0).rem_euclid(6.0);
    let c = v * s;
    let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (r + m, g + m, b + m)
}

/// Trail colour. Classic Matrix green that slides through yellow toward red as
/// RAM (and swap) pressure grows — the "warning" end of the gradient.
fn rain_color(load: f64, swap: f64) -> (f64, f64, f64) {
    let heat = (load + swap * 0.35).clamp(0.0, 1.0);
    let hue = 120.0 * (1.0 - heat); // 120° green -> 0° red
    hsv_to_rgb(hue, 0.85, 0.55 + 0.35 * heat)
}

/// Head glyph colour: near-white, tinted by the trail colour so the leading
/// character of every stream pops.
fn head_color(load: f64) -> (f64, f64, f64) {
    let (r, g, b) = rain_color(load, 0.0);
    (
        (r * 0.30 + 0.70).min(1.0),
        (g * 0.30 + 0.70).min(1.0),
        (b * 0.30 + 0.70).min(1.0),
    )
}

// ---------------------------------------------------------------------------
// Animation — Matrix "digital rain"
// ---------------------------------------------------------------------------
//
// The canvas is a grid of monospace glyph cells. Each column is an independent
// vertical *stream* of falling data, exactly like the classic effect, except
// the glyphs are data values (hex / binary / decimal) and every property of the
// rain is driven by live memory pressure:
//
//   * RAM + swap -> fall speed, trail length, density and colour (green -> red)
//   * RAM        -> additive bloom around the leading glyph of each stream
//
// A per-cell `intensity` field decays every frame while the head resets it to
// 1.0, so a trail is just decaying alpha and its length falls out naturally
// from (speed x decay) with no per-glyph bookkeeping.

/// Which alphabet the streams display.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GlyphSet {
    Hex,
    Binary,
    Decimal,
}

impl GlyphSet {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "hex" | "hexadecimal" => Some(GlyphSet::Hex),
            "bin" | "binary" => Some(GlyphSet::Binary),
            "dec" | "decimal" => Some(GlyphSet::Decimal),
            _ => None,
        }
    }

    fn len(self) -> usize {
        match self {
            GlyphSet::Hex => HEX_DIGITS.len(),
            GlyphSet::Decimal => DEC_DIGITS.len(),
            GlyphSet::Binary => 2,
        }
    }
}

fn glyph_str(set: GlyphSet, code: u8) -> &'static str {
    match set {
        GlyphSet::Hex => HEX_DIGITS[(code as usize) % HEX_DIGITS.len()],
        GlyphSet::Decimal => DEC_DIGITS[(code as usize) % DEC_DIGITS.len()],
        GlyphSet::Binary => {
            if code & 1 == 0 {
                "0"
            } else {
                "1"
            }
        }
    }
}

/// One vertical column of falling data.
struct Stream {
    /// Head row position (fractional, so motion stays smooth at any speed).
    head: f64,
    /// Base fall speed in rows per second (scaled by load each step).
    speed: f64,
    /// Rows of travel past the bottom before the stream recycles.
    margin: f64,
}

struct Visual {
    /// Elapsed animation time, seconds.
    t: f64,
    /// Time of the last rendered frame (frame-rate cap).
    last_draw: Instant,
    /// Exponentially smoothed load fractions (0..1).
    ram: f64,
    swap: f64,
    cache: f64,

    // --- viewport-derived grid state ---
    /// Glyph cell size in logical pixels.
    cell: f64,
    cols: usize,
    rows: usize,
    /// Glyph code per cell (index into the active [`GlyphSet`]).
    glyphs: Vec<u8>,
    /// Brightness per cell; decays every step to form the falling trails.
    intens: Vec<f32>,
    /// One stream per grid column.
    streams: Vec<Stream>,
    glyph_set: GlyphSet,

    rng: u64,
    min_frame: f64,
}

impl Visual {
    fn new(opts: &Opts) -> Self {
        Visual {
            t: 0.0,
            last_draw: Instant::now(),
            ram: 0.0,
            swap: 0.0,
            cache: 0.0,
            cell: opts.cell.max(6.0),
            cols: 0,
            rows: 0,
            glyphs: Vec::new(),
            intens: Vec::new(),
            streams: Vec::new(),
            glyph_set: opts.glyphs,
            rng: 0x9E37_79B9_7F4A_7C15_u64,
            min_frame: 1.0 / opts.fps.max(1.0),
        }
    }

    /// (Re)build the grid for a new viewport size. Only runs when the surface
    /// size actually changes, so steady-state frames never allocate.
    fn ensure_grid(&mut self, width: i32, height: i32) {
        let cols = ((width as f64) / self.cell).ceil().max(1.0) as usize;
        let rows = ((height as f64) / self.cell).ceil().max(1.0) as usize;
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.glyphs = vec![0u8; cols * rows];
        self.intens = vec![0.0f32; cols * rows];
        self.streams = Vec::with_capacity(cols);
        for _ in 0..cols {
            let stream = self.new_stream(true);
            self.streams.push(stream);
        }
    }

    fn new_stream(&mut self, scatter: bool) -> Stream {
        // Scatter the initial heads so columns start desynchronized.
        let stagger = if scatter {
            rand01(&mut self.rng) * self.rows.max(1) as f64
        } else {
            rand01(&mut self.rng) * 8.0
        };
        Stream {
            head: -stagger,
            speed: 0.6 + rand01(&mut self.rng) * 1.8,
            margin: 6.0 + rand01(&mut self.rng) * 30.0,
        }
    }

    fn random_glyph(&mut self) -> u8 {
        (rand01(&mut self.rng) * self.glyph_set.len() as f64) as u8
    }

    /// Advance the rain one frame. This is where memory pressure is mapped onto
    /// the visual properties of the streams.
    fn step(&mut self, dt: f64) {
        if self.cols == 0 || self.rows == 0 || dt <= 0.0 {
            return;
        }
        let load = self.ram.clamp(0.0, 1.0) + self.swap.clamp(0.0, 1.0) * 0.35;

        // --- memory -> motion / density ---
        // Higher pressure: streams fall faster and their trails persist longer
        // (a longer decay time constant), so the screen fills with more glyphs.
        let speed_mul = 0.6 + load * 4.5;
        let tau = 0.28 + load * 1.4;
        let decay = (-dt / tau).exp() as f32;

        for v in self.intens.iter_mut() {
            *v *= decay;
        }

        let cols = self.cols;
        let rows = self.rows as i64;

        for c in 0..cols {
            // Copy the stream scalars out so the grid/RNG can be mutated freely.
            let mut head = self.streams[c].head;
            let mut speed = self.streams[c].speed;
            let margin = self.streams[c].margin;

            let prev = head.floor() as i64;
            head += speed * speed_mul * dt;
            let now = head.floor() as i64;

            // Light every row the head swept across this frame.
            let mut row = prev + 1;
            while row <= now {
                if row >= 0 && row < rows {
                    let idx = (row as usize) * cols + c;
                    self.glyphs[idx] = self.random_glyph();
                    self.intens[idx] = 1.0;
                }
                row += 1;
            }
            // Keep the leading cell white-hot while it is on screen.
            if now >= 0 && now < rows {
                self.intens[(now as usize) * cols + c] = 1.0;
            }

            // Recycle once fully drained. Lower pressure => longer dormancy =>
            // visibly sparser rain.
            if head > rows as f64 + margin {
                let dormancy = (1.0 - load.clamp(0.0, 1.0)) * self.rows.max(1) as f64 * 0.7;
                head = -margin - rand01(&mut self.rng) * dormancy - 2.0;
                speed = 0.6 + rand01(&mut self.rng) * 1.8;
            }

            self.streams[c].head = head;
            self.streams[c].speed = speed;
        }
    }

    /// Ease the smoothed metrics toward the live values and advance the rain.
    /// Rendering happens separately in [`Visual::draw`].
    fn update(&mut self, dt: f64, stats: &SharedStats) {
        let snap = stats.load();

        // Exponential smoothing hides the 500 ms sampling steps and gives the
        // colour/speed transitions a fluid, organic feel.
        let k = 1.0 - (-dt / 0.35).exp();
        self.ram += (snap.ram_used() - self.ram) * k;
        self.swap += (snap.swap_used() - self.swap) * k;
        self.cache += (snap.cache_used().min(1.0) - self.cache) * k;

        self.t += dt;
        self.step(dt);
    }

    /// Render one full-viewport frame onto the (already transparent) surface.
    fn draw(&mut self, ctx: &Context, width: i32, height: i32) {
        // Guarantee a pristine, fully transparent frame.
        ctx.set_operator(Operator::Clear);
        let _ = ctx.paint();
        ctx.set_operator(Operator::Over);

        self.ensure_grid(width, height);
        if self.cols == 0 || self.rows == 0 {
            return;
        }

        let load = self.ram.clamp(0.0, 1.0);
        let swap = self.swap.clamp(0.0, 1.0);
        let (br, bg, bb) = rain_color(load, swap);
        let (hr, hg, hb) = head_color(load);

        let cell = self.cell;

        // Monospace font; centre each glyph horizontally within its cell.
        ctx.select_font_face("monospace", FontSlant::Normal, FontWeight::Normal);
        ctx.set_font_size(cell * 0.86);
        let advance = ctx.text_extents("0").map(|e| e.x_advance()).unwrap_or(cell);
        let x_pad = ((cell - advance) * 0.5).max(0.0);

        // ---- glyph pass: the trails themselves ----
        for row in 0..self.rows {
            let y = (row as f64 + 1.0) * cell - cell * 0.14;
            for col in 0..self.cols {
                let idx = row * self.cols + col;
                let a = self.intens[idx];
                if a < 0.04 {
                    continue;
                }
                // Leading glyphs render near-white; the tail fades to colour.
                let (r, g, b) = if a > 0.85 { (hr, hg, hb) } else { (br, bg, bb) };
                let alpha = (a as f64).powf(0.65) * 0.95;
                ctx.set_source_rgba(r, g, b, alpha);
                ctx.move_to(col as f64 * cell + x_pad, y);
                let _ = ctx.show_text(glyph_str(self.glyph_set, self.glyphs[idx]));
            }
        }

        // ---- bloom pass: additive glow on the heads, scaled by pressure ----
        let bloom = 0.10 + load * 0.55 + self.cache * 0.20;
        let radius = cell * (0.9 + load * 0.7);
        let sprite = RadialGradient::new(0.0, 0.0, 0.0, 0.0, 0.0, radius);
        sprite.add_color_stop_rgba(0.0, br, bg, bb, 0.55 * bloom);
        sprite.add_color_stop_rgba(0.4, br, bg, bb, 0.18 * bloom);
        sprite.add_color_stop_rgba(1.0, br, bg, bb, 0.0);
        ctx.set_operator(Operator::Add);
        for row in 0..self.rows {
            for col in 0..self.cols {
                if self.intens[row * self.cols + col] < 0.9 {
                    continue;
                }
                let x = (col as f64 + 0.5) * cell;
                let y = (row as f64 + 0.5) * cell;
                let _ = ctx.save();
                ctx.translate(x, y);
                let _ = ctx.set_source(&sprite);
                ctx.arc(0.0, 0.0, radius, 0.0, TAU);
                let _ = ctx.fill();
                let _ = ctx.restore();
            }
        }
        ctx.set_operator(Operator::Over);

        // A very faint full-screen wash that deepens with load (global "glow").
        let wash = 0.015 + load * 0.07;
        ctx.set_source_rgba(br, bg, bb, wash);
        let _ = ctx.paint();
    }
}

// ---------------------------------------------------------------------------
// UI plumbing
// ---------------------------------------------------------------------------

/// Install an **empty** input region on the window's surface. An empty region
/// means "no rectangle accepts pointer input", which is exactly click-through.
fn apply_click_through(window: &ApplicationWindow) {
    if let Some(surface) = window.surface() {
        let empty = Region::create();
        surface.set_input_region(Some(&empty));
    }
}

fn build_ui(app: &Application, opts: &Opts, stats: Arc<SharedStats>) {
    if !gtk4_layer_shell::is_supported() {
        eprintln!(
            "ramdisplay: this compositor does not advertise wlr-layer-shell; \
             are you sure you are running under Wayland (Hyprland/Sway)?"
        );
        std::process::exit(1);
    }

    let window = ApplicationWindow::builder()
        .application(app)
        .title("ramdisplay")
        .build();

    // No decorations and never a normal toplevel. A full-screen anchored layer
    // surface must accept the compositor's configure size, so it has to stay
    // resizable (GTK ignores compositor resizes when `resizable` is false).
    window.set_decorated(false);
    window.set_resizable(opts.fullscreen);

    // --- layer-shell configuration (must happen before realize/present) ---
    window.init_layer_shell();
    window.set_namespace(Some(opts.namespace.as_str()));
    window.set_layer(Layer::Background); // above wallpaper, behind all windows
    window.set_keyboard_mode(KeyboardMode::None); // never steals focus
    window.set_exclusive_zone(0); // reserve no space for other windows

    if opts.fullscreen {
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(edge, true);
        }
    } else {
        window.set_default_size(opts.size, opts.size);
        // No anchors => the compositor centers the surface at its natural size.
    }

    // Optional explicit monitor selection, e.g. `--monitor 1`.
    if let (Some(index), Some(display)) = (opts.monitor, gdk::Display::default()) {
        let monitors = display.monitors();
        if let Some(monitor) = monitors
            .item(index.max(0) as u32)
            .and_then(|obj| obj.downcast::<gdk::Monitor>().ok())
        {
            window.set_monitor(Some(&monitor));
        }
    }

    // --- transparent window background (per-pixel RGBA on Wayland) ---
    let css = CssProvider::new();
    // `load_from_data` is the version-portable entry point (deprecated only on
    // GTK ≥ 4.12 in favour of `load_from_string`).
    css.load_from_data(
        "window, window.background, window > contents, .background { \
             background: none; background-color: rgba(0,0,0,0); \
             box-shadow: none; border: none; }",
    );
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    // --- drawing surface ---
    let area = DrawingArea::new();
    area.set_hexpand(true);
    area.set_vexpand(true);
    area.set_can_focus(false);
    window.set_child(Some(&area));

    let visual = Rc::new(RefCell::new(Visual::new(opts)));

    // Paint callback: reads the current simulation state, draws it.
    {
        let visual = visual.clone();
        area.set_draw_func(move |_area, ctx, width, height| {
            visual.borrow_mut().draw(ctx, width, height);
        });
    }

    // Frame clock: tied to the compositor's v-sync, and paused automatically
    // while the surface is occluded. We only advance/render when the target
    // frame interval has elapsed, capping CPU use on high-refresh displays.
    {
        let visual = visual.clone();
        area.add_tick_callback(move |area: &DrawingArea, _clock: &gdk::FrameClock| {
            let now = Instant::now();
            let elapsed = now.duration_since(visual.borrow().last_draw).as_secs_f64();
            if elapsed < visual.borrow().min_frame {
                return glib::ControlFlow::Continue;
            }
            // Clamp huge gaps (e.g. after the surface was hidden for a while).
            let dt = elapsed.min(0.1);
            {
                let mut v = visual.borrow_mut();
                v.last_draw = now;
                v.update(dt, &stats);
            }
            area.queue_draw();
            glib::ControlFlow::Continue
        });
    }

    // Click-through: apply on realize and on every map, then defensively
    // re-assert it at a low frequency in case GTK recomputes the region.
    window.connect_realize(apply_click_through);
    window.connect_map(apply_click_through);
    {
        let weak = window.downgrade();
        glib::timeout_add_seconds_local(2, move || {
            if let Some(window) = weak.upgrade() {
                apply_click_through(&window);
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });
    }

    window.present();
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let (opts, help) = parse_args();
    if help {
        print_help();
        return;
    }

    let stats = Arc::new(SharedStats::default());
    spawn_reader(
        stats.clone(),
        Duration::from_millis(opts.poll_ms.max(50)),
        opts.simulate,
    );

    let app = Application::builder()
        .application_id("dev.ramdisplay.widget")
        .build();

    app.connect_activate(move |app| build_ui(app, &opts, stats.clone()));

    // `run()` never returns while the widget lives.
    // Pass a sanitized argv so GApplication's GOption parser never sees our
    // custom flags (otherwise it aborts with "Unknown option --size").
    let _ = app.run_with_args(&["ramdisplay"]);
}

// ---------------------------------------------------------------------------
// Offscreen rendering test / preview generator
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::cairo::{Context as CairoContext, Format, ImageSurface};

    /// Render the rain offscreen (no compositor) after letting it run for a few
    /// simulated seconds, so the trails are fully developed.
    fn render(ram: f64, swap: f64, cache: f64, glyphs: GlyphSet) -> ImageSurface {
        const W: i32 = 640;
        const H: i32 = 400;

        let surface = ImageSurface::create(Format::ARgb32, W, H).unwrap();
        let ctx = CairoContext::new(&surface).unwrap();

        let mut opts = Opts::default();
        opts.cell = 16.0;
        opts.glyphs = glyphs;
        let mut visual = Visual::new(&opts);

        // Feed a synthetic snapshot through the exact same SharedStats slot the
        // real reader thread uses.
        let stats = SharedStats::default();
        stats.store(&MemSnapshot {
            total_kb: 100,
            available_kb: ((100.0 * (1.0 - ram)) as u64).max(1),
            buffers_kb: 0,
            cached_kb: (100.0 * cache) as u64,
            sreclaimable_kb: 0,
            swap_total_kb: 100,
            swap_free_kb: ((100.0 * (1.0 - swap)) as u64).max(1),
        });

        visual.draw(&ctx, W, H); // builds the grid
        for _ in 0..180 {
            visual.update(1.0 / 60.0, &stats); // 3 seconds of rain
        }
        visual.draw(&ctx, W, H);
        surface
    }

    #[test]
    fn preview_frames_render() {
        let dir = std::env::temp_dir();
        let cases = [
            ("low", 0.12, 0.0, 0.30, GlyphSet::Hex),
            ("mid", 0.55, 0.20, 0.45, GlyphSet::Hex),
            ("high", 0.93, 0.70, 0.60, GlyphSet::Hex),
            ("binary-high", 0.93, 0.70, 0.60, GlyphSet::Binary),
            ("decimal-mid", 0.55, 0.20, 0.45, GlyphSet::Decimal),
        ];
        for (name, ram, swap, cache, glyphs) in cases {
            let surface = render(ram, swap, cache, glyphs);
            let path = dir.join(format!("ramdisplay-preview-{name}.png"));
            let mut file = std::fs::File::create(&path).unwrap();
            surface.write_to_png(&mut file).unwrap();
            eprintln!("wrote {}", path.display());
        }
    }
}
