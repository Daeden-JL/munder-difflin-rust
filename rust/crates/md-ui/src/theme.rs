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
}

fn half() -> f64 {
    0.5
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
    /// Fill colour. Themes differ more in their furniture than their cast.
    #[serde(default)]
    pub color: Option<String>,
}

fn default_w() -> f64 {
    30.0
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
    /// Where agents wander when they are not at a desk: `[x0, y0, x1, y1]`.
    pub roam: [f64; 4],
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
pub fn assign_in(
    ids: &[String],
    god: Option<&str>,
    theme: Option<&Theme>,
) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    let mut taken: Vec<String> = Vec::new();

    if let Some(t) = theme {
        for id in ids {
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
        let a = assign_in(&ids, Some("michael"), None);
        let b = assign_in(&ids, Some("michael"), None);
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

        let out = assign_in(&ids, Some("michael"), Some(office));
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
        let out = assign_in(&ids, Some("michael"), Some(serenity));
        assert_eq!(out["michael"], "leader");
        assert_eq!(serenity.character(&out["michael"]).unwrap().display, "Mal");
    }

    /// The orchestrator wears the leader's face. Without this it landed
    /// wherever the alphabet put it — on michael/dwight/jim the god came out
    /// dressed as Jim while Dwight wore Michael's suit.
    #[test]
    fn the_orchestrator_takes_the_leader_slot() {
        let ids: Vec<String> = ["michael", "dwight", "jim"].iter().map(|s| s.to_string()).collect();
        let out = assign_in(&ids, Some("michael"), None);
        assert_eq!(out["michael"], "leader");
        // The rest keep stable id order behind it.
        assert_eq!(out["dwight"], "second");
        assert_eq!(out["jim"], "operator");

        // With no god named, plain id order.
        let out = assign_in(&ids, None, None);
        assert_eq!(out["dwight"], "leader");
    }

    /// A small cast must still dress a large floor.
    #[test]
    fn more_agents_than_slots_wrap_rather_than_going_unassigned() {
        let ids: Vec<String> = (0..ARCHETYPES.len() + 3).map(|i| format!("a{i:02}")).collect();
        let out = assign_in(&ids, None, None);
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
