pub mod releases;
mod settings;

pub use settings::init_settings;

pub const RELEASES_REPOSITORY: &str = "somtooo/zed";
pub const RELEASE_TAG: Option<&str> = option_env!("ENDO_RELEASE_TAG");
