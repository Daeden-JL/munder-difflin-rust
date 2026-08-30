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
                {["agents", "preferences", "tools", "accounts"].into_iter().map(|t| {
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
    let command = RwSignal::new("claude".to_string());
    let is_god = RwSignal::new(false);
    let roster = RwSignal::new(Vec::<Value>::new());

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
        let (n, r, c, cmd, god) = (
            name.get_untracked(), role.get_untracked(), cwd.get_untracked(),
            command.get_untracked(), is_god.get_untracked(),
        );
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
                "command": cmd,
                "cwd": c,
                "cols": 100, "rows": 30,
                "hive": { "name": n, "role": r, "isGod": god },
            }]);
            match api::rpc("pty:spawn", opts).await {
                Ok(v) if v["ok"] == true => {
                    status.set("hired".into());
                    name.set(String::new());
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
                <label>"name"</label>
                <input prop:value=move || name.get() placeholder="Dwight"
                       on:input=move |e| name.set(event_target_value(&e))/>
                <label>"role"</label>
                <input prop:value=move || role.get() placeholder="assistant to the regional manager"
                       on:input=move |e| role.set(event_target_value(&e))/>
                <label>"working directory"</label>
                <input prop:value=move || cwd.get()
                       on:input=move |e| cwd.set(event_target_value(&e))/>
                <label>"command"</label>
                <input prop:value=move || command.get()
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
                        view! {
                            <div class="row">
                                <b>{a["name"].as_str().unwrap_or(&id).to_string()}</b>
                                <span class="dim">{a["role"].as_str().unwrap_or("").to_string()}</span>
                                <span class="grow"></span>
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
                <select on:change=move |e| theme_idx.set(event_target_value(&e).parse().unwrap_or(0))>
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

/// MCP servers, and the consent that arms them.
#[component]
fn Tools(status: RwSignal<String>) -> impl IntoView {
    let list = RwSignal::new(Vec::<Value>::new());
    let load = move || {
        leptos::task::spawn_local(async move {
            if let Ok(v) = api::get_json("/api/mcp").await {
                list.set(v["catalog"].as_array().cloned().unwrap_or_default());
            }
        });
    };
    Effect::new(move |_| load());

    view! {
        <section class="wide">
            <h3>"Tool servers (MCP)"</h3>
            <p class="hint">
                "Read-only servers are on by default. Anything that writes outside the
                 workspace or needs a credential stays off until you turn it on here —
                 a default can never arm one."
            </p>
            <For each=move || list.get() key=|e| e["id"].as_str().unwrap_or("").to_string() let:e>
                {
                    let id = e["id"].as_str().unwrap_or("").to_string();
                    let on = e["enabled"].as_bool().unwrap_or(false);
                    let tier = e["tier"].as_str().unwrap_or("").to_string();
                    view! {
                        <div class="row tool">
                            <label class="check">
                                <input type="checkbox" prop:checked=on on:change=move |ev| {
                                    let (id, next) = (id.clone(), event_target_checked(&ev));
                                    status.set("saving…".into());
                                    leptos::task::spawn_local(async move {
                                        let patch = json!([{ "mcpDefaults": { id: { "enabled": next } } }]);
                                        match api::rpc("config:update", patch).await {
                                            Ok(_) => status.set("saved".into()),
                                            Err(e) => status.set(e),
                                        }
                                        load();
                                    });
                                }/>
                                <b>{e["label"].as_str().unwrap_or("").to_string()}</b>
                            </label>
                            <span class=move || if tier == "safe-readonly" { "tag" } else { "tag warn" }>
                                {e["tier"].as_str().unwrap_or("").to_string()}
                            </span>
                            <span class="dim">{e["description"].as_str().unwrap_or("").to_string()}</span>
                        </div>
                    }
                }
            </For>
        </section>
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
