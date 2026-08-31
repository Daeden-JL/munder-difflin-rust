# Writing a theme

A theme is one JSON file. It supplies a **cast** — nine characters, each an art
recipe and a personality — and a **room** for them to work in. There is no art
to commission and no code to change: the painter never names a character, so a
new theme costs a file.

Start by copying `rust/crates/md-ui/themes/office.json` and editing it. Add the
filename to `builtin()` in `theme.rs`, run `cargo test -p md-ui`, and the tests
will tell you what is missing.

---

## The shape

```jsonc
{
  "id": "serenity",              // unique, lowercase
  "name": "Serenity",            // shown in the theme picker
  "layout": { ... },             // the room
  "cast": { "leader": { ... } }  // archetype → character
}
```

---

## 1. The cast: nine archetypes

Agents bind to an **archetype**, never to a character. Binding an agent to
`dwight` would be meaningless the moment someone switched themes; binding it to
`second` is not. Every theme fills the same nine slots, so a switch preserves
each agent's identity, memory and desk and changes only who they look like.

| slot | the role it plays | Office | Serenity | TOS |
|---|---|---|---|---|
| `leader` | the orchestrator | Michael | Mal | Kirk |
| `second` | the enforcer | Dwight | Zoe | Spock |
| `operator` | the one who does it | Jim | Wash | Sulu |
| `engineer` | keeps it running | Kevin | Kaylee | Scotty |
| `analyst` | checks the numbers | Oscar | Simon | McCoy |
| `wildcard` | unpredictable | Creed | River | Chekov |
| `muscle` | blunt instrument | Stanley | Jayne | Chapel |
| `counsel` | the conscience | Toby | Book | Uhura |
| `liaison` | holds it together | Pam | Inara | Rand |

Fill all nine. A theme that fills only some still works — a missing slot falls
back rather than leaving an invisible agent — but a test will tell you about it,
because a half-cast theme is nearly always an oversight.

**Naming matters.** Where a character's name matches an agent's, that binding
wins over the slot order. An agent called `pam` on the Office theme is Pam, not
whoever the ordering would have picked.

---

## 2. Characters: art as a recipe

```jsonc
"leader": {
  "display": "Mal",
  "recipe": {
    "skin": "light",                    // light | tan | brown | dark
    "hairc": [56, 40, 26],              // RGB
    "hair": "styleShort",               // see below
    "part": "R",                        // styleShort only: L or R
    "cloth": "dressshirt",              // see below
    "c1": [122, 90, 60],                // garment colour
    "brow": "flat",                     // flat | angry | raised | soft
    "mouth": "neutral"                  // neutral | smile | frown | grin
  },
  "personality": { ... }
}
```

**Hair** — `styleShort` (takes `part`, `recede`), `styleFloppy`, `styleFrame`
(takes `length`, `vol` — the long one), `styleBun`, `styleCurly`, `styleMessy`
(takes `length`), `styleRecede`, `styleSpiky`, `styleBald` (takes `recede`).

**Clothing** — `suit` (takes `tie`), `dressshirt` (takes `tie`), `polo` (takes
`c2` for an accent), `blouse`, `cardigan` (takes `c2` for the inner layer),
`sweater`.

**Optional** — `facial` (`mustache`, `mustacheSm`, `stubble`, `goatee`),
`glasses`, `blush`, `lashes` (bigger, lashed eyes), `heavy` (a wider build and a
fuller face — a different silhouette, not a recolour).

Shading is derived from each colour you give, so you supply one colour per
garment rather than three.

### Two failure modes, deliberately different

* An unknown **field** is rejected at parse. A typo like `"glases": true` is an
  error you see, not a hat that silently never appears.
* An unknown **value** still paints. A theme inventing `"cloth": "browncoat"`
  gets a plain garment rather than an invisible character — a mistake in a theme
  file must never blank someone out of the floor.

---

## 3. Personality: flavour is data

```jsonc
"personality": {
  "traitLine": "Captain. Aims to misbehave.",
  "idle":    ["We're still flyin'.", "That's good enough."],
  "working": ["Let's get to it.", "We do the job, we get paid."],
  "greet":   ["Cap'n.", "We're all right."],
  "restless": 0.6,
  "primaryPoi": "bridge",        // where they work
  "secondaryPoi": "cargo-hold"   // where else you find them
}
```

The Office floor's charm came from characters having opinions — but hard-coded
ones, which is why it was one cast forever. Here it is data, so a theme has a
personality rather than being a re-skin.

It shows four ways:

* `traitLine` appears in the conversation header when the agent is selected.
* `idle` is muttered while wandering, on settling rather than on a timer, so a
  line reads as a thought.
* `working` is said on arriving at a station to do real work.
* `greet` fires when two characters end up standing together — rarely, or
  neighbours would chatter constantly.
* `restless` (0.0–1.0) scales how long they linger before wandering. Creed at
  0.9 and Stanley at 0.15 move differently without a line of movement code.
* `primaryPoi` / `secondaryPoi` are POI ids (see *The map*, below). The first is
  the character's workstation — they idle there and return to it. The second is
  where they turn up when they wander, which is what stops "not at your desk"
  meaning "anywhere". Both are the theme's DEFAULT: an operator can post an
  agent somewhere else when they hire it.

Write short lines. They render in a speech bubble about 150 pixels wide.

---

## 4. The room

```jsonc
"layout": {
  "wall": "#2f3a44",        // back wall
  "wallDepth": 42,          // how deep the wall band is
  "floor": "#6b5f4c",
  "trim": "#4a4034",        // skirting between the two planes
  "grid": "#101f3c",        // optional blueprint grid over the floor
  "gridStep": 8,
  "props": [ ... ],         // scenery
  "stations": [ ... ],      // where work happens
  "pois": [ ... ],          // named places, and the map's legend
  "poiLabels": true,        // paint those names onto the room
  "desks": { ... },         // each archetype's fallback place
  "doors": [ ... ],         // ways in and out
  "walk": [ ... ],          // the rooms, and so the walls between them
  "roam": [48, 84, 268, 140]
}
```

The room is **320 × 176** pixels. It is scaled up with nearest-neighbour
sampling, so think in whole pixels and keep detail coarse.

### Props: the room itself

```jsonc
{ "x": 62, "y": 80, "w": 46, "h": 16, "color": "#a08d70", "lip": true }
{ "x": 128, "y": 150, "w": 64, "h": 18, "color": "#8f7f66", "round": true }
```

Painted in order, so a later prop sits on top of an earlier one — that is how
you put a console on a dais without a z-index. `lip` adds a darker front edge,
which is what makes a flat rectangle read as a surface you could put a mug on.
`round` draws an ellipse: rugs, hatches, a warp core, a landing pad. `border`
outlines the rectangle, which is what turns a stack of them into a deck plan —
rooms on a plan are read from their walls, not their fill.

Primitives rather than sprites, on purpose. A room you can write in a text
editor is a room people will actually write.

### Stations: where work happens

```jsonc
{ "kind": "terminal", "label": "ENGINE", "x": 276, "y": 92, "w": 36, "h": 22, "color": "#6a4a3a" }
```

A station is a **destination**, and this is what makes the floor mean something.
When an agent runs a tool, the hook event carries the tool's name and the agent
walks to the matching station:

| `kind` | tools that send an agent here |
|---|---|
| `shelf` | `Read` `Edit` `Write` `MultiEdit` `Grep` `Glob` `NotebookEdit` |
| `terminal` | `Bash` `BashOutput` `KillShell` |
| `web` | `WebFetch` `WebSearch` |
| `board` | `TodoWrite` `TaskCreate` `TaskUpdate` |
| `mailbox` | mail and messages |
| `desk` | anything else |

**All five kinds are required.** The `label` is yours: a `terminal` can be
ENGINE on a ship, ENGINEERING on a bridge, or HANGAR in a docking bay. That is
where most of a theme's character comes from.

`h` (default 22) is how deep the console is drawn, and the figure stands just
clear of its bottom edge. A room seen side-on can afford a chunky slab; a room
seen from above cannot, because the slab would be most of the room — so Serenity
uses `"h": 10` and puts each console at the top of the room it belongs to, which
lands the person who works there in the middle of it.

### POIs: the places on the map

```jsonc
"pois": [
  { "id": "engine-room", "label": "Engine", "x": 282, "y": 92 },
  { "id": "galley",      "label": "Galley", "x": 128, "y": 100 }
]
```

A station answers *where does this tool happen*; a POI answers *where does this
person belong*. Keeping them separate is what stops the floor emptying out
whenever nobody is running a tool: the engine room is Kaylee's post whether or
not there is a shell open in it.

`x`/`y` is a **standing spot** — the top-left of an 18×32 figure, so the feet
land 32 pixels below. Place a post so its occupant stands inside the room it
names, not so the coordinate does.

POIs are also the **posting list**. When someone hires an agent they pick a
personality from the cast and, optionally, a primary and secondary POI to
override that character's own. The ids are therefore public: renaming one
orphans every agent posted to it, which is why `id` is separate from `label`.

`poiLabels` paints those names onto the room. True for a map read as a plan,
false for a room read as a room — an office does not label its own kitchen.

Post ids are per-theme, and that is fine: an agent carrying `engine-room` onto
the Office floor falls back to whatever the Office character it is dressed as
does. Nobody is ever stranded at (0, 0).

### Desks: where each character belongs

```jsonc
"desks": {
  "leader":   [150, 64],
  "engineer": [262, 96]
}
```

One `[x, y]` per archetype. Characters **gravitate home**: an idle agent mostly
returns to its own post and only sometimes wanders, weighted by `restless`. A
floor where everyone drifts anywhere reads as a crowd; a floor where everyone
has a place reads as a workplace. Give the `leader` the best seat — the corner
office, the captain's chair, the centre of the bridge.

Desks are now the **fallback**, below the agent's own posting and the
character's `primaryPoi`. Keep them: they are what catches a character with no
post and an agent carrying a post id from a theme this one has never heard of.

### Doors: ways in and out

```jsonc
{ "label": "TURBOLIFT", "x": 148, "y": 46, "w": 28, "h": 34,
  "color": "#3d4a66", "threshold": [162, 86] }
```

`threshold` is where someone standing in the doorway is: a newly hired agent
appears there and walks to its desk, so joining the floor is something you see
happen rather than a figure blinking into existence.

Doors are also most of a room's identity. Serenity has two shuttles and the
stairs to the catwalk; both Star Trek ships have a turbolift; the Office has
reception and the break room; A New Hope has the bay doors and the ship's ramp.

### Walk: where the floor actually is

```jsonc
"walk": [
  [ 78,  74, 168,  88],   // the spine corridor
  [ 82,  82, 118, 138],   // the infirmary, opening onto it
  [124,  82, 160, 138]    // the galley, likewise
]
```

Rectangles a character can **stand** in. Two that overlap are joined, and the
overlap is the doorway — you connect two rooms by having their boxes share a few
pixels. A walk is then a breadth-first route over those rooms, turning at the
centre of each opening, and because a rectangle is convex and both ends of every
leg are inside one, no leg can cross a wall. The guarantee is in the shape of the
model, not in a collision check.

Leave it out and the room is one open floor with straight-line movement, which is
correct for a room drawn side-on: a straight line across an office is a walk
somebody could take. The four side-on themes carry none. A **deck plan needs
it** — without it the crew go through the bulkheads.

Two things to get right:

* These are in **feet space** — where the character stands, not where the
  sprite's top-left corner is. A figure is 32 pixels tall and most rooms are
  not, so its body is over the room behind it much of the time; its feet are the
  only part that is really anywhere. A post at `[128, 100]` stands at
  `[137, 130]`.
* Make each overlap at least **six pixels** across. A walker counts itself
  arrived within a pixel of its waypoint, and a doorway two pixels wide leaves
  no room for that.

A test walks a character around each themed map for several simulated minutes
and fails if its feet ever leave the deck, and another fails if any post,
console or doorway is cut off from the rest — an unreachable room is a figure
standing still forever, which is far harder to notice than one taking a
shortcut.

### Roam: where they wander

`[x0, y0, x1, y1]` — the box an idle agent wanders inside. Keep `y0` below
`wallDepth` or they will walk into the wall, and keep it clear of your props so
nobody stands inside a desk. A test checks the first of those for you.

---

## Checking your work

```sh
cd rust && cargo test -p md-ui --target aarch64-apple-darwin
./rust/build-ui.sh
```

The tests assert that every theme fills all nine slots with a paintable
character that has a trait line and something to say, provides all five station
kinds, names at least six places and posts every character to two of them that
exist, keeps its roam box out of the wall, and shares neither a character nor a
floor colour with another theme — the check that a theme is a *place* rather
than a palette swap.

---

## A word on why Serenity exists

The Office theme would pass any validator written beside it, because the engine
was shaped around it. The other four are what prove the engine real: a
genuinely different cast is what finds the assumptions the first one hides. If
you are extending the engine, add a theme that breaks your expectations and see
what falls over.
