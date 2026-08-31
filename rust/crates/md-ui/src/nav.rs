//! Getting from one place to another without going through a wall.
//!
//! A room drawn side-on is one open floor: a straight line from anywhere to
//! anywhere is a walk somebody could take, which is why the floor had no
//! navigation at all. A deck plan is not — it is rooms joined by corridors and
//! hatches, and a straight line from the bridge to the engine room crosses six
//! bulkheads.
//!
//! The model is deliberately small. A theme lists the **rectangles its crew can
//! stand in**; two that overlap are joined, and the overlap IS the doorway. A
//! route is a breadth-first walk over those rectangles, turned into waypoints at
//! the centre of each overlap. Because a rectangle is convex and both ends of
//! every leg lie inside one, no leg can leave walkable space — the guarantee
//! comes from the shape of the model rather than from a collision check.
//!
//! Rectangles are in **feet space**: where a character stands, not where their
//! sprite's top-left corner is. A figure drawn side-on is taller than most of
//! the rooms it walks through, so its body is over the room behind it more often
//! than not; its feet are the only part that is actually anywhere.

/// Where a character's feet are, relative to the sprite's top-left. The sprite
/// is 18×32 and stands on its bottom edge.
pub const FOOT_DX: f64 = 9.0;
pub const FOOT_DY: f64 = 30.0;

pub fn to_feet(p: [f64; 2]) -> [f64; 2] {
    [p[0] + FOOT_DX, p[1] + FOOT_DY]
}

pub fn to_sprite(p: [f64; 2]) -> [f64; 2] {
    [p[0] - FOOT_DX, p[1] - FOOT_DY]
}

/// The walkable space of one room, and what it connects to.
pub struct Nav {
    rects: Vec<[f64; 4]>,
    /// For each rectangle, the rectangles it opens onto and where the opening
    /// is. Precomputed: adjacency never changes for a given theme, and a route
    /// is asked for every time anyone chooses somewhere to go.
    doors: Vec<Vec<(usize, [f64; 2])>>,
}

fn contains(r: &[f64; 4], p: [f64; 2]) -> bool {
    p[0] >= r[0] && p[0] <= r[2] && p[1] >= r[1] && p[1] <= r[3]
}

/// The nearest point of a rectangle to `p`, and how far away it is.
fn nearest(r: &[f64; 4], p: [f64; 2]) -> ([f64; 2], f64) {
    let q = [p[0].clamp(r[0], r[2]), p[1].clamp(r[1], r[3])];
    let (dx, dy) = (q[0] - p[0], q[1] - p[1]);
    (q, dx * dx + dy * dy)
}

impl Nav {
    /// Build the graph. Two rectangles that overlap are joined through the
    /// centre of the overlap, so a theme author connects two rooms by having
    /// their walkable boxes share a few pixels — which is what a doorway is.
    pub fn new(walk: &[[f64; 4]]) -> Self {
        let rects: Vec<[f64; 4]> = walk
            .iter()
            // Normalised, so a box authored bottom-up still works rather than
            // silently containing nothing.
            .map(|r| [r[0].min(r[2]), r[1].min(r[3]), r[0].max(r[2]), r[1].max(r[3])])
            .collect();

        let mut doors = vec![Vec::new(); rects.len()];
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                let (a, b) = (&rects[i], &rects[j]);
                let (x0, x1) = (a[0].max(b[0]), a[2].min(b[2]));
                let (y0, y1) = (a[1].max(b[1]), a[3].min(b[3]));
                if x0 > x1 || y0 > y1 {
                    continue;
                }
                let mid = [(x0 + x1) / 2.0, (y0 + y1) / 2.0];
                doors[i].push((j, mid));
                doors[j].push((i, mid));
            }
        }
        Self { rects, doors }
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// Which room a point is in.
    fn zone(&self, p: [f64; 2]) -> Option<usize> {
        self.rects.iter().position(|r| contains(r, p))
    }

    /// The closest walkable point, for a target that is inside a wall.
    ///
    /// Wandering picks points out of a box that does not know about the rooms,
    /// and a post can be authored a pixel outside its own floor. Both should
    /// put someone against the nearest wall rather than refusing to move.
    pub fn snap(&self, p: [f64; 2]) -> [f64; 2] {
        self.rects
            .iter()
            .map(|r| nearest(r, p))
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(q, _)| q)
            .unwrap_or(p)
    }

    /// The rooms to pass through, from one point to another.
    fn rooms(&self, from: usize, to: usize) -> Option<Vec<usize>> {
        if from == to {
            return Some(vec![from]);
        }
        // Breadth-first, so the route has as few rooms in it as possible.
        // Distance would be better and is not worth it: these graphs have a
        // dozen nodes and the difference is never visible.
        let mut prev = vec![usize::MAX; self.rects.len()];
        let mut queue = std::collections::VecDeque::from([from]);
        prev[from] = from;
        while let Some(at) = queue.pop_front() {
            if at == to {
                let mut path = vec![to];
                while *path.last()? != from {
                    path.push(prev[*path.last()?]);
                }
                path.reverse();
                return Some(path);
            }
            for (next, _) in &self.doors[at] {
                if prev[*next] == usize::MAX {
                    prev[*next] = at;
                    queue.push_back(*next);
                }
            }
        }
        None
    }

    /// Whether there is a way from one point to another at all.
    ///
    /// Separate from `route`, which answers "walk there" and falls back to a
    /// straight line rather than leaving somebody standing still. That fallback
    /// is right at runtime and useless to a test — asserting on the route's
    /// last point says nothing, because an unreachable target is still its own
    /// last point.
    pub fn connected(&self, from: [f64; 2], to: [f64; 2]) -> bool {
        match (self.zone(self.snap(from)), self.zone(self.snap(to))) {
            (Some(a), Some(b)) => self.rooms(a, b).is_some(),
            // Nothing to be cut off from.
            _ => self.rects.is_empty(),
        }
    }

    /// Waypoints from `from` to `to`, in feet space, ending at the target.
    ///
    /// A target in a room nothing connects to is not silently dropped — the
    /// walk still happens, in a straight line, exactly as it did before there
    /// was any navigation. An unreachable room is a bug in the theme, and a
    /// figure standing still forever is a much harder one to notice than a
    /// figure taking a shortcut.
    pub fn route(&self, from: [f64; 2], to: [f64; 2]) -> Vec<[f64; 2]> {
        let to = if self.zone(to).is_some() { to } else { self.snap(to) };
        let (Some(a), Some(b)) = (self.zone(from).or_else(|| self.zone(self.snap(from))), self.zone(to))
        else {
            return vec![to];
        };
        let Some(rooms) = self.rooms(a, b) else {
            return vec![to];
        };

        let mut out = Vec::with_capacity(rooms.len());
        for pair in rooms.windows(2) {
            if let Some((_, door)) = self.doors[pair[0]].iter().find(|(n, _)| *n == pair[1]) {
                out.push(*door);
            }
        }
        out.push(to);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three rooms in a row, joined only at their overlaps: getting from the
    /// first to the third has to pass through the second.
    fn three_in_a_row() -> Nav {
        Nav::new(&[
            [0.0, 0.0, 20.0, 10.0],
            [18.0, 0.0, 40.0, 10.0],
            [38.0, 0.0, 60.0, 10.0],
        ])
    }

    #[test]
    fn a_route_across_three_rooms_stops_at_both_doorways() {
        let nav = three_in_a_row();
        let route = nav.route([5.0, 5.0], [55.0, 5.0]);
        assert_eq!(route.len(), 3, "two doorways and the target: {route:?}");
        assert_eq!(route[0][0], 19.0, "the first doorway is the first overlap");
        assert_eq!(route[1][0], 39.0);
        assert_eq!(route[2], [55.0, 5.0]);
    }

    #[test]
    fn a_route_within_one_room_is_a_straight_line() {
        let nav = three_in_a_row();
        assert_eq!(nav.route([2.0, 2.0], [15.0, 8.0]), vec![[15.0, 8.0]]);
    }

    /// Every leg has to stay inside a single room, or the whole model is
    /// pointless: that is what makes a straight-line walk safe.
    #[test]
    fn every_leg_of_a_route_stays_inside_one_room() {
        // An L: the only way between the arms is the corner they share.
        let nav = Nav::new(&[
            [0.0, 0.0, 10.0, 40.0],
            [8.0, 30.0, 50.0, 40.0],
        ]);
        let mut at = [5.0, 5.0];
        for leg in nav.route(at, [45.0, 35.0]) {
            let shared = nav
                .rects
                .iter()
                .any(|r| contains(r, at) && contains(r, leg));
            assert!(shared, "leg {at:?} -> {leg:?} crosses a wall");
            at = leg;
        }
    }

    #[test]
    fn a_target_inside_a_wall_is_pulled_to_the_nearest_floor() {
        let nav = three_in_a_row();
        // Well above every room.
        let route = nav.route([5.0, 5.0], [30.0, 90.0]);
        assert_eq!(*route.last().unwrap(), [30.0, 10.0]);
    }

    /// A room nothing connects to must not strand whoever is sent there.
    #[test]
    fn an_unreachable_target_still_gets_walked_to() {
        let nav = Nav::new(&[[0.0, 0.0, 10.0, 10.0], [90.0, 90.0, 100.0, 100.0]]);
        assert_eq!(nav.route([5.0, 5.0], [95.0, 95.0]), vec![[95.0, 95.0]]);
        // ...but it is still not connected, and `route` alone cannot say so.
        assert!(!nav.connected([5.0, 5.0], [95.0, 95.0]));
        assert!(nav.connected([5.0, 5.0], [8.0, 8.0]));
    }

    #[test]
    fn three_rooms_in_a_row_are_all_connected() {
        let nav = three_in_a_row();
        assert!(nav.connected([5.0, 5.0], [55.0, 5.0]));
    }

    #[test]
    fn a_theme_with_no_walkable_space_routes_straight_there() {
        let nav = Nav::new(&[]);
        assert!(nav.is_empty());
        assert_eq!(nav.route([1.0, 2.0], [3.0, 4.0]), vec![[3.0, 4.0]]);
    }

    #[test]
    fn feet_and_sprite_coordinates_round_trip() {
        assert_eq!(to_sprite(to_feet([12.0, 34.0])), [12.0, 34.0]);
    }
}
