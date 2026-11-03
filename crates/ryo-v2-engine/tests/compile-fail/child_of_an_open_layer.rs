// A child exists only for a sealed layer.
use ryo_v2_engine::{EngineDb, OpenLayer};

fn misuse<Db: EngineDb>(open: &OpenLayer<Db>) {
    let _child = open.child();
}

fn main() {}
