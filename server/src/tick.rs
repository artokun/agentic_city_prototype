use bevy::prelude::*;
use tracing::info;

pub struct GameTickPlugin;

impl Plugin for GameTickPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TickCount>()
            .add_systems(Update, tick_system);
    }
}

#[derive(Resource, Default)]
pub struct TickCount(pub u64);

fn tick_system(mut tick: ResMut<TickCount>) {
    tick.0 += 1;
    if tick.0 % 100 == 0 {
        info!("tick {}", tick.0);
    }
}
