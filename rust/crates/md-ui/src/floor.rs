//! The office floor.
//!
//! **Canvas 2D, not wgpu** — a deliberate divergence from the conversion plan.
//! The art is 18×28 pixel sprites and a flat tile grid; at that size a GPU
//! pipeline is machinery without a payoff, and it would cost a shader stack, a
//! surface lifecycle and a device-lost path to blit a few dozen sprites. Canvas
//! draws this at frame rate with none of that. If the floor ever grows real
//! lighting or thousands of entities, the renderer is one module to replace.
//!
//! Sprites are painted ONCE into an offscreen bitmap per character and then
//! blitted. Re-running the recipe painter every frame would redo the same
//! per-pixel work sixty times a second for art that never changes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use leptos::prelude::*;
use wasm_bindgen::{Clamped, JsCast};
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageData};

use crate::pixel;
use crate::theme::{self, Theme};

/// Pixels per art pixel. Integer, and nearest-neighbour sampling is disabled
/// below — a fractional scale or smoothing turns pixel art into mush.
const SCALE: f64 = 3.0;
/// Tile grid for desks.
const DESK_W: f64 = 26.0;
const DESK_H: f64 = 30.0;

/// Where one agent's desk was drawn, so a click can be resolved against the
/// same layout the paint produced rather than a recomputed one.
struct Hit {
    id: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// One agent as the floor needs to draw it.
#[derive(Clone, PartialEq)]
pub struct Occupant {
    pub id: String,
    pub name: String,
    pub archetype: String,
    /// `working` | `idle` | `blocked` | `waiting`
    pub status: String,
    pub live: bool,
}

/// A character's art, painted once and kept as an offscreen canvas.
///
/// A canvas rather than an `ImageData`, because `putImageData` ignores the
/// transform and so cannot scale — and drawing pixel art at 1:1 on a 3x floor
/// would put a postage stamp on every desk. `drawImage` from a canvas scales,
/// which is the whole reason the bitmap is staged here first.
struct Sprites {
    cache: HashMap<String, HtmlCanvasElement>,
}

impl Sprites {
    fn new() -> Self {
        Self { cache: HashMap::new() }
    }

    /// The painted portrait for an archetype in this theme.
    ///
    /// Keyed by theme AND archetype: switching themes must not hand back the
    /// previous cast's art for the same slot.
    fn get(&mut self, doc: &web_sys::Document, theme: &Theme, archetype: &str) -> Option<&HtmlCanvasElement> {
        let key = format!("{}:{archetype}", theme.id);
        if !self.cache.contains_key(&key) {
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
            let octx: CanvasRenderingContext2d =
                off.get_context("2d").ok()??.unchecked_into();
            octx.put_image_data(&data, 0.0, 0.0).ok()?;
            self.cache.insert(key.clone(), off);
        }
        self.cache.get(&key)
    }
}

fn status_colour(status: &str) -> &'static str {
    match status {
        "working" => "#4ec994",
        "blocked" => "#e5675a",
        "waiting" => "#d8a657",
        _ => "#5a6473",
    }
}

#[component]
pub fn Floor(
    occupants: Signal<Vec<Occupant>>,
    theme: RwSignal<usize>,
    selected: RwSignal<Option<String>>,
) -> impl IntoView {
    let canvas_ref = NodeRef::<leptos::html::Canvas>::new();
    let sprites = Rc::new(RefCell::new(Sprites::new()));
    let themes = Rc::new(theme::builtin());

    // Filled by the paint, read by the click handler: recomputing the layout in
    // two places is how a click and a desk drift apart.
    let boxes: Rc<RefCell<Vec<Hit>>> = Rc::new(RefCell::new(Vec::new()));

    {
        let (sprites, themes, boxes) = (sprites.clone(), themes.clone(), boxes.clone());
        Effect::new(move |_| {
            let list = occupants.get();
            let theme_idx = theme.get();
            let Some(canvas) = canvas_ref.get() else { return };
            let Some(t) = themes.get(theme_idx % themes.len().max(1)) else { return };

            let Some(doc) = window().document() else { return };
            let el: HtmlCanvasElement = canvas.unchecked_into();
            let Ok(Some(ctx)) = el.get_context("2d") else { return };
            let ctx: CanvasRenderingContext2d = ctx.unchecked_into();

            let per_row = ((el.width() as f64) / (DESK_W * SCALE)).floor().max(1.0) as usize;
            let rows = list.len().div_ceil(per_row).max(1);
            let needed = (rows as f64 * DESK_H * SCALE) as u32;
            if el.height() != needed {
                el.set_height(needed);
            }

            ctx.set_image_smoothing_enabled(false);
            ctx.set_fill_style_str(t.floor.as_deref().unwrap_or("#c8b89a"));
            ctx.fill_rect(0.0, 0.0, el.width() as f64, el.height() as f64);

            let mut hits = Vec::new();
            let mut s = sprites.borrow_mut();
            for (i, o) in list.iter().enumerate() {
                let col = (i % per_row) as f64;
                let row = (i / per_row) as f64;
                let x = col * DESK_W * SCALE;
                let y = row * DESK_H * SCALE;

                // Desk slab under the figure, so a character reads as sitting AT
                // something rather than floating on the floor colour.
                ctx.set_fill_style_str(t.wall.as_deref().unwrap_or("#e8e0d0"));
                ctx.fill_rect(x + 2.0, y + 22.0 * SCALE, DESK_W * SCALE - 4.0, 7.0 * SCALE);

                if let Some(sprite) = s.get(&doc, t, &o.archetype) {
                    let _ = ctx.draw_image_with_html_canvas_element_and_dw_and_dh(
                        sprite,
                        x + 4.0 * SCALE,
                        y + 2.0 * SCALE,
                        pixel::W as f64 * SCALE,
                        pixel::H as f64 * SCALE,
                    );
                }

                // Status pip and name.
                ctx.set_fill_style_str(status_colour(if o.live { &o.status } else { "gone" }));
                ctx.fill_rect(x + 4.0, y + 4.0, 6.0, 6.0);
                ctx.set_fill_style_str("#1b2028");
                ctx.set_font("11px ui-monospace, monospace");
                let _ = ctx.fill_text(&o.name, x + 6.0, y + DESK_H * SCALE - 16.0);
                // Who they are DRESSED as, under who they are. Without this a
                // theme switch is invisible unless you already know the cast —
                // the art changes but nothing says why.
                if let Some(c) = t.character(&o.archetype) {
                    ctx.set_fill_style_str("#6b7688");
                    ctx.set_font("9px ui-monospace, monospace");
                    let _ = ctx.fill_text(&c.display, x + 6.0, y + DESK_H * SCALE - 5.0);
                }

                if selected.get().as_deref() == Some(o.id.as_str()) {
                    ctx.set_stroke_style_str("#7aa2f7");
                    ctx.set_line_width(2.0);
                    ctx.stroke_rect(x + 1.0, y + 1.0, DESK_W * SCALE - 2.0, DESK_H * SCALE - 2.0);
                }
                hits.push(Hit { id: o.id.clone(), x, y, w: DESK_W * SCALE, h: DESK_H * SCALE });
            }
            *boxes.borrow_mut() = hits;
        });
    }

    let on_click = {
        let boxes = boxes.clone();
        move |ev: leptos::ev::MouseEvent| {
            let Some(canvas) = canvas_ref.get() else { return };
            let el: HtmlCanvasElement = canvas.unchecked_into();
            let rect = el.get_bounding_client_rect();
            // The canvas is laid out by CSS, so a click in page space has to be
            // scaled into the backing-store space the hit boxes are in.
            let sx = el.width() as f64 / rect.width().max(1.0);
            let sy = el.height() as f64 / rect.height().max(1.0);
            let px = (ev.client_x() as f64 - rect.left()) * sx;
            let py = (ev.client_y() as f64 - rect.top()) * sy;
            for hit in boxes.borrow().iter() {
                if px >= hit.x && px < hit.x + hit.w && py >= hit.y && py < hit.y + hit.h {
                    selected.set(Some(hit.id.clone()));
                    return;
                }
            }
        }
    };

    let names: Vec<String> = theme::builtin().iter().map(|t| t.name.clone()).collect();

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
            <canvas node_ref=canvas_ref width="900" height="120" on:click=on_click/>
        </div>
    }
}
