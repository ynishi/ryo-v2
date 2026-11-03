// A layer cannot be sealed while its database guard is borrowed.
use ryo_v2_engine::{EngineDb, OpenLayer};

fn misuse<Db: EngineDb>(open: OpenLayer<Db>) {
    let db = open.db();
    let _sealed = open.seal();
    drop(db);
}

fn main() {}
