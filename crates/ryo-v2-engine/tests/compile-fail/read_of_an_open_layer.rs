// A read exists only for a sealed layer.
use ryo_v2_engine::{EngineDb, OpenLayer};

fn misuse<Db: EngineDb>(open: &OpenLayer<Db>) {
    let _ = open.read(|_db| ());
}

fn main() {}
