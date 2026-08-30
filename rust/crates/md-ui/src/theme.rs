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
    /// Shown on the desk. The agent's own name still wins where one is set —
    /// this is who they are dressed as, not what they are called.
    pub display: String,
    pub recipe: Recipe,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Theme {
    pub id: String,
    pub name: String,
    /// Floor colours, so a theme can change the room as well as the cast.
    #[serde(default)]
    pub floor: Option<String>,
    #[serde(default)]
    pub wall: Option<String>,
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

/// Assign archetypes to agents by arrival order.
///
/// `ids` must be in a stable order — creation order, not display order — or the
/// floor reshuffles whenever the roster is re-sorted.
pub fn assign(ids: &[String]) -> HashMap<String, String> {
    ids.iter()
        .enumerate()
        .map(|(i, id)| (id.clone(), ARCHETYPES[i % ARCHETYPES.len()].to_string()))
        .collect()
}

/// The built-in Office theme, as data.
///
/// Bundled rather than fetched so a fresh install has a floor before it has a
/// network. Additional themes load from the tenant's own theme directory,
/// which is why this is a plain string rather than a `match` somewhere.
pub const OFFICE_JSON: &str = include_str!("../themes/office.json");
pub const SERENITY_JSON: &str = include_str!("../themes/serenity.json");

pub fn builtin() -> Vec<Theme> {
    [OFFICE_JSON, SERENITY_JSON]
        .iter()
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the whole indirection: switching themes must not renumber
    /// the floor.
    #[test]
    fn archetype_assignment_is_stable_across_a_theme_switch() {
        let ids: Vec<String> = ["michael-1", "dwight-2", "jim-3"].iter().map(|s| s.to_string()).collect();
        let a = assign(&ids);
        let b = assign(&ids);
        assert_eq!(a, b);
        assert_eq!(a["michael-1"], "leader");
        assert_eq!(a["dwight-2"], "second");
        assert_eq!(a["jim-3"], "operator");
    }

    /// A small cast must still dress a large floor.
    #[test]
    fn more_agents_than_slots_wrap_rather_than_going_unassigned() {
        let ids: Vec<String> = (0..ARCHETYPES.len() + 3).map(|i| format!("a{i}")).collect();
        let out = assign(&ids);
        assert_eq!(out.len(), ids.len());
        assert!(out.values().all(|v| ARCHETYPES.contains(&v.as_str())));
        assert_eq!(out["a0"], out[&format!("a{}", ARCHETYPES.len())], "the roster wraps");
    }

    /// Both bundled themes must parse, fill every slot, and paint.
    #[test]
    fn the_bundled_themes_are_complete_and_paintable() {
        let themes = builtin();
        assert_eq!(themes.len(), 2, "office and serenity both parse");
        for t in themes {
            for a in ARCHETYPES {
                let c = t.character(a).unwrap_or_else(|| panic!("{} has no {a}", t.id));
                assert!(!c.display.is_empty());
                let cv = crate::pixel::portrait(&c.recipe);
                let painted = cv.buf.chunks(4).filter(|p| p[3] > 0).count();
                assert!(painted > 200, "{}/{a} painted {painted} pixels", t.id);
            }
        }
    }

    /// Serenity is the proof the engine is real: the Office theme would pass any
    /// validator written beside it, because the engine was shaped around it.
    #[test]
    fn the_two_themes_are_actually_different_casts() {
        let themes = builtin();
        let office = themes.iter().find(|t| t.id == "office").unwrap();
        let serenity = themes.iter().find(|t| t.id == "serenity").unwrap();
        for a in ARCHETYPES {
            let (o, s) = (office.character(a).unwrap(), serenity.character(a).unwrap());
            assert_ne!(o.display, s.display, "{a} is the same person in both themes");
        }
    }

    #[test]
    fn a_theme_missing_a_slot_still_dresses_everyone() {
        let partial: Theme = serde_json::from_str(
            r#"{"id":"tiny","name":"Tiny","cast":{"leader":{"display":"Solo","recipe":
               {"skin":"light","hairc":[1,2,3],"hair":"styleShort","cloth":"suit","c1":[1,2,3]}}}}"#,
        )
        .unwrap();
        assert_eq!(partial.character("leader").unwrap().display, "Solo");
        assert_eq!(partial.character("counsel").unwrap().display, "Solo", "falls back rather than vanishing");
    }
}
