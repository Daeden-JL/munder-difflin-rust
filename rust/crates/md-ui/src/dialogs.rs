//! Modal dialogs: closing time, and first-run theme selection.

use leptos::prelude::*;
use serde_json::json;

use crate::api;
use crate::theme;

/// A modal shell. Everything inside is centred over a scrim, and clicking the
/// scrim dismisses — but only where dismissing is safe, which is why `on_close`
/// is optional.
#[component]
pub fn Modal(
    #[prop(optional)] on_close: Option<Callback<()>>,
    #[prop(into)] title: String,
    children: Children,
) -> impl IntoView {
    view! {
        <div class="scrim" on:click=move |_| { if let Some(c) = on_close { c.run(()); } }>
            // Stop the click on the panel itself from reaching the scrim, or
            // every interaction inside would dismiss the dialog.
            <div class="modal" on:click=move |e| e.stop_propagation()>
                <div class="modal-head">
                    <b>{title}</b>
                    <span class="grow"></span>
                    <Show when=move || on_close.is_some()>
                        <button class="ghost" on:click=move |_| {
                            if let Some(c) = on_close { c.run(()); }
                        }>"close"</button>
                    </Show>
                </div>
                <div class="modal-body">{children()}</div>
            </div>
        </div>
    }
}

/// What to do when the user closes the floor.
///
/// Three genuinely different intentions, and the old single button silently
/// picked one. Agents are long-running processes: leaving is not the same as
/// stopping, and stopping is not the same as stopping *safely*.
#[component]
pub fn ClosingTime(open: RwSignal<bool>) -> impl IntoView {
    let status = RwSignal::new(String::new());
    let busy = RwSignal::new(false);

    // The graceful path: the god broadcasts, every worker saves its state and
    // acknowledges, and only then does the floor wind down.
    let wind_down = move |_| {
        busy.set(true);
        status.set("asking the orchestrator to close the floor…".into());
        leptos::task::spawn_local(async move {
            match api::rpc("app:startClosingTime", json!([])).await {
                Ok(v) if v["ok"] == true => {
                    status.set(
                        "Closing time started. Agents are saving their state; \
                         the floor closes once every one has confirmed."
                            .into(),
                    );
                }
                Ok(v) => status.set(v["error"].as_str().unwrap_or("could not start").to_string()),
                Err(e) => status.set(e),
            }
            busy.set(false);
        });
    };

    let leave_running = move |_| {
        leptos::task::spawn_local(async move {
            let _ = api::post_json("/api/logout", &json!({})).await;
            // A full reload rather than clearing state by hand: the session
            // cookie is gone, and every signal should start from that fact.
            let _ = window().location().reload();
        });
    };

    let stop_now = move |_| {
        busy.set(true);
        status.set("stopping every agent…".into());
        leptos::task::spawn_local(async move {
            // Kill each pty. Deliberately NOT the graceful path: this is the
            // "I know, do it anyway" option.
            if let Ok(list) = api::rpc("pty:list", json!([])).await {
                for s in list.as_array().cloned().unwrap_or_default() {
                    if let Some(id) = s["id"].as_str() {
                        let _ = api::rpc("pty:kill", json!([id])).await;
                    }
                }
            }
            status.set("all agents stopped.".into());
            busy.set(false);
        });
    };

    view! {
        <Show when=move || open.get()>
            <Modal title="Closing time" on_close=Callback::new(move |_| open.set(false))>
                <p class="lead">
                    "Your agents are long-running processes. Leaving is not the same as
                     stopping them, so pick what you actually want."
                </p>

                <div class="choice">
                    <button prop:disabled=move || busy.get() on:click=wind_down>
                        "Close the floor safely"
                    </button>
                    <p>
                        "The orchestrator tells everyone to finish up. Each agent parks its
                         work, writes what it learned to memory, and confirms. The floor
                         closes once every one of them has — nothing is lost mid-thought."
                    </p>
                </div>

                <div class="choice">
                    <button class="ghost" prop:disabled=move || busy.get() on:click=leave_running>
                        "Sign out, leave them running"
                    </button>
                    <p>
                        "You leave; they carry on. Work continues in the background and is
                         waiting when you sign back in."
                    </p>
                </div>

                <div class="choice">
                    <button class="ghost danger" prop:disabled=move || busy.get() on:click=stop_now>
                        "Stop every agent now"
                    </button>
                    <p>
                        "Immediate. Anything an agent was holding but had not written down
                         is lost — use this when something is wrong, not to finish a day."
                    </p>
                </div>

                <p class="status">{move || status.get()}</p>
            </Modal>
        </Show>
    }
}

/// First run: pick a theme before anything else.
///
/// Shown once, because the floor is the first thing anyone sees and a
/// deliberate choice beats defaulting silently to the Office. Choosing writes
/// to the tenant config, so the answer survives a reload and a different
/// browser.
#[component]
pub fn ThemePicker(open: RwSignal<bool>, theme_idx: RwSignal<usize>) -> impl IntoView {
    let choose = move |i: usize| {
        theme_idx.set(i);
        open.set(false);
        leptos::task::spawn_local(async move {
            let id = theme::builtin().get(i).map(|t| t.id.clone()).unwrap_or_default();
            let _ = api::rpc("config:update", json!([{ "theme": id, "themeChosen": true }])).await;
        });
    };

    view! {
        <Show when=move || open.get()>
            // No dismiss: this is a choice, and a modal you can click past is a
            // choice that gets skipped by accident.
            <Modal title="Pick a floor">
                <p class="lead">
                    "Your agents work somewhere. Pick where — it changes the room, the cast
                     and how they behave, but never who your agents are or what they remember.
                     You can change it later in setup."
                </p>
                <div class="theme-grid">
                    // Rebuilt per render: `Show` re-runs its children, so a
                    // list consumed once would be empty the second time.
                    {theme::builtin().into_iter().enumerate().map(|(i, t)| {
                        let leader = t.character("leader").map(|c| c.display.clone()).unwrap_or_default();
                        let blurb = t.character("leader")
                            .map(|c| c.personality.trait_line.clone())
                            .unwrap_or_default();
                        let cast: Vec<String> = theme::ARCHETYPES
                            .iter()
                            .filter_map(|a| t.character(a).map(|c| c.display.clone()))
                            .take(5)
                            .collect();
                        view! {
                            <button class="theme-card" on:click=move |_| choose(i)>
                                <div class="swatch" style=format!(
                                    "background:{};border-color:{}", t.layout.floor, t.layout.wall)>
                                    <span class="wall" style=format!("background:{}", t.layout.wall)></span>
                                </div>
                                <b>{t.name.clone()}</b>
                                <i>{cast.join(" · ")}</i>
                                <span class="dim">{format!("{leader} — {blurb}")}</span>
                            </button>
                        }
                    }).collect::<Vec<_>>()}
                </div>
            </Modal>
        </Show>
    }
}
