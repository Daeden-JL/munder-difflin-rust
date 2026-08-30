//! The office floor: a room agents walk around in.
//!
//! Not a roster of tiles. Each agent has a position and a destination, walks
//! between them, and **wanders on its own when idle** — that idling is pure
//! client-side animation and costs nothing, which is the point: a floor that
//! only moved when the model did would be still almost all the time.
//!
//! Movement means something. A tool call arrives as a hook event carrying the
//! tool's name, and that name maps to a station: `Read` sends an agent to the
//! shelves, `Bash` to the terminal, `WebFetch` to the web desk, `TodoWrite` to
//! the board. So the floor is a live picture of what the fleet is doing rather
//! than a decoration.
//!
//! **Canvas 2D, not wgpu** — a deliberate divergence from the conversion plan.
//! The art is 18×32 sprites on a flat grid; a GPU pipeline there costs a shader
//! stack, a surface lifecycle and a device-lost path to blit a few dozen
//! sprites. If the floor ever grows real lighting or thousands of entities, the
//! renderer is one module to replace.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use leptos::prelude::*;
use wasm_bindgen::{Clamped, JsCast};
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageData};

use crate::pixel;
use crate::theme::{self, Theme};

/// Pixels per art pixel. Integer, with smoothing off: a fractional scale or
/// bilinear filtering turns pixel art into mush.
const SCALE: f64 = 2.0;
/// Room size in art pixels.
const ROOM_W: f64 = 320.0;
const ROOM_H: f64 = 176.0;
/// Walking speed, art pixels per second. Slow enough to read as walking.
const SPEED: f64 = 22.0;
/// How long a step frame lasts. Faster than the stride looks wrong.
const STEP_SECS: f64 = 0.18;
/// How long an idle agent lingers before wandering somewhere else.
const LINGER_SECS: f64 = 3.5;

/// Where a tool sends an agent.
///
/// Ported from `usePtyParser.ts`, which derived the same mapping by scraping
/// the terminal. Here the tool name arrives structurally on the hook event, so
/// the mapping is the only part that was ever worth keeping.
pub fn station_for(tool: &str) -> &'static str {
    match tool {
        "Read" | "Edit" | "Write" | "MultiEdit" | "Grep" | "Glob" | "NotebookEdit" => "shelf",
        "Bash" | "BashOutput" | "KillShell" => "terminal",
        "WebFetch" | "WebSearch" => "web",
        "TodoWrite" | "TaskCreate" | "TaskUpdate" => "board",
        _ => "desk",
    }
}

/// A place on the floor, in art pixels. Agents stand just below a station and
/// face it.
struct Station {
    kind: &'static str,
    label: &'static str,
    x: f64,
    y: f64,
    w: f64,
}

/// The room. Hand-placed rather than loaded from the Tiled map: the `.tmj`
/// carries a tileset image this client does not ship, and the layout is what
/// matters — a bookshelf on the left, terminals on the right, the board on the
/// back wall, desks in the middle.
fn stations() -> Vec<Station> {
    vec![
        Station { kind: "board", label: "BOARD", x: 120.0, y: 14.0, w: 76.0 },
        Station { kind: "shelf", label: "SHELF", x: 10.0, y: 30.0, w: 26.0 },
        Station { kind: "web", label: "WEB", x: 274.0, y: 30.0, w: 36.0 },
        Station { kind: "terminal", label: "TERM", x: 274.0, y: 96.0, w: 36.0 },
        Station { kind: "mailbox", label: "MAIL", x: 10.0, y: 120.0, w: 26.0 },
    ]
}

/// One agent's motion state. Kept OUTSIDE the reactive graph: it changes every
/// frame, and routing sixty updates a second through signals would re-render
/// the surrounding view for something only the canvas cares about.
struct Walker {
    x: f64,
    y: f64,
    tx: f64,
    ty: f64,
    /// Which station it is heading to, if any. `None` means wandering.
    at: Option<&'static str>,
    facing_back: bool,
    step: u8,
    step_t: f64,
    linger: f64,
    /// A per-agent deterministic seed, so wandering differs between agents but
    /// is not driven by a shared global.
    seed: u32,
}

impl Walker {
    fn new(index: usize, seed: u32) -> Self {
        // Start spread along the desk row rather than stacked at the origin.
        let x = 60.0 + (index as f64 * 34.0) % 180.0;
        let y = 96.0 + ((index % 2) as f64) * 22.0;
        Self { x, y, tx: x, ty: y, at: None, facing_back: false, step: 0, step_t: 0.0, linger: 0.0, seed }
    }

    /// A cheap deterministic PRNG. `Math.random` would do, but a per-walker
    /// sequence keeps two agents from wandering in lockstep after a reload.
    fn rand(&mut self) -> f64 {
        self.seed = self.seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.seed >> 8) as f64 / 16_777_216.0
    }

    fn arrived(&self) -> bool {
        (self.tx - self.x).abs() < 1.0 && (self.ty - self.y).abs() < 1.0
    }

    fn advance(&mut self, dt: f64) {
        if !self.arrived() {
            let (dx, dy) = (self.tx - self.x, self.ty - self.y);
            let dist = (dx * dx + dy * dy).sqrt().max(0.001);
            let travel = (SPEED * dt).min(dist);
            self.x += dx / dist * travel;
            self.y += dy / dist * travel;
            // Walking away from the viewer shows the back of the head.
            self.facing_back = dy < -0.5;
            self.step_t += dt;
            if self.step_t >= STEP_SECS {
                self.step_t = 0.0;
                // Frames cycle 1,2,1,2 — phase 0 is standing, and stepping
                // through it makes the gait stutter.
                self.step = if self.step == 1 { 2 } else { 1 };
            }
            return;
        }

        // Arrived. Stand still, then wander — unless posted to a station, where
        // the agent stays until its work sends it somewhere else.
        self.step = 0;
        self.step_t = 0.0;
        if self.at.is_some() {
            return;
        }
        self.linger -= dt;
        if self.linger <= 0.0 {
            self.linger = LINGER_SECS + self.rand() * LINGER_SECS;
            self.tx = 50.0 + self.rand() * (ROOM_W - 110.0);
            self.ty = 84.0 + self.rand() * 56.0;
        }
    }

    fn send_to(&mut self, s: &Station) {
        self.at = Some(s.kind);
        // Stand below the station, so the figure does not cover the label.
        self.tx = s.x + s.w / 2.0 - pixel::SCENE_W as f64 / 2.0;
        self.ty = s.y + 20.0;
    }
}

/// One agent as the floor needs to draw it.
#[derive(Clone, PartialEq)]
pub struct Occupant {
    pub id: String,
    pub name: String,
    pub archetype: String,
    pub status: String,
    pub live: bool,
}

/// Baked sprite frames for one character: three gait phases, front and back.
struct Frames {
    front: Vec<HtmlCanvasElement>,
    back: Vec<HtmlCanvasElement>,
}

#[derive(Default)]
struct Sprites {
    cache: HashMap<String, Rc<Frames>>,
}

impl Sprites {
    /// Bake every frame for a character once.
    ///
    /// Keyed by theme AND archetype: switching themes must not hand back the
    /// previous cast's art for the same slot.
    fn get(&mut self, doc: &web_sys::Document, theme: &Theme, archetype: &str) -> Option<Rc<Frames>> {
        let key = format!("{}:{archetype}", theme.id);
        if !self.cache.contains_key(&key) {
            let c = theme.character(archetype)?;
            let bake = |back: bool| -> Vec<HtmlCanvasElement> {
                (0..3)
                    .filter_map(|phase| {
                        let cv = pixel::scene(&c.recipe, phase, back);
                        let data = ImageData::new_with_u8_clamped_array_and_sh(
                            Clamped(&cv.buf),
                            pixel::SCENE_W as u32,
                            pixel::SCENE_H as u32,
                        )
                        .ok()?;
                        let off: HtmlCanvasElement = doc.create_element("canvas").ok()?.unchecked_into();
                        off.set_width(pixel::SCENE_W as u32);
                        off.set_height(pixel::SCENE_H as u32);
                        let octx: CanvasRenderingContext2d = off.get_context("2d").ok()??.unchecked_into();
                        octx.put_image_data(&data, 0.0, 0.0).ok()?;
                        Some(off)
                    })
                    .collect()
            };
            let frames = Frames { front: bake(false), back: bake(true) };
            if frames.front.len() != 3 {
                return None;
            }
            self.cache.insert(key.clone(), Rc::new(frames));
        }
        self.cache.get(&key).cloned()
    }
}

fn status_colour(status: &str, live: bool) -> &'static str {
    if !live {
        return "#5a6473";
    }
    match status {
        "working" => "#4ec994",
        "blocked" => "#e5675a",
        "waiting" => "#d8a657",
        _ => "#8b96a8",
    }
}

/// The animation-loop closure, held so it can re-arm itself each frame.
type RafLoop = Rc<RefCell<Option<wasm_bindgen::closure::Closure<dyn FnMut(f64)>>>>;

/// The display name of the character an archetype is dressed as, in this theme.
pub fn character_name(theme: &Theme, archetype: &str) -> Option<String> {
    theme.character(archetype).map(|c| c.display.clone())
}

/// One character's portrait as a data URL, for use in ordinary DOM.
///
/// The floor draws walking figures; the roster wants a face. Both come from the
/// same recipe, which is the payoff of the art being procedural: a second view
/// of a character costs a function call, not a second sprite sheet.
pub fn portrait_data_url(theme: &Theme, archetype: &str) -> Option<String> {
    let doc = window().document()?;
    let c = theme.character(archetype)?;
    let cv = pixel::portrait(&c.recipe);
    let data = ImageData::new_with_u8_clamped_array_and_sh(
        Clamped(&cv.buf),
        pixel::W as u32,
        pixel::H as u32,
    )
    .ok()?;
    let off: HtmlCanvasElement = doc.create_element("canvas").ok()?.unchecked_into();
    off.set_width(pixel::W as u32);
    off.set_height(pixel::H as u32);
    let ctx: CanvasRenderingContext2d = off.get_context("2d").ok()??.unchecked_into();
    ctx.put_image_data(&data, 0.0, 0.0).ok()?;
    off.to_data_url().ok()
}

#[component]
pub fn Floor(
    occupants: Signal<Vec<Occupant>>,
    theme: RwSignal<usize>,
    selected: RwSignal<Option<String>>,
    /// The most recent `(agentId, tool)` from the hook stream. Changing it is
    /// what sends an agent to a station.
    activity: RwSignal<Option<(String, String)>>,
) -> impl IntoView {
    let canvas_ref = NodeRef::<leptos::html::Canvas>::new();
    let sprites = Rc::new(RefCell::new(Sprites::default()));
    let walkers: Rc<RefCell<HashMap<String, Walker>>> = Rc::new(RefCell::new(HashMap::new()));
    let themes = Rc::new(theme::builtin());
    let names: Vec<String> = themes.iter().map(|t| t.name.clone()).collect();

    // A tool call moves the agent that made it. Applied to the walker map
    // directly rather than through a signal, because the animation loop owns
    // position and two writers would fight.
    {
        let walkers = walkers.clone();
        Effect::new(move |_| {
            let Some((id, tool)) = activity.get() else { return };
            let kind = station_for(&tool);
            let all = stations();
            let Some(s) = all.iter().find(|s| s.kind == kind) else { return };
            if let Some(w) = walkers.borrow_mut().get_mut(&id) {
                w.send_to(s);
            }
        });
    }

    // The animation loop. One `requestAnimationFrame` chain for the whole
    // floor: a timer per agent would drift apart and repaint the canvas
    // several times a frame.
    {
        let (sprites, walkers, themes) = (sprites.clone(), walkers.clone(), themes.clone());
        Effect::new(move |_| {
            let Some(canvas) = canvas_ref.get() else { return };
            let el: HtmlCanvasElement = canvas.unchecked_into();
            let Some(doc) = window().document() else { return };

            let raf: RafLoop = Rc::new(RefCell::new(None));
            let outer = raf.clone();
            let last = Rc::new(RefCell::new(0.0f64));

            let (sprites, walkers, themes) = (sprites.clone(), walkers.clone(), themes.clone());
            *outer.borrow_mut() = Some(wasm_bindgen::closure::Closure::new(move |now: f64| {
                let dt = {
                    let mut l = last.borrow_mut();
                    // Clamp the first frame and any tab-restore gap: a
                    // multi-second dt would teleport everyone across the room.
                    let d = if *l == 0.0 { 0.016 } else { ((now - *l) / 1000.0).clamp(0.0, 0.1) };
                    *l = now;
                    d
                };

                let list = occupants.get_untracked();
                let t = &themes[theme.get_untracked() % themes.len().max(1)];

                // Reconcile walkers with the roster: new agents get a walker,
                // departed ones lose theirs, so the map cannot grow forever.
                {
                    let mut w = walkers.borrow_mut();
                    w.retain(|id, _| list.iter().any(|o| &o.id == id));
                    for (i, o) in list.iter().enumerate() {
                        if !w.contains_key(&o.id) {
                            let seed = o.id.bytes().fold(7u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
                            w.insert(o.id.clone(), Walker::new(i, seed));
                        }
                    }
                    for walker in w.values_mut() {
                        walker.advance(dt);
                    }
                }

                if let Ok(Some(ctx)) = el.get_context("2d") {
                    let ctx: CanvasRenderingContext2d = ctx.unchecked_into();
                    draw(&ctx, &el, &doc, t, &list, &walkers.borrow(), &mut sprites.borrow_mut(), selected.get_untracked());
                }

                if let Some(cb) = raf.borrow().as_ref() {
                    let _ = window().request_animation_frame(cb.as_ref().unchecked_ref());
                }
            }));

            if let Some(cb) = outer.borrow().as_ref() {
                let _ = window().request_animation_frame(cb.as_ref().unchecked_ref());
            }
            // The closure is deliberately leaked with the effect: the loop lives
            // as long as the floor does, and dropping it mid-frame would leave
            // the browser calling into freed memory.
            std::mem::forget(outer);
        });
    }

    // Clicking a figure selects that agent. Hit-tested against the walkers'
    // live positions, so the target is where the agent visibly IS.
    let on_click = {
        let walkers = walkers.clone();
        move |ev: leptos::ev::MouseEvent| {
            let Some(canvas) = canvas_ref.get() else { return };
            let el: HtmlCanvasElement = canvas.unchecked_into();
            let rect = el.get_bounding_client_rect();
            let sx = el.width() as f64 / rect.width().max(1.0);
            let sy = el.height() as f64 / rect.height().max(1.0);
            let px = (ev.client_x() as f64 - rect.left()) * sx / SCALE;
            let py = (ev.client_y() as f64 - rect.top()) * sy / SCALE;
            for (id, w) in walkers.borrow().iter() {
                if px >= w.x && px < w.x + pixel::SCENE_W as f64
                    && py >= w.y && py < w.y + pixel::SCENE_H as f64
                {
                    selected.set(Some(id.clone()));
                    return;
                }
            }
        }
    };

    view! {
        <div class="floor">
            <div class="floor-bar">
                <span class="dim">"theme"</span>
                <select on:change=move |e| {
                    theme.set(event_target_value(&e).parse().unwrap_or(0));
                }>
                    {names.into_iter().enumerate().map(|(i, n)| view! {
                        <option value=i.to_string()>{n}</option>
                    }).collect::<Vec<_>>()}
                </select>
            </div>
            <canvas node_ref=canvas_ref
                    width=(ROOM_W * SCALE) as u32
                    height=(ROOM_H * SCALE) as u32
                    on:click=on_click/>
        </div>
    }
}

/// Paint one frame: room, stations, then figures back-to-front.
#[allow(clippy::too_many_arguments)]
fn draw(
    ctx: &CanvasRenderingContext2d,
    el: &HtmlCanvasElement,
    doc: &web_sys::Document,
    t: &Theme,
    list: &[Occupant],
    walkers: &HashMap<String, Walker>,
    sprites: &mut Sprites,
    selected: Option<String>,
) {
    ctx.set_image_smoothing_enabled(false);
    ctx.save();
    let _ = ctx.scale(SCALE, SCALE);

    let floor = t.floor.as_deref().unwrap_or("#c8b89a");
    let wall = t.wall.as_deref().unwrap_or("#e8e0d0");

    // Wall band across the back, then the floor. The wall is what makes it read
    // as a room rather than a rug.
    ctx.set_fill_style_str(wall);
    ctx.fill_rect(0.0, 0.0, ROOM_W, 46.0);
    ctx.set_fill_style_str(floor);
    ctx.fill_rect(0.0, 46.0, ROOM_W, ROOM_H - 46.0);
    // Skirting, to separate the two planes.
    ctx.set_fill_style_str("#8a7c66");
    ctx.fill_rect(0.0, 44.0, ROOM_W, 2.0);

    // Desk row down the middle, where agents idle.
    ctx.set_fill_style_str("#a08d70");
    for i in 0..4 {
        let x = 56.0 + i as f64 * 56.0;
        ctx.fill_rect(x, 120.0, 44.0, 14.0);
        ctx.set_fill_style_str("#8a7659");
        ctx.fill_rect(x, 132.0, 44.0, 3.0);
        ctx.set_fill_style_str("#a08d70");
    }

    for s in stations() {
        ctx.set_fill_style_str(match s.kind {
            "board" => "#7a6a4e",
            "shelf" => "#6f5b40",
            "web" => "#4e6070",
            "terminal" => "#3f4650",
            _ => "#6a6a72",
        });
        ctx.fill_rect(s.x, s.y, s.w, 22.0);
        ctx.set_fill_style_str("#2a2620");
        ctx.set_font("7px ui-monospace, monospace");
        let _ = ctx.fill_text(s.label, s.x + 3.0, s.y + 13.0);
    }

    // Back to front, so a figure lower on the floor overlaps one behind it.
    let mut order: Vec<&Occupant> = list.iter().collect();
    order.sort_by(|a, b| {
        let ay = walkers.get(&a.id).map(|w| w.y).unwrap_or(0.0);
        let by = walkers.get(&b.id).map(|w| w.y).unwrap_or(0.0);
        ay.partial_cmp(&by).unwrap_or(std::cmp::Ordering::Equal)
    });

    for o in order {
        let Some(w) = walkers.get(&o.id) else { continue };
        let Some(frames) = sprites.get(doc, t, &o.archetype) else { continue };
        let set = if w.facing_back { &frames.back } else { &frames.front };
        let Some(sprite) = set.get(w.step as usize) else { continue };

        // A soft shadow anchors the figure to the floor; without one it looks
        // pasted on.
        ctx.set_fill_style_str("rgba(0,0,0,0.16)");
        ctx.begin_path();
        let _ = ctx.ellipse(
            w.x + pixel::SCENE_W as f64 / 2.0,
            w.y + pixel::SCENE_H as f64 - 1.0,
            7.0, 2.5, 0.0, 0.0, std::f64::consts::TAU,
        );
        ctx.fill();

        let _ = ctx.draw_image_with_html_canvas_element(sprite, w.x.round(), w.y.round());

        if selected.as_deref() == Some(o.id.as_str()) {
            ctx.set_stroke_style_str("#7aa2f7");
            ctx.set_line_width(1.0);
            ctx.stroke_rect(w.x - 1.5, w.y - 1.5, pixel::SCENE_W as f64 + 3.0, pixel::SCENE_H as f64 + 3.0);
        }

        // Name plate under the feet, with a status pip.
        let cx = w.x + pixel::SCENE_W as f64 / 2.0;
        ctx.set_font("6px ui-monospace, monospace");
        ctx.set_text_align("center");
        ctx.set_fill_style_str("rgba(20,24,30,0.55)");
        let label_w = (o.name.len() as f64 * 3.6).max(14.0) + 8.0;
        ctx.fill_rect(cx - label_w / 2.0, w.y + pixel::SCENE_H as f64 + 1.0, label_w, 8.0);
        ctx.set_fill_style_str("#e6edf6");
        let _ = ctx.fill_text(&o.name, cx + 2.0, w.y + pixel::SCENE_H as f64 + 7.0);
        ctx.set_fill_style_str(status_colour(&o.status, o.live));
        ctx.fill_rect(cx - label_w / 2.0 + 2.0, w.y + pixel::SCENE_H as f64 + 3.0, 3.0, 3.0);
        ctx.set_text_align("start");
    }

    ctx.restore();
    let _ = el;
}
