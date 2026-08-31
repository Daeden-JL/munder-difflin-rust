//! The theme engine.
//!
//! A theme is DATA: a cast of characters, each an art recipe plus a name. The
//! painter (`pixel.rs`) never names a character, so adding a cast costs a file
//! rather than a code change.
//!
//! **Agents bind to an archetype, not to a character.** Binding an agent to
//! `dwight` is meaningless the moment someone switches themes; binding it to
//! `second` is not. Each theme fills the same slots, so a switch preserves every
//! agent's identity, memory and desk and changes only who they look like.
//!
//! ```text
//! leader   Michael  →  Mal          engineer  —        →  Kaylee
//! second   Dwight   →  Zoë          analyst   Oscar    →  Simon
//! operator Jim      →  Wash         wildcard  Creed    →  River
//! ```
//!
//! Assignment is by ORDER of arrival on the floor, and it is stable: the first
//! agent takes `leader`, the second `second`, and an agent keeps its slot for as
//! long as it exists. Anything derived from the agent's name would reshuffle the
//! floor whenever someone renamed an agent.

use std::collections::HashMap;

use leptos::prelude::*;
use serde::Deserialize;

use crate::pixel::Recipe;

/// The slots every theme must fill, in assignment order. A theme with fewer
/// characters than slots wraps, so a small cast still dresses a large floor.
pub const ARCHETYPES: [&str; 9] = [
    "leader", "second", "operator", "engineer", "analyst",
    "wildcard", "muscle", "counsel", "liaison",
];

#[derive(Debug, Clone, Deserialize)]
pub struct Character {
    /// Who the agent is dressed as. This is what the floor and the roster show:
    /// switching themes is supposed to change who you are looking at.
    pub display: String,
    pub recipe: Recipe,
    /// What this character says when idle, and how they behave.
    ///
    /// Flavour is DATA, like the art. The Office floor's charm came from
    /// characters having opinions — hard-coded ones, which is why it was one
    /// cast forever. A theme that ships its own lines is a theme with a
    /// personality rather than a re-skin.
    #[serde(default)]
    pub personality: Personality,
}

/// A character's observable behaviour.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Personality {
    /// One-line self-description, shown beside the agent in the conversation
    /// header. Personality you can read, not only overhear.
    #[serde(default)]
    pub trait_line: String,
    /// Muttered while idle at a desk.
    #[serde(default)]
    pub idle: Vec<String>,
    /// Said on arriving at a station to do work.
    #[serde(default)]
    pub working: Vec<String>,
    /// Said when two characters end up next to each other.
    #[serde(default)]
    pub greet: Vec<String>,
    /// How restless they are: 0.0 sits still, 1.0 rarely stops moving. Scales
    /// how long they linger before wandering.
    #[serde(default = "half")]
    pub restless: f64,
    /// Where this character works, as a POI id, and where they are found when
    /// they are not there.
    ///
    /// The theme's DEFAULT, not the agent's answer: an agent may be posted
    /// anywhere on the map, and this is what it gets if nobody says otherwise.
    /// A crew with workstations reads as a crew; one that drifts reads as a
    /// crowd, and the difference is entirely this pair of strings.
    #[serde(default)]
    pub primary_poi: String,
    #[serde(default)]
    pub secondary_poi: String,
}

fn half() -> f64 {
    0.5
}

/// A named place on the map: somewhere a character can be posted.
///
/// Stations answer "where does this TOOL happen"; a POI answers "where does
/// this PERSON belong". The two are deliberately separate — the engine room is
/// Kaylee's post whether or not she is running a shell there, and a floor where
/// the only destinations are tool stations empties out the moment nobody is
/// working.
///
/// `x`/`y` is a standing spot: the top-left of the figure, in room pixels, the
/// same coordinate space desks are authored in.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Poi {
    /// Stable, lowercase, referenced by characters and by agents. Renaming one
    /// orphans every reference, which is why it is not the label.
    pub id: String,
    /// What the map calls it.
    pub label: String,
    pub x: f64,
    pub y: f64,
}

/// One place on the floor. Positions are in ROOM pixels, so a theme's layout is
/// authored against its own room rather than against a shared grid.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Station {
    /// Which tool group sends an agent here: shelf, terminal, web, board,
    /// mailbox, desk.
    pub kind: String,
    pub label: String,
    pub x: f64,
    pub y: f64,
    #[serde(default = "default_w")]
    pub w: f64,
    /// How tall the console is. A room seen side-on can afford a chunky slab;
    /// a room seen from above cannot, because the slab would be most of the
    /// room. The figure stands just clear of the bottom edge either way, so
    /// changing this moves the console AND the person who works at it.
    #[serde(default = "default_h")]
    pub h: f64,
    /// Fill colour. Themes differ more in their furniture than their cast.
    #[serde(default)]
    pub color: Option<String>,
    /// Where the person working at this console stands, as the top-left of the
    /// figure — the same coordinates a post is authored in.
    ///
    /// Authored, because the fallback below is a guess: "just under the slab"
    /// is right for a console on a wall and lands in the next room along on a
    /// deck plan. A theme that has not named its spots still works.
    #[serde(default)]
    pub spot: Option<[f64; 2]>,
}

impl Station {
    /// Where to stand to work here.
    pub fn spot(&self) -> [f64; 2] {
        self.spot.unwrap_or([
            self.x + self.w / 2.0 - crate::pixel::SCENE_W as f64 / 2.0,
            self.y + self.h - 2.0,
        ])
    }
}

fn default_w() -> f64 {
    30.0
}

fn default_h() -> f64 {
    22.0
}

/// One piece of scenery.
///
/// A rectangle with a colour and an optional darker lip along its front edge —
/// which is what makes a flat slab read as a surface you could put a mug on.
/// Deliberately not a sprite: a room built from primitives is a room a theme
/// author can write in a text editor.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Prop {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub color: String,
    /// A front lip, for anything with a top surface.
    #[serde(default)]
    pub lip: bool,
    /// Draw as an ellipse: rugs, hatches, the warp core.
    #[serde(default)]
    pub round: bool,
    /// An outline. What turns a stack of rectangles into a deck plan: rooms on
    /// a blueprint are read from their walls, not their fill.
    #[serde(default)]
    pub border: Option<String>,
}

/// The room a theme is set in.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Layout {
    /// Back wall colour, and how deep the wall band is.
    pub wall: String,
    #[serde(default = "wall_depth")]
    pub wall_depth: f64,
    pub floor: String,
    /// Skirting / trim between the two planes.
    #[serde(default)]
    pub trim: Option<String>,
    /// Everything drawn on the floor that is not a station: desks, consoles,
    /// crates, rugs, bulkheads. Painted in order, so later entries sit on top.
    #[serde(default)]
    pub props: Vec<Prop>,
    pub stations: Vec<Station>,
    /// Each archetype's own desk, keyed by slot.
    ///
    /// Characters gravitate HOME. In the original, seats were claimed in order
    /// with the orchestrator taking the corner office — a floor where everyone
    /// wanders anywhere reads as a crowd, not a workplace.
    #[serde(default)]
    pub desks: HashMap<String, [f64; 2]>,
    /// Doorways: where characters enter from and leave through.
    ///
    /// A room with no way in is a diorama. An agent that has just been hired
    /// walks in through one of these, and an archived one walks out.
    #[serde(default)]
    pub doors: Vec<Door>,
    /// Every named place on this map.
    ///
    /// The map's legend AND its posting list: a theme that names its rooms lets
    /// an operator say "Kaylee works in the engine room" instead of typing a
    /// coordinate nobody can check.
    #[serde(default)]
    pub pois: Vec<Poi>,
    /// Paint the POI names onto the room. True for a map read as a plan, false
    /// for a room read as a room — an office does not label its own kitchen.
    #[serde(default)]
    pub poi_labels: bool,
    /// A blueprint grid over the floor, and how far apart its lines are.
    #[serde(default)]
    pub grid: Option<String>,
    #[serde(default = "grid_step")]
    pub grid_step: f64,
    /// The rectangles a character can stand in, as `[x0, y0, x1, y1]` in FEET
    /// space — where they stand, not where their sprite's top-left corner is.
    ///
    /// Two that overlap are joined, and the overlap is the doorway. Empty means
    /// the whole room is one open floor, which is true of a room drawn side-on
    /// and is why the four of those carry none: a straight line across an
    /// office is a walk somebody could take. A deck plan is rooms and
    /// corridors, and a straight line across one crosses bulkheads.
    #[serde(default)]
    pub walk: Vec<[f64; 4]>,
    /// Where agents wander when they are not at a desk: `[x0, y0, x1, y1]`.
    pub roam: [f64; 4],
}

fn grid_step() -> f64 {
    8.0
}

/// A way in or out of the room.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Door {
    pub label: String,
    pub x: f64,
    pub y: f64,
    #[serde(default = "door_w")]
    pub w: f64,
    #[serde(default = "door_h")]
    pub h: f64,
    #[serde(default)]
    pub color: Option<String>,
    /// Where someone standing in this doorway is: the point they walk from on
    /// arrival, and to on departure.
    pub threshold: [f64; 2],
}

fn door_w() -> f64 {
    22.0
}
fn door_h() -> f64 {
    30.0
}

fn wall_depth() -> f64 {
    46.0
}

#[derive(Debug, Clone, Deserialize)]
pub struct Theme {
    pub id: String,
    pub name: String,
    /// The room. A theme is a place as much as a cast — the Office is not the
    /// bridge of a starship, and dressing the same room differently would make
    /// every theme feel like a palette swap.
    pub layout: Layout,
    /// archetype → character.
    pub cast: HashMap<String, Character>,
}

impl Theme {
    /// The character for one archetype, falling back through the slot order.
    ///
    /// A theme that fills only some slots still dresses everyone: a missing
    /// `counsel` borrows from the first slot the theme does define, rather than
    /// leaving an invisible agent on the floor.
    /// A named place, by id. `None` for an id this theme does not have — which
    /// is the normal case after a theme switch, and why every caller falls back
    /// rather than treating it as an error.
    pub fn poi(&self, id: &str) -> Option<&Poi> {
        self.layout.pois.iter().find(|p| p.id == id)
    }

    /// Where a character belongs on this map: their own post, then the
    /// archetype's desk. Both are optional, so a sparse theme still places
    /// everyone somewhere.
    pub fn post(&self, archetype: &str, secondary: bool) -> Option<[f64; 2]> {
        let c = self.character(archetype)?;
        let id = if secondary {
            &c.personality.secondary_poi
        } else {
            &c.personality.primary_poi
        };
        match self.poi(id) {
            Some(p) => Some([p.x, p.y]),
            None if secondary => None,
            None => self.layout.desks.get(archetype).copied(),
        }
    }

    pub fn character(&self, archetype: &str) -> Option<&Character> {
        self.cast.get(archetype).or_else(|| {
            ARCHETYPES
                .iter()
                .find_map(|a| self.cast.get(*a))
        })
    }
}

/// Assign an archetype to every agent, preferring a character whose name
/// matches the agent's own.
///
/// **The orchestrator takes `leader`** among the unmatched. Without that rule
/// the god landed wherever the alphabet put it — on a floor of
/// michael/dwight/jim the orchestrator came out dressed as Jim while Dwight
/// wore Michael's suit.
///
/// The agents on a Munder Difflin floor are usually NAMED after the cast, so an
/// agent called Pam dressed as Kevin reads as a bug even when the ordering is
/// correct. Where a theme has a character of the same name, that binding wins;
/// everyone else fills the remaining slots in the usual order.
///
/// `names` is `(id, display name)`. Matching is case-insensitive and on the
/// first word, so "Pam Beesly" still finds Pam.
///
/// `pinned` is agent id → archetype, chosen when the agent was hired. It beats
/// both rules: an operator who picked "Kaylee" from the personality list has
/// said what they want, and a later hire whose name happens to collide must not
/// take it away from them.
pub fn assign_in(
    ids: &[String],
    god: Option<&str>,
    theme: Option<&Theme>,
    pinned: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    let mut taken: Vec<String> = Vec::new();

    // Chosen slots first, and only once each: two agents pinned to the same
    // archetype would otherwise share a face, so the second falls through to
    // the ordinary rules.
    for id in ids {
        let Some(slot) = pinned.get(id) else { continue };
        if !ARCHETYPES.contains(&slot.as_str()) || taken.contains(slot) {
            continue;
        }
        out.insert(id.clone(), slot.clone());
        taken.push(slot.clone());
    }

    if let Some(t) = theme {
        for id in ids {
            if out.contains_key(id) {
                continue;
            }
            // The agent id doubles as its name on this floor.
            let first = id.split(['-', '_', ' ']).next().unwrap_or(id).to_lowercase();
            if let Some((slot, _)) = t.cast.iter().find(|(slot, c)| {
                !taken.contains(slot)
                    && c.display.split_whitespace().next().unwrap_or("").to_lowercase() == first
            }) {
                out.insert(id.clone(), slot.clone());
                taken.push(slot.clone());
            }
        }
    }

    // Everyone unmatched fills the remaining slots, god first.
    let mut rest: Vec<&String> = ids.iter().filter(|id| !out.contains_key(*id)).collect();
    rest.sort();
    if let Some(g) = god {
        if let Some(i) = rest.iter().position(|id| id.as_str() == g) {
            let leader = rest.remove(i);
            rest.insert(0, leader);
        }
    }
    let mut free: Vec<&str> =
        ARCHETYPES.iter().copied().filter(|a| !taken.iter().any(|t| t == a)).collect();
    if free.is_empty() {
        free = ARCHETYPES.to_vec();
    }
    for (i, id) in rest.into_iter().enumerate() {
        out.insert(id.clone(), free[i % free.len()].to_string());
    }
    out
}


/// Choose a theme, and remember the choice.
///
/// Stored on the tenant rather than in the browser, so signing in from another
/// machine shows the same room.
///
/// **Every place that changes the theme goes through here.** Only the first-run
/// picker used to write the choice down; the floor's own dropdown and the one
/// in setup moved the signal and nothing else, so changing the theme and
/// reloading put you silently back on whichever theme the picker had recorded.
pub fn select(theme: RwSignal<usize>, i: usize) {
    theme.set(i);
    leptos::task::spawn_local(async move {
        let id = builtin().get(i).map(|t| t.id.clone()).unwrap_or_default();
        if id.is_empty() {
            return;
        }
        // `themeChosen` too: picking one from a dropdown is every bit as much a
        // choice as picking one from the first-run dialog, and leaving it unset
        // would ask again on the next visit.
        let _ = crate::api::rpc(
            "config:update",
            serde_json::json!([{ "theme": id, "themeChosen": true }]),
        )
        .await;
    });
}

/// The built-in Office theme, as data.
///
/// Bundled rather than fetched so a fresh install has a floor before it has a
/// network. Additional themes load from the tenant's own theme directory,
/// which is why this is a plain string rather than a `match` somewhere.
pub fn builtin() -> Vec<Theme> {
    [
        include_str!("../themes/office.json"),
        include_str!("../themes/serenity.json"),
        include_str!("../themes/tos.json"),
        include_str!("../themes/tng.json"),
        include_str!("../themes/anh.json"),
    ]
    .iter()
    .filter_map(|s| match serde_json::from_str::<Theme>(s) {
        Ok(t) => Some(t),
        // A malformed theme is skipped rather than taking the app down, but it
        // is loud in the console: a silently missing theme is very hard to
        // diagnose from the picker.
        Err(e) => {
            leptos::logging::error!("theme failed to parse: {e}");
            None
        }
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the whole indirection: switching themes must not renumber
    /// the floor.
    #[test]
    fn archetype_assignment_is_stable_across_a_theme_switch() {
        let ids: Vec<String> = ["michael", "dwight", "jim"].iter().map(|s| s.to_string()).collect();
        let a = assign_in(&ids, Some("michael"), None, &HashMap::new());
        let b = assign_in(&ids, Some("michael"), None, &HashMap::new());
        assert_eq!(a, b);
    }

    /// An agent named after a member of the cast should BE that member. Without
    /// this, Pam came out dressed as Kevin — correct by ordering, wrong to look
    /// at.
    #[test]
    fn an_agent_named_after_a_character_gets_that_character() {
        let themes = builtin();
        let office = themes.iter().find(|t| t.id == "office").unwrap();
        let ids: Vec<String> = ["michael", "dwight", "jim", "pam", "ryan"]
            .iter().map(|s| s.to_string()).collect();

        let out = assign_in(&ids, Some("michael"), Some(office), &HashMap::new());
        let who = |id: &str| office.character(&out[id]).unwrap().display.clone();
        assert_eq!(who("pam"), "Pam", "an agent named Pam must be Pam");
        assert_eq!(who("michael"), "Michael");
        assert_eq!(who("dwight"), "Dwight");
        assert_eq!(who("jim"), "Jim");
        // Ryan is not in the cast, so he fills a remaining slot rather than
        // going undressed.
        assert!(!who("ryan").is_empty());

        // Nobody may be assigned twice, or two agents share one face.
        let mut slots: Vec<&String> = out.values().collect();
        let n = slots.len();
        slots.sort();
        slots.dedup();
        assert_eq!(slots.len(), n, "two agents share an archetype");
    }

    /// In a theme with no matching names, ordering is all there is — and the
    /// orchestrator still leads.
    #[test]
    fn a_theme_with_no_matching_names_falls_back_to_order() {
        let themes = builtin();
        let serenity = themes.iter().find(|t| t.id == "serenity").unwrap();
        let ids: Vec<String> = ["michael", "dwight", "jim"].iter().map(|s| s.to_string()).collect();
        let out = assign_in(&ids, Some("michael"), Some(serenity), &HashMap::new());
        assert_eq!(out["michael"], "leader");
        assert_eq!(serenity.character(&out["michael"]).unwrap().display, "Mal");
    }

    /// The orchestrator wears the leader's face. Without this it landed
    /// wherever the alphabet put it — on michael/dwight/jim the god came out
    /// dressed as Jim while Dwight wore Michael's suit.
    #[test]
    fn the_orchestrator_takes_the_leader_slot() {
        let ids: Vec<String> = ["michael", "dwight", "jim"].iter().map(|s| s.to_string()).collect();
        let out = assign_in(&ids, Some("michael"), None, &HashMap::new());
        assert_eq!(out["michael"], "leader");
        // The rest keep stable id order behind it.
        assert_eq!(out["dwight"], "second");
        assert_eq!(out["jim"], "operator");

        // With no god named, plain id order.
        let out = assign_in(&ids, None, None, &HashMap::new());
        assert_eq!(out["dwight"], "leader");
    }

    /// A small cast must still dress a large floor.
    #[test]
    fn more_agents_than_slots_wrap_rather_than_going_unassigned() {
        let ids: Vec<String> = (0..ARCHETYPES.len() + 3).map(|i| format!("a{i:02}")).collect();
        let out = assign_in(&ids, None, None, &HashMap::new());
        assert_eq!(out.len(), ids.len());
        assert!(out.values().all(|v| ARCHETYPES.contains(&v.as_str())));
        assert_eq!(out["a00"], out[&format!("a{:02}", ARCHETYPES.len())], "the roster wraps");
    }

    /// Both bundled themes must parse, fill every slot, and paint.
    #[test]
    fn the_bundled_themes_are_complete_and_paintable() {
        let themes = builtin();
        assert_eq!(themes.len(), 5, "every bundled theme parses");
        for t in themes {
            for a in ARCHETYPES {
                let c = t.character(a).unwrap_or_else(|| panic!("{} has no {a}", t.id));
                assert!(!c.display.is_empty());
                let cv = crate::pixel::portrait(&c.recipe);
                let painted = cv.buf.chunks(4).filter(|p| p[3] > 0).count();
                assert!(painted > 200, "{}/{a} painted {painted} pixels", t.id);
                // Flavour is what makes a theme a place rather than a palette.
                assert!(!c.personality.trait_line.is_empty(), "{}/{a} has no trait", t.id);
                assert!(!c.personality.idle.is_empty(), "{}/{a} says nothing when idle", t.id);
            }
            // A theme is a room too: every tool group needs somewhere to go, or
            // an agent doing that work has nowhere to walk.
            for kind in ["board", "shelf", "web", "terminal", "mailbox"] {
                assert!(
                    t.layout.stations.iter().any(|s| s.kind == kind),
                    "{} has no {kind} station", t.id
                );
            }
            let [x0, y0, x1, y1] = t.layout.roam;
            assert!(x1 > x0 && y1 > y0, "{} has an empty roam box", t.id);
            assert!(y0 >= t.layout.wall_depth, "{} lets agents roam into the wall", t.id);
        }
    }

    /// Every theme is a genuinely different cast. The Office theme would pass
    /// any validator written beside it, because the engine was shaped around
    /// it; the others are what prove the engine real.
    #[test]
    fn no_two_themes_share_a_character() {
        let themes = builtin();
        for a in ARCHETYPES {
            let mut seen: Vec<String> = themes
                .iter()
                .map(|t| t.character(a).unwrap().display.clone())
                .collect();
            let before = seen.len();
            seen.sort();
            seen.dedup();
            assert_eq!(seen.len(), before, "{a} is shared between themes: {seen:?}");
        }
    }

    /// Rooms differ too, or every theme is a palette swap of the same office.
    #[test]
    fn every_theme_has_its_own_room() {
        let themes = builtin();
        let mut floors: Vec<String> = themes.iter().map(|t| t.layout.floor.clone()).collect();
        let before = floors.len();
        floors.sort();
        floors.dedup();
        assert_eq!(floors.len(), before, "two themes share a floor colour");

        // Serenity is a ship, not an office: its stations are named for a ship.
        let ser = themes.iter().find(|t| t.id == "serenity").unwrap();
        let labels: Vec<&str> = ser.layout.stations.iter().map(|s| s.label.as_str()).collect();
        assert!(labels.contains(&"ENGINE") && labels.contains(&"GALLEY"), "{labels:?}");
    }

    /// A chosen personality is a decision, and a later hire must not undo it.
    /// Without the pin, hiring someone called `kaylee` would silently take the
    /// engineer's face off the agent whose operator asked for it.
    #[test]
    fn a_pinned_archetype_beats_a_name_match_and_the_ordering() {
        let themes = builtin();
        let serenity = themes.iter().find(|t| t.id == "serenity").unwrap();
        let ids: Vec<String> =
            ["ada", "kaylee", "zed"].iter().map(|s| s.to_string()).collect();

        let mut pins = HashMap::new();
        pins.insert("ada".to_string(), "engineer".to_string());
        let out = assign_in(&ids, None, Some(serenity), &pins);

        assert_eq!(out["ada"], "engineer", "the pin wins");
        assert_ne!(out["kaylee"], "engineer", "the name match yields to it");
        let mut slots: Vec<&String> = out.values().collect();
        let n = slots.len();
        slots.sort();
        slots.dedup();
        assert_eq!(slots.len(), n, "two agents share an archetype");
    }

    /// A pin to a slot someone else already holds is dropped rather than
    /// duplicated: two figures wearing one face is worse than a reassignment.
    #[test]
    fn two_agents_pinned_to_one_slot_do_not_share_a_face() {
        let ids: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let mut pins = HashMap::new();
        pins.insert("a".to_string(), "leader".to_string());
        pins.insert("b".to_string(), "leader".to_string());
        let out = assign_in(&ids, None, None, &pins);
        assert_ne!(out["a"], out["b"]);
    }

    /// Every map names its places, and every reference to one resolves.
    ///
    /// A dangling POI id is invisible: the character silently falls back to a
    /// desk and the theme looks merely badly laid out rather than broken.
    #[test]
    fn every_theme_names_its_places_and_every_reference_resolves() {
        for t in builtin() {
            assert!(t.layout.pois.len() >= 6, "{} names only {} places", t.id, t.layout.pois.len());

            let mut ids: Vec<&str> = t.layout.pois.iter().map(|p| p.id.as_str()).collect();
            let before = ids.len();
            ids.sort();
            ids.dedup();
            assert_eq!(ids.len(), before, "{} has two places with one id", t.id);

            let [x0, y0, x1, y1] = t.layout.roam;
            for p in &t.layout.pois {
                assert!(!p.label.is_empty(), "{}/{} has no label", t.id, p.id);
                // A post outside the room is a character standing in the void.
                assert!(
                    p.x >= 0.0 && p.x <= 320.0 - crate::pixel::SCENE_W as f64
                        && p.y >= t.layout.wall_depth
                        && p.y <= 176.0 - crate::pixel::SCENE_H as f64,
                    "{}/{} is off the map at ({}, {})", t.id, p.id, p.x, p.y
                );
                let _ = (x0, y0, x1, y1);
            }

            // Everyone has somewhere to be, and somewhere else to be found.
            for a in ARCHETYPES {
                let c = t.character(a).unwrap();
                for (which, id) in [
                    ("primary", &c.personality.primary_poi),
                    ("secondary", &c.personality.secondary_poi),
                ] {
                    assert!(!id.is_empty(), "{}/{a} has no {which} post", t.id);
                    assert!(t.poi(id).is_some(), "{}/{a} is posted to unknown {which} `{id}`", t.id);
                }
                assert_ne!(
                    c.personality.primary_poi, c.personality.secondary_poi,
                    "{}/{a} has one place, twice", t.id
                );
                assert!(t.post(a, false).is_some(), "{}/{a} has nowhere to work", t.id);
            }
        }
    }

    /// An agent carries its posts across a theme switch, and a Serenity POI id
    /// means nothing on the Office floor. Falling back to the character's own
    /// post is what keeps that from stranding anyone at (0, 0).
    #[test]
    fn a_post_from_another_theme_falls_back_to_the_character() {
        let themes = builtin();
        let office = themes.iter().find(|t| t.id == "office").unwrap();
        assert!(office.poi("engine-room").is_none());
        assert!(office.post("engineer", false).is_some());
    }

    /// A map with walls has to be a map you can walk. Every place anyone is
    /// ever sent must be on walkable ground and reachable from everywhere else
    /// — an unreachable room is a figure that stands still forever, which is
    /// much harder to spot than one taking a shortcut.
    #[test]
    fn every_place_on_a_walled_map_is_walkable_and_reachable() {
        use crate::nav::{to_feet, Nav};

        for t in builtin() {
            if t.layout.walk.is_empty() {
                // A room drawn side-on is one open floor, and needs none of
                // this: a straight line across an office is a walk.
                continue;
            }
            let nav = Nav::new(&t.layout.walk);

            // Every destination the floor can choose, in the sprite coordinates
            // it chooses them in.
            let mut spots: Vec<(String, [f64; 2])> = Vec::new();
            for p in &t.layout.pois {
                spots.push((format!("post {}", p.id), [p.x, p.y]));
            }
            for st in &t.layout.stations {
                spots.push((format!("station {}", st.label), st.spot()));
            }
            for d in &t.layout.doors {
                spots.push((format!("doorway {}", d.label), d.threshold));
            }

            for (what, at) in &spots {
                let feet = to_feet(*at);
                assert_eq!(
                    nav.snap(feet), feet,
                    "{}: {what} is not on walkable ground", t.id,
                );
            }

            // And you can get from any one of them to any other. Checked
            // against the first rather than every pair: reachability is
            // symmetric and transitive here, so one hub proves the graph is
            // one piece. `connected` rather than `route`, which answers "walk
            // there" and falls back to a straight line — asserting on the
            // route's last point would pass for a room nothing reaches.
            let (hub_name, hub) = &spots[0];
            for (what, at) in &spots[1..] {
                assert!(
                    nav.connected(to_feet(*hub), to_feet(*at)),
                    "{}: {what} is cut off from {hub_name}", t.id,
                );
            }
        }
    }

    /// Every map says where its tools are worked at.
    ///
    /// The fallback is a guess that happens to suit a console on a wall; a map
    /// that leans on it has not decided anything, and on a deck plan the guess
    /// puts somebody in the next room.
    #[test]
    fn every_map_names_the_spot_for_each_of_its_tools() {
        for t in builtin() {
            for st in &t.layout.stations {
                assert!(st.spot.is_some(), "{}: {} has no spot to work at", t.id, st.label);
                let [x, y] = st.spot();
                assert!(
                    x >= 0.0 && x <= 320.0 - crate::pixel::SCENE_W as f64
                        && y >= 0.0 && y <= 176.0 - crate::pixel::SCENE_H as f64,
                    "{}: {} is worked at ({x}, {y}), off the map", t.id, st.label
                );
            }
        }
    }

    /// Serenity is the map this exists for, so it says so out loud: a deck plan
    /// whose rooms were not declared walkable would silently fall back to
    /// straight lines through the bulkheads.
    #[test]
    fn the_ship_declares_its_walkable_space() {
        let themes = builtin();
        let ser = themes.iter().find(|t| t.id == "serenity").unwrap();
        assert!(
            ser.layout.walk.len() >= 10,
            "the ship has {} walkable boxes", ser.layout.walk.len()
        );
    }

    #[test]
    fn a_theme_missing_a_slot_still_dresses_everyone() {
        // r##..##, because a `#` in a hex colour would close a plain r#"..."#.
        let partial: Theme = serde_json::from_str(
            r##"{"id":"tiny","name":"Tiny",
                "layout":{"wall":"#000","floor":"#111","stations":[],"roam":[0,50,100,100]},
                "cast":{"leader":{"display":"Solo","recipe":
               {"skin":"light","hairc":[1,2,3],"hair":"styleShort","cloth":"suit","c1":[1,2,3]}}}}"##,
        )
        .unwrap();
        assert_eq!(partial.character("leader").unwrap().display, "Solo");
        assert_eq!(partial.character("counsel").unwrap().display, "Solo", "falls back rather than vanishing");
    }
}
