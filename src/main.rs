#[macro_use]
extern crate lazy_static;

mod config;
mod container;
mod language;
mod protocol;
mod logger;

pub const CONFIG_FILE: &'static str = "config.toml";

use std::fs::read_to_string;

use log::*;

use config::Config;
use language::Languages;
use protocol::open_protocol;

lazy_static! {
    static ref CONFIG: Config = {
        let s = read_to_string(CONFIG_FILE).expect("Some error occured");
        info!("Loaded PMS slave config file");
        toml::from_str(&s).expect("Some error occured")
    };
    static ref LANGUAGES: Languages = Languages::load().expect("Some error occured");
}

static LOGGER: logger::StdoutLogger = logger::StdoutLogger;

#[async_std::main]
async fn main() {
    log::set_logger(&LOGGER).map(|()| log::set_max_level(LevelFilter::Info));
    info!("pms-slave {}", env!("CARGO_PKG_VERSION"));
    open_protocol().await
}
