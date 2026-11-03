// Only an open layer lends a database handle.
use ryo_v2_engine::{EngineDb, SealedLayer};

fn misuse<Db: EngineDb>(sealed: &SealedLayer<Db>) {
    let _db = sealed.db();
}

fn main() {}
