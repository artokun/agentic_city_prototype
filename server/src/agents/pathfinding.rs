use std::collections::{HashMap, VecDeque};

use crate::world::map::{GridPos, WorldMap};

const NEIGHBORS: [(i32, i32); 4] = [(0, 1), (0, -1), (1, 0), (-1, 0)];

pub fn bfs(map: &WorldMap, from: GridPos, to: GridPos) -> Option<VecDeque<GridPos>> {
    if from == to {
        return Some(VecDeque::new());
    }

    let mut visited: HashMap<GridPos, GridPos> = HashMap::new();
    let mut queue = VecDeque::new();
    queue.push_back(from);
    visited.insert(from, from);

    while let Some(current) = queue.pop_front() {
        for (dx, dy) in NEIGHBORS {
            let next = GridPos {
                x: current.x + dx,
                y: current.y + dy,
            };

            if visited.contains_key(&next) {
                continue;
            }

            if !map.is_walkable(&next) {
                continue;
            }

            visited.insert(next, current);

            if next == to {
                // Reconstruct path (excluding `from`)
                let mut path = VecDeque::new();
                let mut step = to;
                while step != from {
                    path.push_front(step);
                    step = visited[&step];
                }
                return Some(path);
            }

            queue.push_back(next);
        }
    }

    None
}
