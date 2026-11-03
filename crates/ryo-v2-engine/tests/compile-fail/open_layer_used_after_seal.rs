// `seal` consumes the open layer: nothing writes to it afterwards.
use ryo_v2_engine::{EngineDb, OpenLayer};

fn misuse<Db: EngineDb>(mut open: OpenLayer<Db>, file: Db::File) {
    let _sealed = open.seal();
    open.write(file, "after the seal");
}

fn main() {}
