use bevy::prelude::*;
use std::collections::HashMap;

use crate::agents::components::*;
use crate::agents::needs::Needs;
use crate::items::{Inventory, ItemType};
use crate::tick::TickCount;
use crate::world::map::GridPos;
use crate::world::structures::{Entrance, SpriteType, StructureId};

// --- Known locations (fog of war) ---

/// What locations this agent knows about. Starts with just the bounty board.
/// Agents discover new locations by looking around, exploring, or asking others.
#[derive(Component, Debug)]
pub struct KnownLocations {
    pub locations: HashMap<Entity, KnownPlace>,
}

impl Default for KnownLocations {
    fn default() -> Self {
        Self {
            locations: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct KnownPlace {
    pub name: String,
    pub pos: GridPos,
    pub entrance: GridPos,
    pub discovered_tick: u64,
    pub source: DiscoverySource,
}

#[derive(Debug, Clone)]
pub enum DiscoverySource {
    /// Agent started knowing this (bounty board).
    Initial,
    /// Agent saw it while looking around.
    Spotted,
    /// Another agent told them about it.
    ToldBy(String),
    /// Agent walked past it.
    Visited,
}

// --- Constants ---

pub const LOOK_RADIUS: i32 = 8;
pub const LOOK_DURATION: u32 = 5;
pub const TRACK_RANGE: i32 = 10;
pub const INSPECT_RANGE: i32 = 1;
pub const INSPECT_DURATION: u32 = 3;

// --- Vision (result of looking around) ---

#[derive(Debug, Clone)]
pub struct VisibleEntity {
    pub entity: Entity,
    pub pos: GridPos,
    pub kind: EntityKind,
    pub name: String,
    pub distance: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityKind {
    Agent,
    Structure,
}

/// What the agent currently sees. Updated by look_around action.
#[derive(Component, Default, Debug)]
pub struct Vision {
    pub visible: Vec<VisibleEntity>,
    pub last_scan_tick: u64,
}

// --- Tracking ---

#[derive(Debug, Clone)]
pub struct TrackedTarget {
    pub name: String,
    pub last_known_pos: GridPos,
    pub in_range: bool,
    pub kind: EntityKind,
}

/// Entities this agent is actively tracking. Updated every tick if in range.
#[derive(Component, Default, Debug)]
pub struct Tracking {
    pub targets: HashMap<Entity, TrackedTarget>,
}

// --- Inspection result ---

#[derive(Debug, Clone)]
pub enum InspectionData {
    Agent {
        name: String,
        gold: u32,
        goal: String,
        energy: f32,
        hunger: f32,
        boredom: f32,
        inventory: Vec<(String, u32)>,
    },
    Structure {
        name: String,
        inventory: Vec<(String, u32)>,
        has_entrance: bool,
    },
}

#[derive(Debug, Clone)]
pub struct Inspection {
    pub target: Entity,
    pub tick: u64,
    pub data: InspectionData,
}

/// Log of inspections this agent has performed.
#[derive(Component, Default, Debug)]
pub struct InspectionLog {
    pub inspections: Vec<Inspection>,
}

// --- Pending actions ---

/// Queued: agent wants to look around.
#[derive(Component)]
pub struct WantsToLook;

/// Queued: agent wants to start tracking an entity.
#[derive(Component)]
pub struct WantsToTrack(pub Entity);

/// Queued: agent wants to inspect an adjacent entity.
#[derive(Component)]
pub struct WantsToInspect(pub Entity);

// --- Systems ---

fn manhattan(a: &GridPos, b: &GridPos) -> i32 {
    (a.x - b.x).abs() + (a.y - b.y).abs()
}

/// System: perform the look-around scan. Populates Vision and discovers locations.
pub fn look_around_system(
    mut commands: Commands,
    tick: Res<TickCount>,
    tile_inventory: Res<crate::world::map::TileInventory>,
    mut lookers: Query<(Entity, &AgentName, &GridPos, &mut Vision, &mut KnownLocations), With<WantsToLook>>,
    agents: Query<(Entity, &AgentName, &GridPos), Without<WantsToLook>>,
    structures: Query<(Entity, &SpriteType, &GridPos, &Entrance), With<StructureId>>,
) {
    for (looker_entity, looker_name, looker_pos, mut vision, mut known) in &mut lookers {
        let mut visible = Vec::new();

        for (entity, name, pos) in &agents {
            let dist = manhattan(looker_pos, pos);
            if dist <= LOOK_RADIUS && entity != looker_entity {
                visible.push(VisibleEntity {
                    entity,
                    pos: *pos,
                    kind: EntityKind::Agent,
                    name: name.0.clone(),
                    distance: dist,
                });
            }
        }

        for (entity, sprite, pos, entrance) in &structures {
            let dist = manhattan(looker_pos, pos);
            if dist <= LOOK_RADIUS {
                visible.push(VisibleEntity {
                    entity,
                    pos: *pos,
                    kind: EntityKind::Structure,
                    name: sprite.0.clone(),
                    distance: dist,
                });

                // Discover this location if new.
                if !known.locations.contains_key(&entity) {
                    known.locations.insert(entity, KnownPlace {
                        name: sprite.0.clone(),
                        pos: *pos,
                        entrance: entrance.0,
                        discovered_tick: tick.0,
                        source: DiscoverySource::Spotted,
                    });
                    tracing::info!("{} discovered {} by looking around", looker_name.0, sprite.0);
                }
            }
        }

        // Scan tile inventory for dropped items within range.
        for (key, items) in &tile_inventory.items {
            let parts: Vec<&str> = key.split('_').collect();
            if parts.len() == 2 {
                if let (Ok(x), Ok(y)) = (parts[0].parse::<i32>(), parts[1].parse::<i32>()) {
                    let tile_pos = GridPos { x, y };
                    let dist = manhattan(looker_pos, &tile_pos);
                    if dist <= LOOK_RADIUS {
                        for item_name in items {
                            visible.push(VisibleEntity {
                                entity: looker_entity, // placeholder entity
                                pos: tile_pos,
                                kind: EntityKind::Structure, // display as structure for now
                                name: format!("[ground] {}", item_name),
                                distance: dist,
                            });
                        }
                    }
                }
            }
        }

        visible.sort_by_key(|v| v.distance);

        tracing::info!(
            "{} scanned: {} entities visible, {} locations known",
            looker_name.0, visible.len(), known.locations.len(),
        );

        vision.visible = visible;
        vision.last_scan_tick = tick.0;

        commands.entity(looker_entity).remove::<WantsToLook>();
    }
}

/// System: passive discovery — agents discover structures they walk past.
pub fn passive_discovery_system(
    tick: Res<TickCount>,
    mut agents: Query<(&AgentName, &GridPos, &mut KnownLocations), Changed<GridPos>>,
    structures: Query<(Entity, &SpriteType, &GridPos, &Entrance), With<StructureId>>,
) {
    for (name, agent_pos, mut known) in &mut agents {
        for (entity, sprite, pos, entrance) in &structures {
            if known.locations.contains_key(&entity) {
                continue;
            }
            let dist = manhattan(agent_pos, pos);
            if dist <= 3 {
                known.locations.insert(entity, KnownPlace {
                    name: sprite.0.clone(),
                    pos: *pos,
                    entrance: entrance.0,
                    discovered_tick: tick.0,
                    source: DiscoverySource::Visited,
                });
                tracing::info!("{} discovered {} while passing by", name.0, sprite.0);
            }
        }
    }
}

/// System: update tracked entity positions every tick.
pub fn tracking_update_system(
    mut trackers: Query<(&GridPos, &mut Tracking)>,
    all_positions: Query<&GridPos, Without<Tracking>>,
) {
    for (tracker_pos, mut tracking) in &mut trackers {
        for (entity, target) in tracking.targets.iter_mut() {
            if let Ok(pos) = all_positions.get(*entity) {
                let dist = manhattan(tracker_pos, pos);
                target.in_range = dist <= TRACK_RANGE;
                if target.in_range {
                    target.last_known_pos = *pos;
                }
            } else {
                target.in_range = false;
            }
        }
    }
}

/// System: handle track requests.
pub fn start_tracking_system(
    mut commands: Commands,
    mut trackers: Query<(Entity, &mut Tracking, &WantsToTrack)>,
    agents: Query<(&AgentName, &GridPos)>,
    structures: Query<(&SpriteType, &GridPos), With<StructureId>>,
) {
    for (entity, mut tracking, wants) in &mut trackers {
        let target = wants.0;

        if let Ok((name, pos)) = agents.get(target) {
            tracking.targets.insert(target, TrackedTarget {
                name: name.0.clone(),
                last_known_pos: *pos,
                in_range: true,
                kind: EntityKind::Agent,
            });
            tracing::info!("Now tracking agent {}", name.0);
        } else if let Ok((sprite, pos)) = structures.get(target) {
            tracking.targets.insert(target, TrackedTarget {
                name: sprite.0.clone(),
                last_known_pos: *pos,
                in_range: true,
                kind: EntityKind::Structure,
            });
            tracing::info!("Now tracking structure {}", sprite.0);
        }

        commands.entity(entity).remove::<WantsToTrack>();
    }
}

/// System: handle inspect requests. Agent must be within 1 tile.
pub fn inspect_system(
    mut commands: Commands,
    tick: Res<TickCount>,
    mut inspectors: Query<(Entity, &GridPos, &mut InspectionLog, &WantsToInspect)>,
    agent_data: Query<(&AgentName, &GridPos, &Inventory, &AgentGoal, &Needs)>,
    structure_data: Query<(&SpriteType, &GridPos, &Inventory, Option<&Entrance>), With<StructureId>>,
) {
    for (inspector, inspector_pos, mut log, wants) in &mut inspectors {
        let target = wants.0;

        // Try as agent.
        if let Ok((name, pos, inv, goal, needs)) = agent_data.get(target) {
            if manhattan(inspector_pos, pos) <= INSPECT_RANGE {
                let items: Vec<_> = inv.items.iter()
                    .map(|(t, c)| (t.to_string(), *c))
                    .collect();

                log.inspections.push(Inspection {
                    target,
                    tick: tick.0,
                    data: InspectionData::Agent {
                        name: name.0.clone(),
                        gold: inv.count(ItemType::GoldCoin),
                        goal: format!("{:?}", goal),
                        energy: needs.energy,
                        hunger: needs.hunger,
                        boredom: needs.boredom,
                        inventory: items,
                    },
                });
                tracing::info!("Inspected agent {}", name.0);
            }
        }

        // Try as structure.
        if let Ok((sprite, pos, inv, entrance)) = structure_data.get(target) {
            if manhattan(inspector_pos, pos) <= INSPECT_RANGE + 5 {
                // Structures are bigger, allow slightly more range.
                let items: Vec<_> = inv.items.iter()
                    .map(|(t, c)| (t.to_string(), *c))
                    .collect();

                log.inspections.push(Inspection {
                    target,
                    tick: tick.0,
                    data: InspectionData::Structure {
                        name: sprite.0.clone(),
                        inventory: items,
                        has_entrance: entrance.is_some(),
                    },
                });
                tracing::info!("Inspected structure {}", sprite.0);
            }
        }

        // Keep log manageable.
        if log.inspections.len() > 30 {
            let drain = log.inspections.len() - 30;
            log.inspections.drain(..drain);
        }

        commands.entity(inspector).remove::<WantsToInspect>();
    }
}
