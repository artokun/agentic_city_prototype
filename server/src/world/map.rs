use bevy::prelude::*;
use std::collections::HashMap;

pub const MAP_WIDTH: i32 = 40;
pub const MAP_HEIGHT: i32 = 40;

pub struct MapPlugin;

impl Plugin for MapPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, init_map);
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
        matches!(self, TileType::Street | TileType::Sidewalk | TileType::Park | TileType::Entrance)
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
        // === Row 1 (y: 2..6) ===
        BuildingDef { name: "cafe",        x: 2,  y: 2,  w: 4, h: 4, entrance: EntranceSide::South },
        BuildingDef { name: "google",      x: 8,  y: 2,  w: 6, h: 4, entrance: EntranceSide::South },
        BuildingDef { name: "office",      x: 16, y: 2,  w: 4, h: 4, entrance: EntranceSide::South },
        BuildingDef { name: "apartments",  x: 22, y: 2,  w: 5, h: 4, entrance: EntranceSide::West },
        BuildingDef { name: "server_room", x: 30, y: 2,  w: 4, h: 4, entrance: EntranceSide::South },

        // === Row 2 (y: 10..14) ===
        BuildingDef { name: "library",     x: 2,  y: 10, w: 4, h: 4, entrance: EntranceSide::South },
        BuildingDef { name: "shop",        x: 8,  y: 10, w: 4, h: 4, entrance: EntranceSide::West },
        BuildingDef { name: "bank",        x: 14, y: 10, w: 5, h: 4, entrance: EntranceSide::South },
        BuildingDef { name: "warehouse",   x: 22, y: 10, w: 6, h: 4, entrance: EntranceSide::West },
        BuildingDef { name: "market",      x: 30, y: 10, w: 5, h: 4, entrance: EntranceSide::South },

        // === Row 3 (y: 18..22) ===
        BuildingDef { name: "gym",         x: 2,  y: 18, w: 4, h: 4, entrance: EntranceSide::South },
        BuildingDef { name: "restaurant",  x: 8,  y: 18, w: 4, h: 4, entrance: EntranceSide::South },
        BuildingDef { name: "post_office", x: 14, y: 18, w: 5, h: 4, entrance: EntranceSide::West },
        BuildingDef { name: "garage",      x: 22, y: 18, w: 4, h: 4, entrance: EntranceSide::South },
        BuildingDef { name: "hotel",       x: 30, y: 18, w: 5, h: 4, entrance: EntranceSide::West },

        // === Row 4 (y: 26..30) ===
        BuildingDef { name: "theater",     x: 2,  y: 26, w: 5, h: 4, entrance: EntranceSide::South },
        BuildingDef { name: "hospital",    x: 9,  y: 26, w: 5, h: 4, entrance: EntranceSide::West },
        BuildingDef { name: "school",      x: 16, y: 26, w: 4, h: 4, entrance: EntranceSide::South },
        BuildingDef { name: "diner",       x: 22, y: 26, w: 4, h: 4, entrance: EntranceSide::South },
        BuildingDef { name: "gas_station", x: 30, y: 26, w: 5, h: 4, entrance: EntranceSide::West },
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

    commands.insert_resource(WorldMap { tiles });
}
