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
    /// Desks or consoles agents idle at, as `[x, y, w, h]` in room pixels.
    #[serde(default)]
    pub furniture: Vec<[f64; 4]>,
    #[serde(default)]
    pub furniture_color: Option<String>,
    pub stations: Vec<Station>,
    /// Where agents wander when idle: `[x0, y0, x1, y1]`. Keeps them off the
    /// walls and out of the furniture.
    pub roam: [f64; 4],
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

/// Assign archetypes to agents.
///
/// **The orchestrator takes `leader`.** Everyone else follows in stable id
/// order. Without the first rule the god landed wherever the alphabet put it —
/// on a floor of michael/dwight/jim/pam/ryan, the orchestrator came out dressed
/// as Jim while Dwight wore Michael's suit.
///
/// The order for everyone else must be stable — id, not display name — or the
/// floor reshuffles whenever an agent is renamed or the roster re-sorted.
pub fn assign(ids: &[String], god: Option<&str>) -> HashMap<String, String> {
    let mut ordered: Vec<&String> = ids.iter().collect();
    ordered.sort();
    if let Some(g) = god {
        if let Some(i) = ordered.iter().position(|id| id.as_str() == g) {
            let leader = ordered.remove(i);
            ordered.insert(0, leader);
        }
    }
    ordered
        .into_iter()
        .enumerate()
        .map(|(i, id)| (id.clone(), ARCHETYPES[i % ARCHETYPES.len()].to_string()))
        .collect()
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
        let a = assign(&ids, Some("michael"));
        let b = assign(&ids, Some("michael"));
        assert_eq!(a, b);
    }

    /// The orchestrator wears the leader's face. Without this it landed
    /// wherever the alphabet put it — on michael/dwight/jim the god came out
    /// dressed as Jim while Dwight wore Michael's suit.
    #[test]
    fn the_orchestrator_takes_the_leader_slot() {
        let ids: Vec<String> = ["michael", "dwight", "jim"].iter().map(|s| s.to_string()).collect();
        let out = assign(&ids, Some("michael"));
        assert_eq!(out["michael"], "leader");
        // The rest keep stable id order behind it.
        assert_eq!(out["dwight"], "second");
        assert_eq!(out["jim"], "operator");

        // With no god named, plain id order.
        let out = assign(&ids, None);
        assert_eq!(out["dwight"], "leader");
    }

    /// A small cast must still dress a large floor.
    #[test]
    fn more_agents_than_slots_wrap_rather_than_going_unassigned() {
        let ids: Vec<String> = (0..ARCHETYPES.len() + 3).map(|i| format!("a{i:02}")).collect();
        let out = assign(&ids, None);
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
