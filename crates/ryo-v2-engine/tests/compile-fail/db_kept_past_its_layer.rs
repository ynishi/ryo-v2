// The database guard cannot outlive its layer.
use ryo_v2_engine::{EngineDb, LayerDb, SealedLayer};

fn misuse<Db: EngineDb>(sealed: &SealedLayer<Db>) -> LayerDb<'static, Db> {
    let open = sealed.child();
    open.db()
}

fn main() {}
