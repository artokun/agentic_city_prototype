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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::map::{GridPos, TileType, WorldMap};
    use std::collections::HashMap;

    fn pos(x: i32, y: i32) -> GridPos {
        GridPos { x, y }
    }

    /// Build a map from a list of walkable positions.
    fn map_from_walkable(positions: &[GridPos]) -> WorldMap {
        let mut tiles = HashMap::new();
        for p in positions {
            tiles.insert(*p, TileType::Sidewalk);
        }
        WorldMap { tiles }
    }

    #[test]
    fn same_position_returns_empty_path() {
        let map = map_from_walkable(&[pos(5, 5)]);
        let path = bfs(&map, pos(5, 5), pos(5, 5)).unwrap();
        assert!(path.is_empty());
    }

    #[test]
    fn straight_line_horizontal() {
        // Walkable corridor from (0,0) to (4,0)
        let tiles: Vec<GridPos> = (0..=4).map(|x| pos(x, 0)).collect();
        let map = map_from_walkable(&tiles);
        let path = bfs(&map, pos(0, 0), pos(4, 0)).unwrap();
        // Path should exclude the start and include the destination
        assert_eq!(path.len(), 4);
        assert_eq!(*path.front().unwrap(), pos(1, 0));
        assert_eq!(*path.back().unwrap(), pos(4, 0));
    }

    #[test]
    fn straight_line_vertical() {
        let tiles: Vec<GridPos> = (0..=3).map(|y| pos(0, y)).collect();
        let map = map_from_walkable(&tiles);
        let path = bfs(&map, pos(0, 0), pos(0, 3)).unwrap();
        assert_eq!(path.len(), 3);
        assert_eq!(*path.back().unwrap(), pos(0, 3));
    }

    #[test]
    fn blocked_path_returns_none() {
        // Two disconnected walkable tiles with a wall between
        let map = map_from_walkable(&[pos(0, 0), pos(2, 0)]);
        assert!(bfs(&map, pos(0, 0), pos(2, 0)).is_none());
    }

    #[test]
    fn destination_not_walkable_returns_none() {
        let map = map_from_walkable(&[pos(0, 0), pos(1, 0)]);
        // (2,0) is not in the map, so not walkable
        assert!(bfs(&map, pos(0, 0), pos(2, 0)).is_none());
    }

    #[test]
    fn routes_around_obstacle() {
        // Grid:
        //   (0,0) (1,0) (2,0)
        //   (0,1)  WALL (2,1)
        //   (0,2) (1,2) (2,2)
        let walkable = vec![
            pos(0, 0), pos(1, 0), pos(2, 0),
            pos(0, 1),            pos(2, 1),
            pos(0, 2), pos(1, 2), pos(2, 2),
        ];
        let map = map_from_walkable(&walkable);
        let path = bfs(&map, pos(0, 0), pos(2, 2)).unwrap();
        // Must go around the wall at (1,1)
        assert!(!path.contains(&pos(1, 1)));
        assert_eq!(*path.back().unwrap(), pos(2, 2));
        // Shortest path is length 4 (e.g. right, right, down, down or down, down, right, right)
        assert_eq!(path.len(), 4);
    }

    #[test]
    fn path_excludes_start() {
        let map = map_from_walkable(&[pos(0, 0), pos(1, 0)]);
        let path = bfs(&map, pos(0, 0), pos(1, 0)).unwrap();
        assert_eq!(path.len(), 1);
        assert_eq!(path[0], pos(1, 0));
    }
}
