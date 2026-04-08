use bevy::prelude::*;
use std::collections::HashMap;

pub const MAP_WIDTH: i32 = 100;
pub const MAP_HEIGHT: i32 = 40;

pub struct MapPlugin;

impl Plugin for MapPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TileInventory>()
            .add_systems(Startup, init_map);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Component)]
pub struct GridPos {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileType {
    Street,
    Sidewalk,
    Building,
    Park,
    Entrance,
}

impl TileType {
    pub fn is_walkable(&self) -> bool {
        matches!(
            self,
            TileType::Street | TileType::Sidewalk | TileType::Park | TileType::Entrance
        )
    }
}

/// Unique key for tile inventory lookups.
pub fn tile_key(x: i32, y: i32) -> String {
    format!("{}_{}", x, y)
}

/// Items dropped on the ground at specific tile positions.
#[derive(Resource, Default)]
pub struct TileInventory {
    /// tile_key → Vec<item name string>
    pub items: HashMap<String, Vec<String>>,
}

impl TileInventory {
    pub fn drop_item(&mut self, x: i32, y: i32, item: String) {
        self.items.entry(tile_key(x, y)).or_default().push(item);
    }

    pub fn take_item(&mut self, x: i32, y: i32, item: &str) -> bool {
        if let Some(items) = self.items.get_mut(&tile_key(x, y)) {
            if let Some(pos) = items.iter().position(|i| i == item) {
                items.remove(pos);
                if items.is_empty() {
                    self.items.remove(&tile_key(x, y));
                }
                return true;
            }
        }
        false
    }

    #[allow(dead_code)]
    pub fn items_at(&self, x: i32, y: i32) -> &[String] {
        self.items
            .get(&tile_key(x, y))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }
}

#[derive(Resource)]
pub struct WorldMap {
    pub tiles: HashMap<GridPos, TileType>,
}

impl WorldMap {
    pub fn is_walkable(&self, pos: &GridPos) -> bool {
        self.tiles.get(pos).is_some_and(|t| t.is_walkable())
    }

    pub fn walkable_positions(&self) -> Vec<GridPos> {
        self.tiles
            .iter()
            .filter(|(_, t)| t.is_walkable())
            .map(|(p, _)| *p)
            .collect()
    }
}

/// A building definition for the city layout.
pub struct BuildingDef {
    pub name: &'static str,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Which side the entrance is on (relative to footprint).
    pub entrance: EntranceSide,
}

pub enum EntranceSide {
    /// Door on the bottom edge (south in isometric).
    South,
    /// Door on the left edge (west in isometric).
    West,
}

impl BuildingDef {
    pub fn entrance_pos(&self) -> GridPos {
        match self.entrance {
            EntranceSide::South => GridPos {
                x: self.x + self.w / 2,
                y: self.y + self.h,
            },
            EntranceSide::West => GridPos {
                x: self.x - 1,
                y: self.y + self.h / 2,
            },
        }
    }
}

/// The city layout. Buildings are rectangular footprints on a street grid.
pub fn city_buildings() -> Vec<BuildingDef> {
    vec![
        // === Commercial Row (y: 2..6) ===
        BuildingDef {
            name: "cafe",
            x: 2,
            y: 2,
            w: 5,
            h: 4,
            entrance: EntranceSide::South,
        },
        BuildingDef {
            name: "google",
            x: 10,
            y: 2,
            w: 6,
            h: 4,
            entrance: EntranceSide::South,
        },
        BuildingDef {
            name: "market",
            x: 20,
            y: 2,
            w: 6,
            h: 4,
            entrance: EntranceSide::South,
        },
        // === Industrial Row (y: 12..16) ===
        BuildingDef {
            name: "warehouse",
            x: 2,
            y: 12,
            w: 8,
            h: 4,
            entrance: EntranceSide::South,
        },
        BuildingDef {
            name: "hotel",
            x: 14,
            y: 12,
            w: 5,
            h: 4,
            entrance: EntranceSide::South,
        },
        // === Residential — far east ===
        BuildingDef {
            name: "apartments",
            x: 80,
            y: 18,
            w: 6,
            h: 5,
            entrance: EntranceSide::West,
        },
        // === Medical (y: 22..26) ===
        BuildingDef {
            name: "hospital",
            x: 2,
            y: 22,
            w: 5,
            h: 4,
            entrance: EntranceSide::South,
        },
        // === Library (y: 22..26) ===
        BuildingDef {
            name: "library",
            x: 14,
            y: 22,
            w: 5,
            h: 4,
            entrance: EntranceSide::South,
        },
    ]
}

/// Generate the city map from building definitions.
pub fn init_map(mut commands: Commands) {
    let mut tiles = HashMap::new();
    let buildings = city_buildings();

    // Fill everything with streets first.
    for y in 0..MAP_HEIGHT {
        for x in 0..MAP_WIDTH {
            tiles.insert(GridPos { x, y }, TileType::Street);
        }
    }

    // Lay down sidewalks (1 tile border around each building).
    for bld in &buildings {
        for x in (bld.x - 1)..=(bld.x + bld.w) {
            for y in (bld.y - 1)..=(bld.y + bld.h) {
                let pos = GridPos { x, y };
                if x >= 0 && x < MAP_WIDTH && y >= 0 && y < MAP_HEIGHT {
                    if let Some(tile) = tiles.get_mut(&pos) {
                        if *tile == TileType::Street {
                            *tile = TileType::Sidewalk;
                        }
                    }
                }
            }
        }
    }

    // Stamp building footprints (blocked).
    for bld in &buildings {
        for x in bld.x..(bld.x + bld.w) {
            for y in bld.y..(bld.y + bld.h) {
                tiles.insert(GridPos { x, y }, TileType::Building);
            }
        }
    }

    // Mark entrances (walkable tiles at building doors).
    for bld in &buildings {
        let entrance = bld.entrance_pos();
        if entrance.x >= 0 && entrance.x < MAP_WIDTH && entrance.y >= 0 && entrance.y < MAP_HEIGHT {
            tiles.insert(entrance, TileType::Entrance);
        }
    }

    // Central park (y: 33..38, x: 14..26).
    for x in 14..26 {
        for y in 33..38 {
            tiles.insert(GridPos { x, y }, TileType::Park);
        }
    }

    // Bounty board sits on the park edge.
    tiles.insert(GridPos { x: 20, y: 32 }, TileType::Sidewalk);

    let map = WorldMap { tiles };

    // Validate navmesh: all building entrances must be reachable from each other.
    let entrances: Vec<GridPos> = buildings.iter().map(|b| b.entrance_pos()).collect();
    let board_pos = GridPos { x: 20, y: 32 };
    let mut all_entrances = entrances;
    all_entrances.push(board_pos);

    match crate::agents::pathfinding::validate_navmesh(&map, &all_entrances) {
        Ok(()) => {
            tracing::info!(
                "NavMesh validated: all {} entrances are reachable",
                all_entrances.len()
            );
        }
        Err(unreachable) => {
            for (from, to) in &unreachable {
                tracing::error!(
                    "NAVMESH ERROR: ({},{}) cannot reach ({},{})",
                    from.x,
                    from.y,
                    to.x,
                    to.y,
                );
            }
            panic!(
                "NavMesh validation failed: {} unreachable pairs",
                unreachable.len()
            );
        }
    }

    commands.insert_resource(map);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a WorldMap using the same logic as init_map, but without bevy Commands.
    fn build_test_map() -> WorldMap {
        let mut tiles = HashMap::new();
        let buildings = city_buildings();

        for y in 0..MAP_HEIGHT {
            for x in 0..MAP_WIDTH {
                tiles.insert(GridPos { x, y }, TileType::Street);
            }
        }

        for bld in &buildings {
            for x in (bld.x - 1)..=(bld.x + bld.w) {
                for y in (bld.y - 1)..=(bld.y + bld.h) {
                    let pos = GridPos { x, y };
                    if x >= 0 && x < MAP_WIDTH && y >= 0 && y < MAP_HEIGHT {
                        if let Some(tile) = tiles.get_mut(&pos) {
                            if *tile == TileType::Street {
                                *tile = TileType::Sidewalk;
                            }
                        }
                    }
                }
            }
        }

        for bld in &buildings {
            for x in bld.x..(bld.x + bld.w) {
                for y in bld.y..(bld.y + bld.h) {
                    tiles.insert(GridPos { x, y }, TileType::Building);
                }
            }
        }

        for bld in &buildings {
            let entrance = bld.entrance_pos();
            if entrance.x >= 0
                && entrance.x < MAP_WIDTH
                && entrance.y >= 0
                && entrance.y < MAP_HEIGHT
            {
                tiles.insert(entrance, TileType::Entrance);
            }
        }

        for x in 14..26 {
            for y in 33..38 {
                tiles.insert(GridPos { x, y }, TileType::Park);
            }
        }

        tiles.insert(GridPos { x: 20, y: 32 }, TileType::Sidewalk);

        WorldMap { tiles }
    }

    #[test]
    fn building_footprints_not_walkable() {
        let map = build_test_map();
        for bld in &city_buildings() {
            for x in bld.x..(bld.x + bld.w) {
                for y in bld.y..(bld.y + bld.h) {
                    let pos = GridPos { x, y };
                    // Skip entrance tiles (which overlap the footprint edge).
                    let entrance = bld.entrance_pos();
                    if pos == entrance {
                        continue;
                    }
                    assert!(
                        !map.is_walkable(&pos),
                        "Building '{}' tile ({},{}) should not be walkable",
                        bld.name,
                        x,
                        y,
                    );
                }
            }
        }
    }

    #[test]
    fn entrances_are_walkable() {
        let map = build_test_map();
        for bld in &city_buildings() {
            let entrance = bld.entrance_pos();
            assert!(
                map.is_walkable(&entrance),
                "Entrance for '{}' at ({},{}) should be walkable",
                bld.name,
                entrance.x,
                entrance.y,
            );
        }
    }

    #[test]
    fn street_tiles_are_walkable() {
        let map = build_test_map();
        // Check a few corners that should remain street.
        let street_positions = vec![
            GridPos { x: 0, y: 0 },
            GridPos { x: MAP_WIDTH - 1, y: 0 },
            GridPos { x: 0, y: MAP_HEIGHT - 1 },
            GridPos { x: MAP_WIDTH - 1, y: MAP_HEIGHT - 1 },
        ];
        for pos in &street_positions {
            assert!(
                map.is_walkable(pos),
                "Street tile ({},{}) should be walkable",
                pos.x,
                pos.y,
            );
        }
    }

    #[test]
    fn tile_type_walkability() {
        assert!(TileType::Street.is_walkable());
        assert!(TileType::Sidewalk.is_walkable());
        assert!(TileType::Park.is_walkable());
        assert!(TileType::Entrance.is_walkable());
        assert!(!TileType::Building.is_walkable());
    }

    #[test]
    fn out_of_bounds_not_walkable() {
        let map = build_test_map();
        assert!(!map.is_walkable(&GridPos { x: -1, y: 0 }));
        assert!(!map.is_walkable(&GridPos {
            x: 0,
            y: MAP_HEIGHT
        }));
    }
}
