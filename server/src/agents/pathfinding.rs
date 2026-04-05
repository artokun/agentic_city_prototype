use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::cmp::Ordering;

use crate::world::map::{GridPos, WorldMap};

/// Movement directions: 4 cardinal (cost 1.0) + 4 diagonal (cost 1.33).
const CARDINAL: [(i32, i32); 4] = [(0, 1), (0, -1), (1, 0), (-1, 0)];
const DIAGONAL: [(i32, i32); 4] = [(1, 1), (1, -1), (-1, 1), (-1, -1)];

/// Cost multiplier for diagonal movement (approximation of sqrt(2)).
const DIAGONAL_COST: u32 = 133; // 1.33x in fixed-point (100 = 1.0)
const CARDINAL_COST: u32 = 100;

#[derive(Clone, Eq, PartialEq)]
struct Node {
    pos: GridPos,
    cost: u32,      // g-cost: actual distance from start
    estimate: u32,   // f-cost: g + heuristic
}

impl Ord for Node {
    fn cmp(&self, other: &Self) -> Ordering {
        other.estimate.cmp(&self.estimate) // min-heap
    }
}

impl PartialOrd for Node {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Octile distance heuristic (consistent with 8-directional movement).
fn heuristic(a: &GridPos, b: &GridPos) -> u32 {
    let dx = (a.x - b.x).unsigned_abs();
    let dy = (a.y - b.y).unsigned_abs();
    let (min, max) = if dx < dy { (dx, dy) } else { (dy, dx) };
    // min diagonal steps + (max - min) cardinal steps
    min * DIAGONAL_COST + (max - min) * CARDINAL_COST
}

/// A* pathfinding with 8-directional movement.
/// Returns path from `from` to `to` (excluding `from`, including `to`).
/// Diagonal steps cost 1.33x cardinal steps.
pub fn astar(map: &WorldMap, from: GridPos, to: GridPos) -> Option<VecDeque<GridPos>> {
    if from == to {
        return Some(VecDeque::new());
    }

    if !map.is_walkable(&to) {
        return None;
    }

    let mut open = BinaryHeap::new();
    let mut came_from: HashMap<GridPos, GridPos> = HashMap::new();
    let mut g_cost: HashMap<GridPos, u32> = HashMap::new();

    open.push(Node {
        pos: from,
        cost: 0,
        estimate: heuristic(&from, &to),
    });
    g_cost.insert(from, 0);

    while let Some(current) = open.pop() {
        if current.pos == to {
            // Reconstruct path.
            let mut path = VecDeque::new();
            let mut step = to;
            while step != from {
                path.push_front(step);
                step = came_from[&step];
            }
            return Some(path);
        }

        let current_g = g_cost[&current.pos];

        // Skip if we've found a better path to this node already.
        if current.cost > current_g {
            continue;
        }

        // Try all 8 neighbors.
        for &(dx, dy) in CARDINAL.iter() {
            try_neighbor(map, &current.pos, dx, dy, CARDINAL_COST, current_g,
                         &to, &mut open, &mut came_from, &mut g_cost);
        }
        for &(dx, dy) in DIAGONAL.iter() {
            // For diagonal movement, both adjacent cardinal tiles must be walkable
            // to prevent cutting through wall corners.
            let adj1 = GridPos { x: current.pos.x + dx, y: current.pos.y };
            let adj2 = GridPos { x: current.pos.x, y: current.pos.y + dy };
            if map.is_walkable(&adj1) && map.is_walkable(&adj2) {
                try_neighbor(map, &current.pos, dx, dy, DIAGONAL_COST, current_g,
                             &to, &mut open, &mut came_from, &mut g_cost);
            }
        }
    }

    None
}

fn try_neighbor(
    map: &WorldMap,
    current: &GridPos,
    dx: i32, dy: i32,
    step_cost: u32,
    current_g: u32,
    target: &GridPos,
    open: &mut BinaryHeap<Node>,
    came_from: &mut HashMap<GridPos, GridPos>,
    g_cost: &mut HashMap<GridPos, u32>,
) {
    let next = GridPos { x: current.x + dx, y: current.y + dy };
    if !map.is_walkable(&next) {
        return;
    }

    let new_g = current_g + step_cost;
    if new_g < *g_cost.get(&next).unwrap_or(&u32::MAX) {
        g_cost.insert(next, new_g);
        came_from.insert(next, *current);
        open.push(Node {
            pos: next,
            cost: new_g,
            estimate: new_g + heuristic(&next, target),
        });
    }
}

/// Legacy BFS wrapper — calls A* internally for backwards compatibility.
pub fn bfs(map: &WorldMap, from: GridPos, to: GridPos) -> Option<VecDeque<GridPos>> {
    astar(map, from, to)
}

/// Validate that all building entrances are reachable from each other.
/// Call this at startup to catch map generation bugs.
pub fn validate_navmesh(map: &WorldMap, entrances: &[GridPos]) -> Result<(), Vec<(GridPos, GridPos)>> {
    let mut unreachable = Vec::new();

    for (i, &from) in entrances.iter().enumerate() {
        for &to in entrances.iter().skip(i + 1) {
            if astar(map, from, to).is_none() {
                unreachable.push((from, to));
            }
        }
    }

    if unreachable.is_empty() {
        Ok(())
    } else {
        Err(unreachable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::map::{TileType, WorldMap};
    use std::collections::HashMap;

    fn pos(x: i32, y: i32) -> GridPos {
        GridPos { x, y }
    }

    fn map_from_walkable(positions: &[GridPos]) -> WorldMap {
        let mut tiles = HashMap::new();
        for p in positions {
            tiles.insert(*p, TileType::Sidewalk);
        }
        WorldMap { tiles }
    }

    fn open_grid(w: i32, h: i32) -> WorldMap {
        let mut tiles = HashMap::new();
        for x in 0..w {
            for y in 0..h {
                tiles.insert(pos(x, y), TileType::Street);
            }
        }
        WorldMap { tiles }
    }

    #[test]
    fn same_position_returns_empty_path() {
        let map = map_from_walkable(&[pos(5, 5)]);
        let path = astar(&map, pos(5, 5), pos(5, 5)).unwrap();
        assert!(path.is_empty());
    }

    #[test]
    fn straight_line_horizontal() {
        let tiles: Vec<GridPos> = (0..=4).map(|x| pos(x, 0)).collect();
        let map = map_from_walkable(&tiles);
        let path = astar(&map, pos(0, 0), pos(4, 0)).unwrap();
        assert_eq!(path.len(), 4);
        assert_eq!(*path.back().unwrap(), pos(4, 0));
    }

    #[test]
    fn straight_line_vertical() {
        let tiles: Vec<GridPos> = (0..=3).map(|y| pos(0, y)).collect();
        let map = map_from_walkable(&tiles);
        let path = astar(&map, pos(0, 0), pos(0, 3)).unwrap();
        assert_eq!(path.len(), 3);
    }

    #[test]
    fn blocked_path_returns_none() {
        let map = map_from_walkable(&[pos(0, 0), pos(2, 0)]);
        assert!(astar(&map, pos(0, 0), pos(2, 0)).is_none());
    }

    #[test]
    fn destination_not_walkable_returns_none() {
        let map = map_from_walkable(&[pos(0, 0), pos(1, 0)]);
        assert!(astar(&map, pos(0, 0), pos(2, 0)).is_none());
    }

    #[test]
    fn diagonal_movement_on_open_grid() {
        let map = open_grid(5, 5);
        let path = astar(&map, pos(0, 0), pos(3, 3)).unwrap();
        // Diagonal should be 3 steps (diagonal each time).
        assert_eq!(path.len(), 3);
        assert_eq!(*path.back().unwrap(), pos(3, 3));
    }

    #[test]
    fn diagonal_blocked_by_wall_corner() {
        // Grid with wall at (1,0) — can't cut diagonally from (0,0) to (1,1)
        let map = map_from_walkable(&[pos(0, 0), pos(0, 1), pos(1, 1)]);
        // (1,0) is NOT walkable, so diagonal from (0,0) to (1,1) is blocked.
        let path = astar(&map, pos(0, 0), pos(1, 1)).unwrap();
        // Must go cardinal: (0,0) → (0,1) → (1,1)
        assert_eq!(path.len(), 2);
    }

    #[test]
    fn path_excludes_start() {
        let map = map_from_walkable(&[pos(0, 0), pos(1, 0)]);
        let path = astar(&map, pos(0, 0), pos(1, 0)).unwrap();
        assert_eq!(path.len(), 1);
        assert_eq!(path[0], pos(1, 0));
    }

    #[test]
    fn validate_navmesh_connected() {
        let map = open_grid(10, 10);
        let entrances = vec![pos(0, 0), pos(5, 5), pos(9, 9)];
        assert!(validate_navmesh(&map, &entrances).is_ok());
    }

    #[test]
    fn validate_navmesh_disconnected() {
        // Two disconnected islands.
        let mut tiles = HashMap::new();
        tiles.insert(pos(0, 0), TileType::Street);
        tiles.insert(pos(10, 10), TileType::Street);
        let map = WorldMap { tiles };
        let entrances = vec![pos(0, 0), pos(10, 10)];
        assert!(validate_navmesh(&map, &entrances).is_err());
    }

    #[test]
    fn bfs_wrapper_works() {
        let map = open_grid(5, 5);
        let path = bfs(&map, pos(0, 0), pos(4, 4)).unwrap();
        assert_eq!(*path.back().unwrap(), pos(4, 4));
    }
}
