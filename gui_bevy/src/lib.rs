use bevy::{app::App, winit::WinitSettings, DefaultPlugins};

pub fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        // Power-saving reactive rendering for applications.
        //
        // Reference: https://github.com/bevyengine/bevy/blob/main/examples/window/low_power.rs
        .insert_resource(WinitSettings::desktop_app())
        .run();
}
