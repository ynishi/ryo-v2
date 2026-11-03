//! What the database of a session holds, by salsa ingredient and by tracked function, as salsa
//! itself measures it.

use crate::RaSession;
use ra_ap_base_db::salsa::{self, IngredientInfo};
use ryo_v2_core::app::ports::CheckError;
use serde::Serialize;

/// The composition of a session's database: one row per salsa struct ingredient that holds a
/// slot, and one row per tracked function that holds a memo.
///
/// rust-analyzer declares no heap size for its values, so every `heap_size_of_fields` is `None`:
/// the report gives counts and shallow sizes. A memo whose value the LRU evicted is counted like
/// one that keeps it, so the report cannot see eviction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryReport {
    /// The input, interned and tracked struct ingredients, sorted by name.
    pub ingredients: Vec<IngredientRow>,
    /// The tracked functions, sorted by query name.
    pub memos: Vec<MemoRow>,
}

/// One salsa struct ingredient (an input, an interned or a tracked struct) of a [`MemoryReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IngredientRow {
    /// The ingredient's debug name, as salsa gives it (the struct's name).
    pub name: String,
    /// The number of its slots.
    pub count: u64,
    /// The bytes of the fields of its slots, shallow.
    pub size_of_fields: u64,
    /// The bytes of salsa's metadata of its slots.
    pub size_of_metadata: u64,
    /// The heap bytes the fields own, where the struct declares a heap size.
    pub heap_size_of_fields: Option<u64>,
}

/// One tracked function of a [`MemoryReport`]: its memos over every ingredient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoRow {
    /// The function's debug name, as salsa keys its memos.
    pub query: String,
    /// The number of its memos.
    pub count: u64,
    /// The bytes of its memoized values, shallow.
    pub size_of_fields: u64,
    /// The bytes of salsa's metadata of its memos.
    pub size_of_metadata: u64,
    /// The heap bytes the values own, where the value type declares a heap size.
    pub heap_size_of_fields: Option<u64>,
}

/// Bytes as salsa counts them, widened.
fn bytes(n: usize) -> u64 {
    n as u64
}

impl RaSession {
    /// The composition of the database, from salsa's own measurement (`memory_usage`).
    ///
    /// The measurement takes the database as a write does (it drains every other handle), so it
    /// is refused while a fork is live.
    ///
    /// # Contract
    /// - C1: with no fork live, the report has one row per salsa ingredient of the database that
    ///   has at least one slot, and one row per tracked function that has at least one memo; a
    ///   count is the number of slots or memos, and the sizes are those salsa reports.
    ///   tested: tests/memory.rs memory_report_lists_ingredients_and_memos
    /// - C2: the report changes no observable state: a `Verifier::check` of the same edits gives
    ///   the same findings before and after it, and `Verifier::text` and `baseline` are unchanged.
    ///   tested: tests/memory.rs memory_report_changes_nothing
    /// - C3: while a fork is live it is `ForkInUse` and changes nothing; on a poisoned session it
    ///   is `Poisoned`.
    ///   tested: tests/memory.rs memory_report_refuses_while_a_fork_is_live
    pub fn memory_report(&mut self) -> Result<MemoryReport, CheckError> {
        if let Some(reason) = &self.poisoned {
            return Err(CheckError::Poisoned(reason.clone()));
        }
        if self.live.is_some() {
            return Err(CheckError::ForkInUse);
        }
        let db: &mut dyn salsa::Database = self.host.raw_database_mut();
        let info = db.memory_usage();
        let mut ingredients: Vec<IngredientRow> = info
            .structs
            .iter()
            .filter(|i| i.count() > 0)
            .map(|i| IngredientRow {
                name: i.debug_name().to_owned(),
                count: bytes(i.count()),
                size_of_fields: bytes(i.size_of_fields()),
                size_of_metadata: bytes(i.size_of_metadata()),
                heap_size_of_fields: i.heap_size_of_fields().map(bytes),
            })
            .collect();
        ingredients.sort_by(|a, b| a.name.cmp(&b.name));
        let mut memos: Vec<MemoRow> = info
            .queries
            .iter()
            .filter(|(_, i)| i.count() > 0)
            .map(|(query, i): (&&str, &IngredientInfo)| MemoRow {
                query: (*query).to_owned(),
                count: bytes(i.count()),
                size_of_fields: bytes(i.size_of_fields()),
                size_of_metadata: bytes(i.size_of_metadata()),
                heap_size_of_fields: i.heap_size_of_fields().map(bytes),
            })
            .collect();
        memos.sort_by(|a, b| a.query.cmp(&b.query));
        Ok(MemoryReport { ingredients, memos })
    }
}
