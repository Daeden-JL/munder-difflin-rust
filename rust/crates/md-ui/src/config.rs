//! The configuration panel: agents, preferences, MCP consent, and accounts.
//!
//! Everything here writes through the same channels an agent or the CLI would
//! use, so the panel is a view of the real state rather than a second copy of
//! it that can drift.

use std::collections::HashMap;

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::api;
use crate::theme;

/// The two entries in the personality picker that are not characters.
const RANDOM: &str = "__random";
const CUSTOM: &str = "__custom";

/// Something to pick with. The floor's own PRNG is per-walker and seeded from
/// an agent id, which is exactly the wrong thing here — the agent does not
/// exist yet, and two hires in a row must not draw the same name.
fn spin() -> usize {
    window()
        .performance()
        .map(|p| p.now().to_bits() as usize)
        .unwrap_or(0)
}

#[component]
pub fn Config(
    theme_idx: RwSignal<usize>,
    /// Bumped when something here changes state the rest of the app reads.
    changed: RwSignal<u32>,
    /// agent id → archetype, so "adopt the cast's names" knows who is whom.
    archetypes: Signal<HashMap<String, String>>,
    is_admin: RwSignal<bool>,
) -> impl IntoView {
    let tab = RwSignal::new("agents".to_string());
    let status = RwSignal::new(String::new());

    view! {
        <div class="config">
            <div class="cfg-tabs">
                {["agents", "engines", "preferences", "tools", "accounts"].into_iter().map(|t| {
                    let is = move || tab.get() == t;
                    view! {
                        <button class="ghost" class:on=is on:click=move |_| tab.set(t.into())>
                            {t}
                        </button>
                    }
                }).collect::<Vec<_>>()}
                <span class="grow"></span>
                <span class="dim">{move || status.get()}</span>
            </div>

            <div class="cfg-body">
                <Show when=move || tab.get() == "agents">
                    <Agents changed status archetypes theme_idx/>
                </Show>
                <Show when=move || tab.get() == "engines">
                    <Engines status/>
                </Show>
                <Show when=move || tab.get() == "preferences">
                    <Preferences status theme_idx/>
                </Show>
                <Show when=move || tab.get() == "tools">
                    <Tools status/>
                </Show>
                <Show when=move || tab.get() == "accounts">
                    <Accounts status is_admin/>
                </Show>
            </div>
        </div>
    }
}

/// Stop an agent and start it again.
///
/// Two calls with a wait between them, because the pty manager refuses a
/// duplicate id and a killed session is only removed once its reader thread
/// notices the child is gone. Respawning immediately after the kill races that
/// and fails with "duplicate id" perhaps half the time — which reads to the
/// user as the button not working.
async fn restart(id: &str, opts: Value) -> Result<Value, String> {
    let live = |list: &Value| {
        list.as_array()
            .map(|a| a.iter().any(|s| s["id"].as_str() == Some(id)))
            .unwrap_or(false)
    };
    if live(&api::rpc("pty:list", json!([])).await.unwrap_or(Value::Null)) {
        let _ = api::rpc("pty:kill", json!([id])).await;
        for _ in 0..60 {
            gloo_timers::future::TimeoutFuture::new(100).await;
            if !live(&api::rpc("pty:list", json!([])).await.unwrap_or(Value::Null)) {
                break;
            }
        }
    }
    api::rpc("pty:spawn", opts).await
}

/// Spawn agents, and manage the ones on the floor.
#[component]
fn Agents(
    changed: RwSignal<u32>,
    status: RwSignal<String>,
    archetypes: Signal<HashMap<String, String>>,
    theme_idx: RwSignal<usize>,
) -> impl IntoView {
    let name = RwSignal::new(String::new());
    let role = RwSignal::new(String::new());
    let cwd = RwSignal::new("~/workspaces".to_string());
    // Which CLI this agent runs. A name from the catalogue, not a binary — the
    // hook wiring follows from the engine, and a typed-in command could never
    // say whether the thing it names reports to the floor.
    let engine = RwSignal::new("claude".to_string());
    // ...but a one-off binary should not need registering, so the command stays
    // overridable. Empty means "whatever the engine runs".
    let command = RwSignal::new(String::new());
    let is_god = RwSignal::new(false);
    let roster = RwSignal::new(Vec::<Value>::new());
    let engines = RwSignal::new(Vec::<Value>::new());

    Effect::new(move |_| {
        let _ = changed.get();
        leptos::task::spawn_local(async move {
            if let Ok(v) = api::get_json("/api/engines").await {
                engines.set(v["engines"].as_array().cloned().unwrap_or_default());
            }
        });
    });

    // What an engine runs, for the line under the picker.
    let engine_line = move |id: &str| -> String {
        engines
            .get()
            .iter()
            .find(|e| e["id"].as_str() == Some(id))
            .map(|e| {
                let cmd = e["command"].as_str().unwrap_or("");
                let args = e["args"].as_array().map(|a| {
                    a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" ")
                }).unwrap_or_default();
                let run = if args.is_empty() { cmd.to_string() } else { format!("{cmd} {args}") };
                let missing = if e["available"].as_bool() == Some(false) {
                    " — not installed on this machine"
                } else {
                    ""
                };
                let quiet = if e["hooks"].as_bool() == Some(true) {
                    ""
                } else {
                    " · quiet on the floor"
                };
                format!("runs `{run}`{quiet}{missing}")
            })
            .unwrap_or_default()
    };
    // Which of the theme's personalities the new agent gets: an archetype slot,
    // RANDOM, or CUSTOM. Random by default — most hires do not care who they
    // look like, and making them choose first is a toll on the common path.
    let personality = RwSignal::new(RANDOM.to_string());
    let persona = RwSignal::new(String::new());
    // Where they are posted, as POI ids. Empty means "wherever this character
    // usually works", which is what keeps the form honest across a theme
    // switch: a Serenity post id is meaningless on the Office floor.
    let primary = RwSignal::new(String::new());
    let secondary = RwSignal::new(String::new());

    let current = move || {
        let themes = theme::builtin();
        let i = theme_idx.get() % themes.len().max(1);
        themes.into_iter().nth(i)
    };
    // Only slots the theme actually fills. `character()` falls back, and a
    // picker built on it would offer the same face under three names.
    let cast_options = move || -> Vec<(String, String, String)> {
        let Some(t) = current() else { return Vec::new() };
        theme::ARCHETYPES
            .iter()
            .filter_map(|a| {
                t.cast.get(*a).map(|c| {
                    (a.to_string(), c.display.clone(), c.personality.trait_line.clone())
                })
            })
            .collect()
    };
    let poi_options = move || -> Vec<(String, String)> {
        current()
            .map(|t| t.layout.pois.iter().map(|p| (p.id.clone(), p.label.clone())).collect())
            .unwrap_or_default()
    };
    // Who is already on the floor, as slots. An archived agent does not hold
    // its personality: the whole point of archiving is to free the desk.
    let aboard = move || -> Vec<String> {
        let slots = archetypes.get();
        roster
            .get()
            .iter()
            .filter(|a| !a["archived"].as_bool().unwrap_or(false))
            .filter_map(|a| a["id"].as_str().and_then(|id| slots.get(id).cloned()))
            .collect()
    };

    // Picking a personality fills the form in with that character — their name,
    // how they behave, and where they work. All of it stays editable: the
    // choice is a starting point, not a lock.
    let choose = move |slot: String| {
        personality.set(slot.clone());
        let Some(t) = current() else { return };
        let Some(c) = t.cast.get(&slot) else {
            // Random and Custom both start from a blank sheet.
            name.set(String::new());
            persona.set(String::new());
            primary.set(String::new());
            secondary.set(String::new());
            return;
        };
        name.set(c.display.clone());
        persona.set(c.personality.trait_line.clone());
        primary.set(c.personality.primary_poi.clone());
        secondary.set(c.personality.secondary_poi.clone());
    };

    let reload = move || {
        leptos::task::spawn_local(async move {
            if let Ok(v) = api::rpc("hive:registry", json!([])).await {
                let mut list: Vec<Value> = v["agents"]
                    .as_object()
                    .map(|m| {
                        m.iter()
                            .map(|(id, a)| {
                                let mut e = a.clone();
                                e["id"] = json!(id);
                                e
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                list.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
                roster.set(list);
            }
        });
    };
    Effect::new(move |_| {
        let _ = changed.get();
        reload();
    });

    let hire = move |_| {
        let (mut n, r, c, cmd, god, eng) = (
            name.get_untracked(), role.get_untracked(), cwd.get_untracked(),
            command.get_untracked(), is_god.get_untracked(), engine.get_untracked(),
        );
        let (mut slot, mut line) = (String::new(), persona.get_untracked());
        let (mut post, mut post2) = (primary.get_untracked(), secondary.get_untracked());

        let choice = personality.get_untracked();
        if choice != CUSTOM {
            let Some(t) = current() else {
                status.set("no theme to hire into".into());
                return;
            };
            // Random draws from the personalities NOT already on the floor —
            // the point of it is to fill the cast out, so handing back a second
            // Kaylee would defeat it.
            slot = if choice == RANDOM {
                let used = aboard();
                let free: Vec<&str> = theme::ARCHETYPES
                    .iter()
                    .copied()
                    .filter(|a| t.cast.contains_key(*a) && !used.iter().any(|u| u == a))
                    .collect();
                if free.is_empty() {
                    status.set(
                        "every personality on this floor is already aboard — \
                         pick one to double up, or write a custom one"
                            .into(),
                    );
                    return;
                }
                free[spin() % free.len()].to_string()
            } else {
                choice
            };
            if let Some(ch) = t.cast.get(&slot) {
                if n.trim().is_empty() {
                    n = ch.display.clone();
                }
                if line.trim().is_empty() {
                    line = ch.personality.trait_line.clone();
                }
                if post.trim().is_empty() {
                    post = ch.personality.primary_poi.clone();
                }
                if post2.trim().is_empty() {
                    post2 = ch.personality.secondary_poi.clone();
                }
            }
        }

        if n.trim().is_empty() {
            status.set("an agent needs a name".into());
            return;
        }
        // The id is derived from the name and is what everything durable is
        // keyed to — memory, transcript, desk. It is deliberately not editable.
        let id: String = n.trim().to_lowercase().chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
            .collect();
        status.set("hiring…".into());
        leptos::task::spawn_local(async move {
            let opts = json!([{
                "id": id.trim_matches('-'),
                // Only when overridden: an empty string would beat the engine
                // and spawn a shell.
                "command": if cmd.trim().is_empty() { Value::Null } else { json!(cmd) },
                "cwd": c,
                "cols": 100, "rows": 30,
                "hive": {
                    "name": n, "role": r, "isGod": god, "engine": eng,
                    // The chosen personality and posting travel with the spawn
                    // and are stored on the registry entry, so they survive a
                    // restart. Assigning them by ordering on every read would
                    // re-roll somebody's choice the next time the floor changed.
                    "archetype": slot, "persona": line,
                    "primaryPoi": post, "secondaryPoi": post2,
                },
            }]);
            match api::rpc("pty:spawn", opts).await {
                Ok(v) if v["ok"] == true => {
                    status.set("hired".into());
                    // Back to a blank sheet. Leaving the picker on the
                    // character just hired means the next hire re-fills the
                    // same name, derives the same id, and collides.
                    name.set(String::new());
                    persona.set(String::new());
                    personality.set(RANDOM.to_string());
                    primary.set(String::new());
                    secondary.set(String::new());
                    changed.update(|n| *n = n.wrapping_add(1));
                }
                Ok(v) => status.set(v["error"].as_str().unwrap_or("could not hire").to_string()),
                Err(e) => status.set(e),
            }
        });
    };

    // Rename every agent to the character it is dressed as.
    //
    // A deliberate ACTION, not a side effect of switching themes: an agent's
    // name is its durable identity, and rewriting it while someone browses
    // themes would rename real records by accident.
    let adopt = move |_| {
        let slots = archetypes.get_untracked();
        let themes = theme::builtin();
        let Some(t) = themes.get(theme_idx.get_untracked() % themes.len().max(1)).cloned() else { return };
        status.set("recasting the floor…".into());
        leptos::task::spawn_local(async move {
            // One request for the whole floor, not one per agent: the server
            // holds every agent BEFORE renaming any, so no one is handed work
            // while the floor is half-renamed.
            let cast: Vec<Value> = slots
                .into_iter()
                .filter_map(|(id, arch)| {
                    let c = t.character(&arch)?;
                    Some(json!({
                        "id": id,
                        "name": c.display,
                        // Pinned, so the binding a recast produced stops
                        // depending on the ordering that produced it: hiring a
                        // tenth agent must not shuffle the nine already named.
                        "archetype": arch,
                        "primaryPoi": c.personality.primary_poi.clone(),
                        "secondaryPoi": c.personality.secondary_poi.clone(),
                        // The character becomes part of the agent's identity, so
                        // a recast changes how it behaves and not just its name.
                        // The trait alone — the name is already the agent's, and
                        // repeating it reads as "Mal Captain. Aims to misbehave."
                        "persona": c.personality.trait_line.clone(),
                    }))
                })
                .collect();
            match api::post_json("/api/recast", &json!(cast)).await {
                Ok(v) => {
                    let n = v["renamed"].as_u64().unwrap_or(0);
                    let note = v["note"].as_str().unwrap_or("");
                    status.set(format!("recast {n} agents as the {} cast. {note}", t.name));
                }
                Err(e) => status.set(e),
            }
            changed.update(|n| *n = n.wrapping_add(1));
        });
    };

    view! {
        <div class="cfg-cols">
            <section>
                <h3>"Hire an agent"</h3>

                <label>"personality"</label>
                <select on:change=move |e| choose(event_target_value(&e))>
                    // Rebuilt whenever the theme or the roster changes: the
                    // list is "who this floor could still be", and both of
                    // those move it.
                    {move || {
                        let used = aboard();
                        let picked = personality.get();
                        let mut opts = vec![view! {
                            <option value=RANDOM.to_string() selected=picked == RANDOM>
                                {"Random — anyone not already aboard".to_string()}
                            </option>
                        }];
                        opts.extend(cast_options().into_iter().map(|(slot, display, line)| {
                            // Saying who is already here is what makes Random
                            // legible, and stops a second Kaylee being a
                            // surprise rather than a decision.
                            let label = if used.contains(&slot) {
                                format!("{display} · {line} (aboard)")
                            } else {
                                format!("{display} · {line}")
                            };
                            let sel = picked == slot;
                            view! { <option value=slot selected=sel>{label}</option> }
                        }));
                        opts.push(view! {
                            <option value=CUSTOM.to_string() selected=picked == CUSTOM>
                                {"Custom — write your own".to_string()}
                            </option>
                        });
                        opts
                    }}
                </select>

                <label>"name"</label>
                <input prop:value=move || name.get() placeholder="Dwight"
                       on:input=move |e| name.set(event_target_value(&e))/>

                // Editable for a custom hire, and shown as written for a
                // character — a trait line you cannot see is a setting you
                // cannot check.
                <Show when=move || personality.get() == CUSTOM>
                    <label>"how they behave"</label>
                    <input prop:value=move || persona.get()
                           placeholder="Believes every rule is load-bearing."
                           on:input=move |e| persona.set(event_target_value(&e))/>
                </Show>
                <Show when=move || personality.get() != CUSTOM && !persona.get().is_empty()>
                    <p class="hint">{move || persona.get()}</p>
                </Show>

                <label>"where they work"</label>
                <select on:change=move |e| primary.set(event_target_value(&e))>
                    {move || {
                        let picked = primary.get();
                        let mut opts = vec![view! {
                            <option value=String::new() selected=picked.is_empty()>
                                {"their usual post".to_string()}
                            </option>
                        }];
                        opts.extend(poi_options().into_iter().map(|(id, label)| {
                            let sel = picked == id;
                            view! { <option value=id selected=sel>{label}</option> }
                        }));
                        opts
                    }}
                </select>

                <label>"and where else you\u{2019}ll find them"</label>
                <select on:change=move |e| secondary.set(event_target_value(&e))>
                    {move || {
                        let picked = secondary.get();
                        let mut opts = vec![view! {
                            <option value=String::new() selected=picked.is_empty()>
                                {"their usual haunt".to_string()}
                            </option>
                        }];
                        opts.extend(poi_options().into_iter().map(|(id, label)| {
                            let sel = picked == id;
                            view! { <option value=id selected=sel>{label}</option> }
                        }));
                        opts
                    }}
                </select>
                <p class="hint">
                    "An agent idles at its post and turns up at the other when it wanders.
                     Posts belong to the map, so switching themes moves everyone to the
                     matching place on the new one rather than stranding them."
                </p>

                <label>"role"</label>
                <input prop:value=move || role.get() placeholder="assistant to the regional manager"
                       on:input=move |e| role.set(event_target_value(&e))/>
                <label>"working directory"</label>
                <input prop:value=move || cwd.get()
                       on:input=move |e| cwd.set(event_target_value(&e))/>

                <label>"engine"</label>
                <select on:change=move |e| engine.set(event_target_value(&e))>
                    {move || {
                        let picked = engine.get();
                        engines.get().into_iter().map(|x| {
                            let id = x["id"].as_str().unwrap_or("").to_string();
                            let mut label = x["label"].as_str().unwrap_or(&id).to_string();
                            // An engine whose binary is missing is still
                            // offerable — you register it, then install it —
                            // but "hired and exited immediately" is a poor way
                            // to find that out.
                            if x["available"].as_bool() == Some(false) {
                                label.push_str(" (not installed)");
                            }
                            let sel = picked == id;
                            view! { <option value=id selected=sel>{label}</option> }
                        }).collect::<Vec<_>>()
                    }}
                </select>
                <p class="hint">{move || engine_line(&engine.get())}</p>

                <label>"command — leave empty to use the engine\u{2019}s"</label>
                <input prop:value=move || command.get()
                       placeholder=move || {
                           engines.get().iter()
                               .find(|e| e["id"].as_str() == Some(engine.get().as_str()))
                               .and_then(|e| e["command"].as_str().map(String::from))
                               .unwrap_or_default()
                       }
                       on:input=move |e| command.set(event_target_value(&e))/>
                <label class="check">
                    <input type="checkbox" prop:checked=move || is_god.get()
                           on:change=move |e| is_god.set(event_target_checked(&e))/>
                    "orchestrator — runs the floor and talks to you"
                </label>
                <button on:click=hire>"hire"</button>
            </section>

            <section>
                <h3>"On the floor"</h3>
                <For each=move || roster.get() key=|a| a["id"].as_str().unwrap_or("").to_string() let:a>
                    {
                        let id = a["id"].as_str().unwrap_or("").to_string();
                        let (i1, i2, i3) = (id.clone(), id.clone(), id.clone());
                        let held = a["onHold"].as_bool().unwrap_or(false);
                        let archived = a["archived"].as_bool().unwrap_or(false);
                        let mine = a.clone();
                        let now = a["provider"].as_str().unwrap_or("claude").to_string();
                        view! {
                            <div class="row">
                                <b>{a["name"].as_str().unwrap_or(&id).to_string()}</b>
                                <span class="dim">{a["role"].as_str().unwrap_or("").to_string()}</span>
                                <span class="grow"></span>
                                // Changing an agent's engine IS restarting it:
                                // the command is the process, so there is no
                                // version of this that takes effect without
                                // one. Said in the hint below rather than in a
                                // dialog nobody reads.
                                <select on:change=move |ev| {
                                    let (a, next) = (mine.clone(), event_target_value(&ev));
                                    let id = a["id"].as_str().unwrap_or("").to_string();
                                    status.set(format!("restarting {id} on {next}…"));
                                    leptos::task::spawn_local(async move {
                                        let opts = json!([{
                                            "id": id,
                                            "cwd": a["cwd"].as_str().unwrap_or("~"),
                                            "cols": 100, "rows": 30,
                                            // Resumed, so switching engines
                                            // does not throw away the thread an
                                            // agent was in the middle of.
                                            "resume": true,
                                            "hive": {
                                                "name": a["name"].as_str().unwrap_or(""),
                                                "role": a["role"].as_str().unwrap_or(""),
                                                "isGod": a["isGod"].as_bool().unwrap_or(false),
                                                "engine": next,
                                                "archetype": a["archetype"].as_str().unwrap_or(""),
                                                "primaryPoi": a["primaryPoi"].as_str().unwrap_or(""),
                                                "secondaryPoi": a["secondaryPoi"].as_str().unwrap_or(""),
                                            },
                                        }]);
                                        match restart(a["id"].as_str().unwrap_or(""), opts).await {
                                            Ok(v) if v["ok"] == true => status.set("restarted".into()),
                                            Ok(v) => status.set(
                                                v["error"].as_str().unwrap_or("could not restart").to_string()),
                                            Err(e) => status.set(e),
                                        }
                                        changed.update(|n| *n = n.wrapping_add(1));
                                    });
                                }>
                                    {move || {
                                        let now = now.clone();
                                        engines.get().into_iter().map(|x| {
                                            let eid = x["id"].as_str().unwrap_or("").to_string();
                                            let label = x["label"].as_str().unwrap_or(&eid).to_string();
                                            let sel = now == eid;
                                            view! { <option value=eid selected=sel>{label}</option> }
                                        }).collect::<Vec<_>>()
                                    }}
                                </select>
                                <button class="ghost" on:click=move |_| {
                                    let id = i1.clone();
                                    leptos::task::spawn_local(async move {
                                        let _ = api::rpc("hive:setAgentHold", json!([id, !held])).await;
                                        changed.update(|n| *n = n.wrapping_add(1));
                                    });
                                }>{if held { "resume" } else { "hold" }}</button>
                                <button class="ghost" on:click=move |_| {
                                    let id = i2.clone();
                                    leptos::task::spawn_local(async move {
                                        let _ = api::rpc("hive:setArchived", json!([id, !archived])).await;
                                        changed.update(|n| *n = n.wrapping_add(1));
                                    });
                                }>{if archived { "restore" } else { "archive" }}</button>
                                <button class="ghost danger" on:click=move |_| {
                                    let id = i3.clone();
                                    leptos::task::spawn_local(async move {
                                        let _ = api::rpc("pty:kill", json!([id])).await;
                                        changed.update(|n| *n = n.wrapping_add(1));
                                    });
                                }>"stop"</button>
                            </div>
                        }
                    }
                </For>
                <p class="hint">
                    "An agent\u{2019}s engine is the process it runs, so changing it stops and
                     starts that agent. Its session is resumed where the engine can, and its
                     memory, transcript and desk are untouched either way."
                </p>
                <p class="hint">
                    "Switching themes changes the costume only. Recasting is the real
                     thing: every agent is held, renamed, and its identity rewritten with
                     its new character — then released so its work resumes. Memory,
                     transcripts and desks are untouched."
                </p>
                <button on:click=adopt>"Recast this floor"</button>
            </section>
        </div>
    }
}

/// The agent CLIs this floor can hire.
///
/// A catalogue rather than a text box, because "which command" is not the only
/// question — whether the CLI speaks Claude Code's hook protocol decides whether
/// its agent narrates itself on the floor or works in silence, and no binary
/// name can answer that.
#[component]
fn Engines(status: RwSignal<String>) -> impl IntoView {
    let list = RwSignal::new(Vec::<Value>::new());
    // The tenant's own overrides, kept raw so a save can send the whole map.
    let raw = RwSignal::new(serde_json::Map::new());

    let load = move || {
        leptos::task::spawn_local(async move {
            if let Ok(v) = api::get_json("/api/engines").await {
                list.set(v["engines"].as_array().cloned().unwrap_or_default());
            }
            if let Ok(cfg) = api::rpc("config:get", json!([])).await {
                raw.set(cfg["engines"].as_object().cloned().unwrap_or_default());
            }
        });
    };
    Effect::new(move |_| load());

    // `config:update` merges only at the TOP level, so sending part of the
    // engines map would delete the rest of it. Every save sends all of it.
    let write = move |next: serde_json::Map<String, Value>| {
        status.set("saving…".into());
        leptos::task::spawn_local(async move {
            match api::rpc("config:update", json!([{ "engines": Value::Object(next) }])).await {
                Ok(_) => status.set("saved".into()),
                Err(e) => status.set(e),
            }
            load();
        });
    };
    let patch = move |id: String, fields: Vec<(&'static str, Value)>| {
        let mut map = raw.get_untracked();
        let entry = map.entry(id).or_insert_with(|| json!({}));
        if let Some(o) = entry.as_object_mut() {
            for (k, v) in fields {
                if v.is_null() {
                    o.remove(k);
                } else {
                    o.insert(k.into(), v);
                }
            }
        }
        write(map);
    };
    let forget = move |id: String| {
        let mut map = raw.get_untracked();
        map.remove(&id);
        write(map);
    };

    let (nid, nlabel, ncmd, nargs, nhooks) = (
        RwSignal::new(String::new()), RwSignal::new(String::new()),
        RwSignal::new(String::new()), RwSignal::new(String::new()),
        RwSignal::new(false),
    );

    let register = move |_| {
        let id: String = nid.get_untracked().trim().to_lowercase().chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let id = id.trim_matches('-').to_string();
        let cmd = ncmd.get_untracked();
        if id.is_empty() || cmd.trim().is_empty() {
            status.set("an engine needs a name and a command".into());
            return;
        }
        let mut map = raw.get_untracked();
        map.insert(id, json!({
            "label": if nlabel.get_untracked().trim().is_empty() {
                json!(Value::Null)
            } else {
                json!(nlabel.get_untracked())
            },
            "command": cmd,
            "args": args_of(&nargs.get_untracked()),
            "hooks": nhooks.get_untracked(),
        }));
        nid.set(String::new());
        nlabel.set(String::new());
        ncmd.set(String::new());
        nargs.set(String::new());
        nhooks.set(false);
        write(map);
    };

    view! {
        <div class="cfg-cols">
            <section>
                <h3>"Register an engine"</h3>
                <label>"name"</label>
                <input prop:value=move || nid.get() placeholder="my-agent"
                       on:input=move |e| nid.set(event_target_value(&e))/>
                <label>"shown as"</label>
                <input prop:value=move || nlabel.get() placeholder="My Agent"
                       on:input=move |e| nlabel.set(event_target_value(&e))/>
                <label>"command"</label>
                <input prop:value=move || ncmd.get() placeholder="my-agent"
                       on:input=move |e| ncmd.set(event_target_value(&e))/>
                <label>"arguments"</label>
                <input prop:value=move || nargs.get() placeholder="--repl --model fast"
                       on:input=move |e| nargs.set(event_target_value(&e))/>
                <label class="check">
                    <input type="checkbox" prop:checked=move || nhooks.get()
                           on:change=move |e| nhooks.set(event_target_checked(&e))/>
                    "speaks Claude Code\u{2019}s hook protocol"
                </label>
                <button on:click=register>"register"</button>
                <p class="hint">
                    "Tick the hook box only if the CLI really does accept Claude Code\u{2019}s
                     --settings and report its tool calls. An engine that claims hooks it
                     does not have puts an agent on the floor that looks alive and never
                     says anything."
                </p>
            </section>

            <section>
                <h3>"Engines"</h3>
                <For each=move || list.get() key=|e| e["id"].as_str().unwrap_or("").to_string() let:e>
                    {
                        let id = e["id"].as_str().unwrap_or("").to_string();
                        let (i1, i2) = (id.clone(), id.clone());
                        let builtin = e["builtin"].as_bool().unwrap_or(false);
                        let hooks = e["hooks"].as_bool().unwrap_or(false);
                        let ok = e["available"].as_bool().unwrap_or(false);
                        let cmd = RwSignal::new(e["command"].as_str().unwrap_or("").to_string());
                        let argv = RwSignal::new(
                            e["args"].as_array().map(|a| a.iter()
                                .filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "))
                                .unwrap_or_default());
                        view! {
                            <div class="row tool">
                                <b>{e["label"].as_str().unwrap_or(&id).to_string()}</b>
                                <span class=if hooks { "tag" } else { "tag warn" }>
                                    {if hooks { "reports" } else { "quiet" }}
                                </span>
                                <span class=if ok { "tag" } else { "tag warn" }>
                                    {if ok { "installed" } else { "missing" }}
                                </span>
                                <span class="grow"></span>
                                <input prop:value=move || cmd.get()
                                       on:input=move |ev| cmd.set(event_target_value(&ev))/>
                                <input prop:value=move || argv.get() placeholder="arguments"
                                       on:input=move |ev| argv.set(event_target_value(&ev))/>
                                <button class="ghost" on:click=move |_| {
                                    patch(i1.clone(), vec![
                                        ("command", json!(cmd.get_untracked())),
                                        ("args", json!(args_of(&argv.get_untracked()))),
                                    ]);
                                }>"save"</button>
                                // A built-in is hidden rather than deleted: its
                                // definition comes back on the next release, so
                                // a "delete" would silently undo itself.
                                <button class="ghost danger" on:click=move |_| {
                                    if builtin {
                                        patch(i2.clone(), vec![("hidden", json!(true))]);
                                    } else {
                                        forget(i2.clone());
                                    }
                                }>{if builtin { "hide" } else { "remove" }}</button>
                            </div>
                        }
                    }
                </For>
                <p class="hint">
                    "\u{201C}Reports\u{201D} means the CLI tells the floor what it is doing, so its
                     agent walks to the shelves when it reads and to the terminal when it
                     runs something. A quiet engine still works and still takes mail typed
                     into its prompt \u{2014} it just stands still."
                </p>
                <Show when=move || raw.get().values().any(|v| v["hidden"] == true)>
                    <button class="ghost" on:click=move |_| {
                        let mut map = raw.get_untracked();
                        map.retain(|_, v| v["hidden"] != true || {
                            v.as_object_mut().map(|o| o.remove("hidden"));
                            !v.as_object().map(|o| o.is_empty()).unwrap_or(true)
                        });
                        write(map);
                    }>"show hidden engines"</button>
                </Show>
            </section>
        </div>
    }
}

/// A command line as a list of arguments. Whitespace-split, deliberately: an
/// engine that needs a quoted argument with a space in it should be a script,
/// not a parsing problem here.
fn args_of(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

#[component]
fn Preferences(status: RwSignal<String>, theme_idx: RwSignal<usize>) -> impl IntoView {
    let cfg = RwSignal::new(json!({}));
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Ok(v) = api::rpc("config:get", json!([])).await {
                cfg.set(v);
            }
        });
    });

    let patch = move |key: &'static str, value: Value| {
        status.set("saving…".into());
        leptos::task::spawn_local(async move {
            match api::rpc("config:update", json!([{ key: value }])).await {
                Ok(v) => {
                    cfg.set(v);
                    status.set("saved".into());
                }
                Err(e) => status.set(e),
            }
        });
    };

    let names: Vec<String> = theme::builtin().iter().map(|t| t.name.clone()).collect();

    view! {
        <div class="cfg-cols">
            <section>
                <h3>"Appearance"</h3>
                <label>"theme"</label>
                <select on:change=move |e| {
                    theme::select(theme_idx, event_target_value(&e).parse().unwrap_or(0))
                }>
                    {names.into_iter().enumerate().map(|(i, n)| view! {
                        <option value=i.to_string() selected=move || theme_idx.get() == i>{n}</option>
                    }).collect::<Vec<_>>()}
                </select>
                <p class="hint">"Themes change the cast and the room, not who your agents are."</p>
            </section>

            <section>
                <h3>"Behaviour"</h3>
                <label class="check">
                    <input type="checkbox"
                           prop:checked=move || cfg.get()["autoMode"].as_bool().unwrap_or(false)
                           on:change=move |e| patch("autoMode", json!(event_target_checked(&e)))/>
                    "auto mode — agents act without asking permission each time"
                </label>
                <label class="check">
                    <input type="checkbox"
                           prop:checked=move || cfg.get()["notifications"].as_bool().unwrap_or(false)
                           on:change=move |e| patch("notifications", json!(event_target_checked(&e)))/>
                    "notify me when an agent needs an answer"
                </label>
                <label>"default token cap per worker (0 for none)"</label>
                <input type="number"
                       prop:value=move || cfg.get()["defaultWorkerTokenCap"].as_i64().unwrap_or(0).to_string()
                       on:change=move |e| {
                           patch("defaultWorkerTokenCap", json!(event_target_value(&e).parse::<i64>().unwrap_or(0)))
                       }/>
            </section>
        </div>
    }
}

/// The tools agents can reach, the consent that arms them, and registering new
/// ones.
#[component]
fn Tools(status: RwSignal<String>) -> impl IntoView {
    let list = RwSignal::new(Vec::<Value>::new());
    // The stored consent and registrations, so a change can send them back
    // whole.
    let raw = RwSignal::new(serde_json::Map::new());
    let load = move || {
        leptos::task::spawn_local(async move {
            if let Ok(v) = api::get_json("/api/mcp").await {
                list.set(v["catalog"].as_array().cloned().unwrap_or_default());
            }
            if let Ok(cfg) = api::rpc("config:get", json!([])).await {
                raw.set(cfg["mcpDefaults"].as_object().cloned().unwrap_or_default());
            }
        });
    };
    Effect::new(move |_| load());

    // `config:update` merges at the TOP level only, so sending one entry
    // replaced `mcpDefaults` wholesale and every other server silently reverted
    // to its default. Every change sends the whole map.
    let write = move |next: serde_json::Map<String, Value>| {
        status.set("saving…".into());
        leptos::task::spawn_local(async move {
            match api::rpc("config:update", json!([{ "mcpDefaults": Value::Object(next) }])).await {
                Ok(_) => status.set("saved".into()),
                Err(e) => status.set(e),
            }
            load();
        });
    };
    let set_fields = move |id: String, fields: Vec<(&'static str, Value)>| {
        let mut map = raw.get_untracked();
        let entry = map.entry(id).or_insert_with(|| json!({}));
        if let Some(o) = entry.as_object_mut() {
            for (k, v) in fields {
                o.insert(k.into(), v);
            }
        }
        write(map);
    };
    let forget = move |id: String| {
        let mut map = raw.get_untracked();
        map.remove(&id);
        write(map);
    };

    let (nid, nlabel, ncmd, nargs, nsafe) = (
        RwSignal::new(String::new()), RwSignal::new(String::new()),
        RwSignal::new(String::new()), RwSignal::new(String::new()),
        RwSignal::new(false),
    );
    let register = move |_| {
        let id: String = nid.get_untracked().trim().to_lowercase().chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let id = id.trim_matches('-').to_string();
        let cmd = ncmd.get_untracked();
        if id.is_empty() || cmd.trim().is_empty() {
            status.set("a tool needs a name and a command".into());
            return;
        }
        let mut map = raw.get_untracked();
        map.insert(id, json!({
            "label": nlabel.get_untracked(),
            "command": cmd,
            "args": args_of(&nargs.get_untracked()),
            // Declaring it safe is what arms it. Anything else waits for the
            // switch, which is the same rule the bundled servers follow.
            "tier": if nsafe.get_untracked() { "safe-readonly" } else { "write" },
        }));
        nid.set(String::new());
        nlabel.set(String::new());
        ncmd.set(String::new());
        nargs.set(String::new());
        nsafe.set(false);
        write(map);
    };

    view! {
        <div class="cfg-cols">
            <section>
                <h3>"Add a tool"</h3>
                <p class="hint">
                    "A tool is an MCP server: a command this floor runs and hands to your
                     agents."
                </p>
                <label>"name"</label>
                <input prop:value=move || nid.get() placeholder="my-tool"
                       on:input=move |e| nid.set(event_target_value(&e))/>
                <label>"shown as"</label>
                <input prop:value=move || nlabel.get() placeholder="My Tool"
                       on:input=move |e| nlabel.set(event_target_value(&e))/>
                <label>"command"</label>
                <input prop:value=move || ncmd.get() placeholder="npx"
                       on:input=move |e| ncmd.set(event_target_value(&e))/>
                <label>"arguments"</label>
                <input prop:value=move || nargs.get() placeholder="-y @scope/server"
                       on:input=move |e| nargs.set(event_target_value(&e))/>
                <label class="check">
                    <input type="checkbox" prop:checked=move || nsafe.get()
                           on:change=move |e| nsafe.set(event_target_checked(&e))/>
                    "read-only — no writes outside the workspace, no credentials"
                </label>
                <button on:click=register>"add"</button>
                <p class="hint">
                    "Ticking read-only turns it on straight away. Leave it clear and the
                     tool is added switched off, waiting for you — which is what anything
                     that writes beyond the workspace or holds a credential should be."
                </p>
            </section>

            <section>
                <h3>"Tools"</h3>
                <p class="hint">
                    "Read-only servers are on by default. Anything that writes outside the
                     workspace or needs a credential stays off until you turn it on here —
                     a default can never arm one, and neither can an agent."
                </p>
                <For each=move || list.get() key=|e| e["id"].as_str().unwrap_or("").to_string() let:e>
                    {
                        let id = e["id"].as_str().unwrap_or("").to_string();
                        let (i1, i2) = (id.clone(), id.clone());
                        let on = e["enabled"].as_bool().unwrap_or(false);
                        let tier = e["tier"].as_str().unwrap_or("").to_string();
                        let builtin = e["builtin"].as_bool().unwrap_or(false);
                        let asked_by = e["proposedBy"].as_str().map(String::from);
                        let run = {
                            let args = e["args"].as_array().map(|a| a.iter()
                                .filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "))
                                .unwrap_or_default();
                            let cmd = e["command"].as_str().unwrap_or("");
                            if args.is_empty() { cmd.to_string() } else { format!("{cmd} {args}") }
                        };
                        view! {
                            <div class="row tool">
                                <label class="check">
                                    <input type="checkbox" prop:checked=on on:change=move |ev| {
                                        set_fields(i1.clone(),
                                            vec![("enabled", json!(event_target_checked(&ev)))]);
                                    }/>
                                    <b>{e["label"].as_str().unwrap_or(&id).to_string()}</b>
                                </label>
                                <span class=move || if tier == "safe-readonly" { "tag" } else { "tag warn" }>
                                    {e["tier"].as_str().unwrap_or("").to_string()}
                                </span>
                                // Whose idea this was. An operator arming it
                                // should know they are approving a request
                                // rather than revisiting their own decision.
                                <Show when={let a = asked_by.clone(); move || a.is_some()}>
                                    <span class="tag warn">
                                        {format!("asked for by {}", asked_by.clone().unwrap_or_default())}
                                    </span>
                                </Show>
                                <span class="dim">{e["description"].as_str().unwrap_or("").to_string()}</span>
                                <span class="grow"></span>
                                <span class="dim">{run}</span>
                                <Show when=move || !builtin>
                                    <button class="ghost danger" on:click={
                                        let i2 = i2.clone();
                                        move |_| forget(i2.clone())
                                    }>"remove"</button>
                                </Show>
                            </div>
                        }
                    }
                </For>
            </section>
        </div>
    }
}

/// Accounts and roles. Visible only to admins — the server enforces it too.
#[component]
fn Accounts(status: RwSignal<String>, is_admin: RwSignal<bool>) -> impl IntoView {
    let list = RwSignal::new(Vec::<Value>::new());
    let user = RwSignal::new(String::new());
    let tenant = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let admin = RwSignal::new(false);

    let load = move || {
        leptos::task::spawn_local(async move {
            if let Ok(v) = api::get_json("/api/accounts").await {
                list.set(v["accounts"].as_array().cloned().unwrap_or_default());
            }
        });
    };
    Effect::new(move |_| {
        if is_admin.get() {
            load();
        }
    });

    let create = move |_| {
        let (u, t, p, a) = (
            user.get_untracked(), tenant.get_untracked(),
            password.get_untracked(), admin.get_untracked(),
        );
        status.set("creating…".into());
        leptos::task::spawn_local(async move {
            let body = json!({
                "user": u, "tenant": t, "password": p,
                "role": if a { "admin" } else { "member" },
            });
            match api::post_json("/api/accounts", &body).await {
                Ok(v) if v["ok"] == true => {
                    status.set("created".into());
                    user.set(String::new());
                    password.set(String::new());
                    load();
                }
                Ok(v) => status.set(v["error"].as_str().unwrap_or("failed").to_string()),
                Err(e) => status.set(e),
            }
        });
    };

    view! {
        <Show when=move || is_admin.get() fallback=|| view! {
            <section><p class="hint">"Only an admin can manage accounts."</p></section>
        }>
            <div class="cfg-cols">
                <section>
                    <h3>"Add an account"</h3>
                    <label>"username"</label>
                    <input prop:value=move || user.get()
                           on:input=move |e| user.set(event_target_value(&e))/>
                    <label>"tenant"</label>
                    <input prop:value=move || tenant.get() placeholder="their own floor"
                           on:input=move |e| tenant.set(event_target_value(&e))/>
                    <label>"password"</label>
                    <input type="password" prop:value=move || password.get()
                           on:input=move |e| password.set(event_target_value(&e))/>
                    <label class="check">
                        <input type="checkbox" prop:checked=move || admin.get()
                               on:change=move |e| admin.set(event_target_checked(&e))/>
                        "admin — can manage accounts"
                    </label>
                    <button on:click=create>"create"</button>
                    <p class="hint">
                        "A tenant is a separate floor: its own agents, files and memory.
                         Giving someone an existing tenant shares that floor with them."
                    </p>
                </section>

                <section>
                    <h3>"Accounts"</h3>
                    <For each=move || list.get() key=|a| a["user"].as_str().unwrap_or("").to_string() let:a>
                        {
                            let u = a["user"].as_str().unwrap_or("").to_string();
                            let (u1, u2) = (u.clone(), u.clone());
                            let is_admin_row = a["role"].as_str() == Some("admin");
                            let disabled = a["disabled"].as_bool().unwrap_or(false);
                            view! {
                                <div class="row" class:off=disabled>
                                    <b>{u.clone()}</b>
                                    <span class="tag">{a["tenant"].as_str().unwrap_or("").to_string()}</span>
                                    <span class=if is_admin_row { "tag warn" } else { "tag" }>
                                        {a["role"].as_str().unwrap_or("").to_string()}
                                    </span>
                                    <span class="grow"></span>
                                    <button class="ghost" on:click=move |_| {
                                        let u = u1.clone();
                                        leptos::task::spawn_local(async move {
                                            let role = if is_admin_row { "member" } else { "admin" };
                                            let v = api::post_json(&format!("/api/accounts/{u}"),
                                                &json!({ "role": role })).await;
                                            if let Ok(v) = v {
                                                if v["ok"] != true {
                                                    status.set(v["error"].as_str().unwrap_or("failed").to_string());
                                                }
                                            }
                                            load();
                                        });
                                    }>{if is_admin_row { "make member" } else { "make admin" }}</button>
                                    <button class="ghost danger" on:click=move |_| {
                                        let u = u2.clone();
                                        leptos::task::spawn_local(async move {
                                            let v = api::post_json(&format!("/api/accounts/{u}"),
                                                &json!({ "disabled": !disabled })).await;
                                            if let Ok(v) = v {
                                                if v["ok"] != true {
                                                    status.set(v["error"].as_str().unwrap_or("failed").to_string());
                                                }
                                            }
                                            load();
                                        });
                                    }>{if disabled { "enable" } else { "disable" }}</button>
                                </div>
                            }
                        }
                    </For>
                </section>
            </div>
        </Show>
    }
}
