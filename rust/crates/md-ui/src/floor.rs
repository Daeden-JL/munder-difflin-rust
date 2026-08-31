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

/// How long a spoken line stays up. Long enough to read, short enough that the
/// floor does not turn into a wall of text.
const SAY_SECS: f64 = 4.0;

/// One agent's motion state. Kept OUTSIDE the reactive graph: it changes every
/// frame, and routing sixty updates a second through signals would re-render
/// the surrounding view for something only the canvas cares about.
struct Walker {
    x: f64,
    y: f64,
    tx: f64,
    ty: f64,
    /// Which station it is heading to, if any. `None` means wandering.
    at: Option<String>,
    facing_back: bool,
    step: u8,
    step_t: f64,
    linger: f64,
    /// Where this theme lets an agent wander.
    roam: [f64; 4],
    /// A per-agent deterministic seed, so wandering differs between agents but
    /// is not driven by a shared global.
    seed: u32,
    /// What this character is saying, and for how long. Flavour is the point of
    /// the floor: a room of silent figures is a status board with legs.
    saying: Option<String>,
    say_t: f64,
    /// How restless this character is, from its personality.
    restless: f64,
    /// This character's own post. Idling returns here rather than stopping
    /// wherever the last wander ended — a floor where everyone drifts anywhere
    /// reads as a crowd, not a workplace.
    home: Option<[f64; 2]>,
    /// Where they are found when they are not at their post.
    ///
    /// Without it, "not at your desk" means anywhere in the roam box, and a
    /// crew with workstations still reads as a crowd the moment it moves. With
    /// it, Wash off the bridge is in the galley — which is a fact about Wash.
    haunt: Option<[f64; 2]>,
    /// Walking in for the first time. A newly hired agent arrives through a
    /// door instead of materialising at its desk.
    entering: bool,
}

impl Walker {
    fn new(index: usize, seed: u32, roam: [f64; 4], restless: f64) -> Self {
        // Start spread across the roaming area rather than stacked at the origin.
        let [x0, y0, x1, y1] = roam;
        let x = x0 + ((index as f64 * 37.0) % (x1 - x0).max(1.0));
        let y = y0 + ((index % 3) as f64) * ((y1 - y0) / 3.0);
        Self {
            x, y, tx: x, ty: y, at: None, facing_back: false, step: 0, step_t: 0.0,
            linger: 0.0, roam, seed, saying: None, say_t: 0.0, restless,
            home: None, haunt: None, entering: false,
        }
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

    /// Say something, unless already mid-sentence — interrupting a line the
    /// reader has not finished is worse than staying quiet.
    fn say(&mut self, lines: &[String]) {
        if self.saying.is_some() || lines.is_empty() {
            return;
        }
        let i = (self.rand() * lines.len() as f64) as usize;
        self.saying = Some(lines[i.min(lines.len() - 1)].clone());
        self.say_t = SAY_SECS;
    }

    fn advance(&mut self, dt: f64, p: &theme::Personality) {
        if self.saying.is_some() {
            self.say_t -= dt;
            if self.say_t <= 0.0 {
                self.saying = None;
            }
        }

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
        // Arriving: the first stop is the desk, and the walk in is the
        // introduction.
        if self.entering {
            self.entering = false;
            if let Some([hx, hy]) = self.home {
                self.tx = hx;
                self.ty = hy;
                return;
            }
        }

        self.linger -= dt;
        if self.linger <= 0.0 {
            // A restless character lingers briefly; a still one settles. This is
            // the personality showing in movement rather than only in words.
            let patience = 1.0 - self.restless.clamp(0.0, 1.0);
            self.linger = LINGER_SECS * (0.4 + patience * 1.8) + self.rand() * LINGER_SECS;

            // Mostly go home; sometimes wander. How often depends on how
            // restless the character is, so Creed roams and Stanley sits.
            let wander = self.rand() < 0.25 + self.restless.clamp(0.0, 1.0) * 0.5;
            // A little scatter around a post, so two visits do not land on the
            // identical pixel.
            let settle = |w: &mut Self, [hx, hy]: [f64; 2]| {
                w.tx = hx + (w.rand() - 0.5) * 6.0;
                w.ty = hy + (w.rand() - 0.5) * 4.0;
            };
            let second = self.haunt.filter(|_| self.rand() < 0.6);
            match (self.home, wander, second) {
                (Some(h), false, _) => settle(self, h),
                (_, true, Some(h)) => settle(self, h),
                _ => {
                    let [x0, y0, x1, y1] = self.roam;
                    self.tx = x0 + self.rand() * (x1 - x0);
                    self.ty = y0 + self.rand() * (y1 - y0);
                }
            }
            // Muttering on settling, not on a timer: the line reads as a thought
            // rather than a ticker.
            if self.rand() < 0.45 {
                self.say(&p.idle);
            }
        }
    }

    fn send_to(&mut self, s: &theme::Station) {
        self.at = Some(s.kind.clone());
        // Stand just clear of the station's bottom edge, so the figure does not
        // cover the label. Off the station's own height rather than a constant:
        // a console drawn flat on a deck plan is a tenth the depth of one drawn
        // side-on, and a fixed offset put the figure in the next room along.
        self.tx = s.x + s.w / 2.0 - pixel::SCENE_W as f64 / 2.0;
        self.ty = s.y + s.h - 2.0;
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
    /// Where this agent was posted when it was hired, as POI ids.
    ///
    /// The operator's answer, and it beats the character's own: someone who
    /// put their engineer on the bridge meant it. Empty, or naming a place
    /// this theme does not have, falls back to the character — which is the
    /// ordinary case after a theme switch, since a Serenity post id means
    /// nothing on the Office floor.
    pub primary_poi: String,
    pub secondary_poi: String,
}

/// Where an agent belongs on this floor, and where it is found when it is not
/// there. Its own posting first, then the character's, then the desk.
fn posts(t: &Theme, o: &Occupant) -> (Option<[f64; 2]>, Option<[f64; 2]>) {
    let pick = |id: &str, secondary: bool| {
        t.poi(id)
            .map(|p| [p.x, p.y])
            .or_else(|| t.post(&o.archetype, secondary))
    };
    (pick(&o.primary_poi, false), pick(&o.secondary_poi, true))
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

/// The one-line self-description for a character.
pub fn character_trait(theme: &Theme, archetype: &str) -> Option<String> {
    theme
        .character(archetype)
        .map(|c| c.personality.trait_line.clone())
        .filter(|t| !t.is_empty())
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
    /// agent id → archetype. Passed in rather than recomputed so the floor and
    /// the roster cannot disagree about who is dressed as whom.
    archetypes: Signal<HashMap<String, String>>,
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
            let themes = theme::builtin();
            let Some(t) = themes.get(theme.get_untracked() % themes.len().max(1)) else { return };
            let Some(station) = t.layout.stations.iter().find(|s| s.kind == kind) else { return };

            let mut ws = walkers.borrow_mut();
            let Some(w) = ws.get_mut(&id) else { return };
            w.send_to(station);
            // Say something on being given work, so the floor narrates itself.
            if let Some(arch) = archetypes.get_untracked().get(&id) {
                if let Some(c) = t.character(arch) {
                    w.say(&c.personality.working);
                }
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
                            let restless = t.character(&o.archetype)
                                .map(|c| c.personality.restless)
                                .unwrap_or(0.5);
                            let mut walker = Walker::new(i, seed, t.layout.roam, restless);
                            let (home, haunt) = posts(t, o);
                            walker.home = home;
                            walker.haunt = haunt;
                            // Arrive through a doorway rather than appearing at
                            // the desk: someone joining the floor should be seen
                            // to join it.
                            if let Some(d) = t.layout.doors.first() {
                                walker.x = d.threshold[0];
                                walker.y = d.threshold[1];
                                walker.entering = true;
                                walker.tx = walker.x;
                                walker.ty = walker.y;
                            }
                            w.insert(o.id.clone(), walker);
                        }
                    }
                    for o in list.iter() {
                        let p = t.character(&o.archetype).map(|c| c.personality.clone()).unwrap_or_default();
                        if let Some(walker) = w.get_mut(&o.id) {
                            // The roam box belongs to the THEME, so switching
                            // themes has to re-home everyone or they wander into
                            // the new room's walls.
                            walker.roam = t.layout.roam;
                            walker.restless = p.restless;
                            // Posts belong to the THEME, so a switch re-homes
                            // everyone rather than leaving them at the old room's
                            // coordinates.
                            let (home, haunt) = posts(t, o);
                            walker.home = home;
                            walker.haunt = haunt;
                            walker.advance(dt, &p);
                        }
                    }

                    // Two characters standing together greet each other. This is
                    // the interaction the floor is FOR — a room where nobody
                    // acknowledges anybody is a status board with legs.
                    let ids: Vec<String> = list.iter().map(|o| o.id.clone()).collect();
                    for i in 0..ids.len() {
                        for j in (i + 1)..ids.len() {
                            let near = match (w.get(&ids[i]), w.get(&ids[j])) {
                                (Some(a), Some(b)) => {
                                    (a.x - b.x).abs() < 22.0 && (a.y - b.y).abs() < 10.0
                                        && a.arrived() && b.arrived()
                                }
                                _ => false,
                            };
                            if !near {
                                continue;
                            }
                            let arch = list[i].archetype.clone();
                            let lines = t.character(&arch).map(|c| c.personality.greet.clone()).unwrap_or_default();
                            if let Some(a) = w.get_mut(&ids[i]) {
                                // Rare, or neighbours would chatter constantly.
                                if a.saying.is_none() && a.rand() < 0.02 {
                                    a.say(&lines);
                                }
                            }
                        }
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

    // The room comes from the THEME. A bridge is not an office, and dressing
    // one room differently would make every theme a palette swap.
    let l = &t.layout;
    ctx.set_fill_style_str(&l.wall);
    ctx.fill_rect(0.0, 0.0, ROOM_W, l.wall_depth);
    ctx.set_fill_style_str(&l.floor);
    ctx.fill_rect(0.0, l.wall_depth, ROOM_W, ROOM_H - l.wall_depth);
    if let Some(trim) = &l.trim {
        ctx.set_fill_style_str(trim);
        ctx.fill_rect(0.0, l.wall_depth - 2.0, ROOM_W, 2.0);
    }

    // A blueprint grid, for a room read as a plan rather than as a room. Drawn
    // under everything, so it reads as paper the ship is printed on.
    if let Some(grid) = &l.grid {
        ctx.set_fill_style_str(grid);
        let step = l.grid_step.max(2.0);
        let mut x = 0.0;
        while x < ROOM_W {
            ctx.fill_rect(x, l.wall_depth, 1.0, ROOM_H - l.wall_depth);
            x += step;
        }
        let mut y = l.wall_depth;
        while y < ROOM_H {
            ctx.fill_rect(0.0, y, ROOM_W, 1.0);
            y += step;
        }
    }

    // Scenery, in order — later props sit on top, which is how a theme author
    // layers a console onto a dais without needing a z-index.
    for p in &l.props {
        ctx.set_fill_style_str(&p.color);
        if p.round {
            ctx.begin_path();
            let _ = ctx.ellipse(
                p.x + p.w / 2.0, p.y + p.h / 2.0, p.w / 2.0, p.h / 2.0,
                0.0, 0.0, std::f64::consts::TAU,
            );
            ctx.fill();
        } else {
            ctx.fill_rect(p.x, p.y, p.w, p.h);
        }
        // An outline is what turns a stack of rectangles into a deck plan:
        // rooms on a plan are read from their walls, not their fill. Stroked at
        // whole-pixel offsets, or a 1px line lands across two rows and blurs.
        if let Some(b) = &p.border {
            ctx.set_stroke_style_str(b);
            ctx.set_line_width(1.0);
            ctx.stroke_rect(p.x + 0.5, p.y + 0.5, p.w - 1.0, p.h - 1.0);
        }
        // A darker front lip is what makes a flat slab read as a surface you
        // could put something on.
        if p.lip {
            ctx.set_fill_style_str("rgba(0,0,0,0.24)");
            ctx.fill_rect(p.x, p.y + p.h - 2.0, p.w, 2.0);
        }
    }

    // Doorways, drawn on the wall band before anything stands in front of them.
    for d in &l.doors {
        ctx.set_fill_style_str(d.color.as_deref().unwrap_or("#3a3a42"));
        ctx.fill_rect(d.x, d.y, d.w, d.h);
        // A lighter inner panel reads as depth — an opening rather than a
        // painted rectangle.
        ctx.set_fill_style_str("rgba(255,255,255,0.10)");
        ctx.fill_rect(d.x + 2.0, d.y + 2.0, d.w - 4.0, d.h - 4.0);
        // Only where there is room for it. A hatch drawn six pixels deep on a
        // deck plan gets its name from the place it opens onto instead, and a
        // label painted across it would be unreadable in both.
        if d.h >= 14.0 {
            ctx.set_fill_style_str("rgba(255,255,255,0.7)");
            ctx.set_font("5px ui-monospace, monospace");
            let _ = ctx.fill_text(&d.label, d.x + 1.0, d.y + d.h - 2.0);
        }
    }


    for st in &l.stations {
        ctx.set_fill_style_str(st.color.as_deref().unwrap_or("#6a6a72"));
        ctx.fill_rect(st.x, st.y, st.w, st.h);
        ctx.set_fill_style_str("rgba(255,255,255,0.82)");
        ctx.set_font("6px ui-monospace, monospace");
        // Centred in the slab rather than a fixed drop from its top, so a
        // shallow console keeps its label inside itself.
        let _ = ctx.fill_text(&st.label, st.x + 3.0, st.y + st.h / 2.0 + 2.0);
    }
    // The map's legend. Painted before the figures, so someone standing at a
    // post covers their own label rather than being covered by it.
    if l.poi_labels {
        ctx.set_font("4px ui-monospace, monospace");
        ctx.set_text_align("center");
        for poi in &l.pois {
            ctx.set_fill_style_str("rgba(150,190,235,0.85)");
            let _ = ctx.fill_text(
                &poi.label,
                poi.x + pixel::SCENE_W as f64 / 2.0,
                (poi.y - 4.0).max(5.0),
            );
        }
        ctx.set_text_align("start");
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

        let cx = w.x + pixel::SCENE_W as f64 / 2.0;

        // Speech. Drawn above the head so it never covers the figure, and
        // clamped to the room so a line near an edge stays readable.
        if let Some(line) = &w.saying {
            ctx.set_font("6px ui-monospace, monospace");
            ctx.set_text_align("center");
            let bw = (line.chars().count() as f64 * 3.5 + 8.0).min(150.0);
            let bx = (cx - bw / 2.0).clamp(2.0, ROOM_W - bw - 2.0);
            let by = w.y - 12.0;
            ctx.set_fill_style_str("rgba(248,250,252,0.94)");
            ctx.fill_rect(bx, by, bw, 10.0);
            // A little tail, so the bubble belongs to this figure.
            ctx.fill_rect(cx - 1.5, by + 10.0, 3.0, 2.0);
            ctx.set_fill_style_str("#1b2028");
            let _ = ctx.fill_text(line, bx + bw / 2.0, by + 7.0);
            ctx.set_text_align("start");
        }

        // The name plate shows who they are DRESSED AS. Switching themes is
        // supposed to change who you are looking at, and a label that kept the
        // agent's own name made the change invisible.
        let shown = t.character(&o.archetype).map(|c| c.display.clone()).unwrap_or_else(|| o.name.clone());
        ctx.set_font("6px ui-monospace, monospace");
        ctx.set_text_align("center");
        ctx.set_fill_style_str("rgba(20,24,30,0.62)");
        let label_w = (shown.chars().count() as f64 * 3.6).max(14.0) + 10.0;
        ctx.fill_rect(cx - label_w / 2.0, w.y + pixel::SCENE_H as f64 + 1.0, label_w, 8.0);
        ctx.set_fill_style_str("#e6edf6");
        let _ = ctx.fill_text(&shown, cx + 2.0, w.y + pixel::SCENE_H as f64 + 7.0);
        ctx.set_fill_style_str(status_colour(&o.status, o.live));
        ctx.fill_rect(cx - label_w / 2.0 + 2.0, w.y + pixel::SCENE_H as f64 + 3.0, 3.0, 3.0);
        ctx.set_text_align("start");
    }

    ctx.restore();
    let _ = el;
}
