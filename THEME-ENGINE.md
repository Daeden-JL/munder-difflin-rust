# Theme engine

Lets a user swap the cast, the pixel-map, and the flavor of the whole floor —
The Office today, Serenity/Firefly or anything else tomorrow.

## What the code already gives us

The current implementation is closer to themeable than it looks, and the design
below is mostly *extraction*, not invention:

| Piece | Where | Already data-driven? |
|---|---|---|
| Cast roster | `scene/office/cast.ts` | **Yes** — `OFFICE_CAST[]` of `{name, displayName, shirt, blurb}` |
| Character art | `scene/office/portraitArt.ts` | **Yes** — `RECIPES: Record<Name, Recipe>` |
| Flavor lines | `scene/office/cafeteriaLines.ts` | **Yes** — per-character pools + `GENERIC` fallback |
| Map | `assets/maps/*.tmj` + `tilesets/` | **Yes** — Tiled maps; `brooklyn99.tmj` already exists |
| UI palette | `design/theme.ts` | Partly |

The single most important fact: **characters are procedurally painted from
recipes, not from sprite sheets.**

```ts
michael: { skin: 'light', hairc: [58,42,28], hair: 'styleShort', hairargs: { part: 'L' },
           cloth: 'suit', c1: [58,63,74], tie: [170,58,58], brow: 'flat', mouth: 'smile' }
```

A new cast is a new *table*, not commissioned pixel art. That is what makes this
feature cheap enough to be worth doing.

## What blocks it today

1. **`OfficeCharacterName` is a hardcoded union type.** It keys `cast.ts`,
   `RECIPES`, `cafeteriaLines`, and the sprite cache. A theme cannot add a
   character without editing the type, so this must become an opaque
   `CharacterId(String)`.
2. **Themes are code, not data.** Adding one means recompiling. Everything below
   assumes themes become loadable data files.
3. **Agents are bound to character names.** Switching theme has to answer "who
   does Dwight become?" — see archetypes.
4. **`portraitArt.ts` primitives are Office-shaped.** `cloth: 'suit'`, `tie`.
   Firefly needs `browncoat`, `vest`, `suspenders`. Themes must be able to
   declare new part compositions, or be limited to recombining existing ones.

## Design

### Archetypes — the key idea

Agents must survive a theme switch. If an agent is bound to `dwight` and the user
switches to Serenity, that binding is meaningless.

So a character does not identify an agent — an **archetype slot** does. Every
theme fills the same slots, and switching themes remaps through them:

| Archetype | The Office | Serenity |
|---|---|---|
| `leader` (the user's clone) | Michael | Mal |
| `second` | Dwight | Zoë |
| `operator` | Jim | Wash |
| `engineer` | — | Kaylee |
| `analyst` | Oscar | Simon |
| `wildcard` | Creed | River |
| `muscle` | — | Jayne |
| `counsel` | Toby | Book |
| `liaison` | Pam | Inara |

An agent stores `{ archetype, characterId }`. On theme switch, `characterId` is
re-resolved from `archetype`; the agent keeps its identity, memory, and desk. A
theme that leaves a slot unfilled falls back to a generic character rather than
dropping the agent.

### Theme package layout

Themes are **data directories**, not code:

```
themes/
  the-office/
    theme.toml        # identity, palette, map, cast roster
    recipes.toml      # pixel recipes per character
    lines.toml        # flavor text pools
    map.tmj
    tilesets/*.png
  serenity/
    …
```

### `theme.toml`

```toml
[theme]
id            = "serenity"
name          = "Serenity"
# What the workplace is called; replaces "Munder Difflin" in UI copy.
org_name      = "Serenity"
org_subtitle  = "a Firefly-class transport"
map           = "map.tmj"

[palette]                      # feeds design/theme.ts
accent    = "#c8a24a"
surface   = "#2a2320"
text      = "#efe6d8"

[[cast]]
id          = "mal"
display_name = "Mal"
archetype   = "leader"
accent      = "#8a5a3c"
blurb       = "Captain. Still flying."

[[cast]]
id          = "kaylee"
display_name = "Kaylee"
archetype   = "engineer"
accent      = "#d98fb0"
blurb       = "Keeps her running"
```

### `recipes.toml`

Same shape as today's `Recipe`, as data:

```toml
[mal]
skin = "light"
hair = "styleShort"
hair_color = [46, 32, 24]
cloth = "browncoat"          # new primitive, see below
c1 = [104, 74, 48]
brow = "flat"
mouth = "neutral"

[kaylee]
skin = "light"
hair = "stylePony"
hair_color = [58, 42, 28]
cloth = "coveralls"
c1 = [150, 128, 96]
mouth = "smile"
```

### Primitives

`cloth`, `hair`, `brow`, `mouth` are functions in `portraitArt.ts`. Two tiers:

- **Built-in primitives** ship with the engine (`suit`, `blouse`, `styleShort`,
  …). Any theme may use them.
- **Theme primitives** are declared as *composition* of built-ins plus color
  bands, so a theme stays pure data:

```toml
[primitives.browncoat]
base   = "coat"
bands  = [{ rows = "12-20", color = [104, 74, 48] }]
collar = "wide"
```

A theme requesting an unknown primitive falls back to the nearest built-in and
logs it, rather than rendering an empty sprite. Themes needing genuinely new
geometry require an engine change — accepted limit, documented up front.

### Map and tilesets

Already solved: `TiledMapRenderer.ts` consumes `.tmj`. A theme points at its own
map and tilesets. Seat/desk spawn points come from named Tiled object layers, so
a ship deck with 9 bunks works exactly like an office with 9 desks. Constraint to
enforce: **a theme's map must expose at least as many seats as the roster**, or
agents have nowhere to sit — validate at load.

### Flavor text

`cafeteriaLines.ts` already has the right shape (per-character pools, `BreakSpot`
contexts, `GENERIC` fallback). It becomes `lines.toml`:

```toml
[spots.galley]
generic = ["protein again", "somebody's in my chair"]

[characters.kaylee.galley]
solo = ["she's purring today", "just needs a new catalyzer"]

[[pairs]]
a = "mal"; b = "jayne"
lines = ["we're not stealing it", "we're just… holding it"]
```

## Where it lives in the Rust port

This lands in **Phase D** with the WASM UI, but the *data model* should be
defined in Phase C so it is not retrofitted:

- `md-theme` crate: parse and validate theme packages, resolve archetypes, expose
  the recipe interpreter.
- Server-side: themes are **per-tenant** — a tenant's selected theme is config,
  and custom theme packages live under the tenant home. This falls out of the
  tenancy work already done.
- New RPC channels (additions to the 161, not ports): `theme:list`,
  `theme:get`, `theme:select`, `theme:install`, `theme:validate`.
- The recipe painter moves into the `wgpu` renderer (task #11) — it is already
  pure pixel-buffer composition, so it ports almost directly and does not depend
  on Pixi.

Because themes are data rather than bundled code, third-party casts are
user-installed files rather than something the project redistributes — which
also keeps the engine clear of the IP that themed casts inevitably involve.

## Build order

1. **Extract** — `OfficeCharacterName` → `CharacterId(String)`; move the Office
   cast into `themes/the-office/` as the first package. No behavior change; this
   is the whole risk-bearing step.
2. **Load** — theme package parser + validator (seat count, archetype coverage,
   unknown primitives).
3. **Archetypes** — add the slot to agent records, implement remap-on-switch.
4. **Switch** — `theme:select` + UI picker, with live re-render.
5. **Second theme** — build Serenity as the proof; anything that needs an engine
   change to make it work is a gap in step 2.
6. **Author docs** — so themes can be contributed without touching Rust.

Step 5 is the real test. The Office theme will pass any validator written
alongside it; a genuinely different cast is what finds the assumptions.
