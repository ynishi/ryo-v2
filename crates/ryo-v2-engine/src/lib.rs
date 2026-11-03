//! `ryo-v2-ra` is the adapter that holds ryo-v2's verification rules (D1, D2, D7, D9) over
//! rust-analyzer; `ryo-v2-engine` is the fork mechanism, with no ryo-v2 type (the frozen base, the
//! layer tree and writes into a layer); so the dependence on the patched salsa and its unsafe
//! ledger stays in one place.
//!
//! The Engine of D11: forks of one analysed database, several alive at once, as layers over a
//! frozen base.
//!
//! - [`Engine::freeze`] takes a loaded database and freezes it: from then on it is the *base*, which
//!   nothing writes. [`Frozen::root`] is the base seen as a sealed layer.
//! - [`SealedLayer::child`] opens a layer on a sealed one. An [`OpenLayer`] takes writes
//!   ([`OpenLayer::write`]) and lends a database handle that sees the chain of layers below it
//!   ([`OpenLayer::db`]). [`OpenLayer::seal`] turns it into a [`SealedLayer`], which takes no
//!   write and may have children; a query on a sealed layer runs in a throwaway child
//!   ([`SealedLayer::read`]).
//! - [`Frozen::thaw`] gives the database back once no layer and no other handle of it lives.
//!
//! Sealing is a type: only an open layer lends a database, only a sealed layer has children and
//! reads, and an open layer is consumed by its seal. The base brand is structural: every layer
//! holds the base it descends from, and every operation that combines a base and a layer is a
//! method of the layer, so no call can pair a layer with another base.
//!
//! Stage 2: the Engine is bound to any salsa database through [`EngineDb`] (a file input with a
//! text), and implements it for rust-analyzer's `RootDatabase`: a fork is a `RootDatabase` handle
//! with a layer; a modified file keeps its `FileText` input and gets its new text in the layer;
//! a created file takes the vacant path of rust-analyzer's `Files` map (a new input under the
//! layer, no write lock on the base). Tracked functions, tracked structs and interned values run
//! under a layer, with their memos, stamps and re-created structs held by the layer.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::fmt;
use std::hash::Hash;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ra_ap_base_db::{FileId, FileText, SourceDatabase};
use ra_ap_ide_db::RootDatabase;
use salsa::Revision;
use salsa::fork::{self, BaseFingerprint, Layer, SharedWrites, SkippedWrites};
use salsa::plumbing::HasStorage;

/// What the Engine needs of a database: a salsa database with a file input whose text a layer
/// can set.
pub trait EngineDb: salsa::Database + HasStorage + Clone + 'static {
    /// The input that holds one file's text.
    type File: Copy + Eq + Hash + fmt::Debug + Send + Sync + 'static;

    /// The text of `file` as this handle sees it (through its layer's chain, if it has one).
    fn file_text(&self, file: Self::File) -> Arc<str>;

    /// Sets the text of `file` in the layer this handle carries
    /// (`salsa::fork::set_input_field`).
    fn set_file_text_in_layer(&self, file: Self::File, text: Arc<str>);

    /// Creates `file`, which the database does not know, with `text`, as an input created under
    /// the layer this handle carries (a counted shared write of the base). Takes no write lock:
    /// it never waits for another handle.
    fn create_file_in_layer(&mut self, file: Self::File, text: Arc<str>);
}

/// rust-analyzer's database: a file is a `vfs::FileId`, its text the `FileText` input that
/// `Files` maps it to.
impl EngineDb for RootDatabase {
    type File = FileId;

    fn file_text(&self, file: FileId) -> Arc<str> {
        Arc::from(&**SourceDatabase::file_text(self, file).text(self))
    }

    fn set_file_text_in_layer(&self, file: FileId, text: Arc<str>) {
        let input = SourceDatabase::file_text(self, file);
        let text = triomphe::Arc::<str>::from(&*text);
        fork::set_input_field::<FileText, _>(self, input, 0, |fields| fields.0 = text);
    }

    fn create_file_in_layer(&mut self, file: FileId, text: Arc<str>) {
        // The vacant entry of `Files` creates the input (`FileText::new`) and inserts it into the
        // map, which every handle of the base shares (a listed shared write, D11 E2). An occupied
        // entry would run a setter, which panics on a layered handle instead of waiting.
        SourceDatabase::set_file_text(self, file, &text);
    }
}

/// What the layers of a base did to it: the listed shared writes, the base writes they reached
/// and did not make, the refused writes (each a panic, which a caller may have caught), and the
/// fingerprint of what the base keeps (D11, I2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Isolation {
    /// The shared writes of the listed kinds.
    pub shared: SharedWrites,
    /// The base writes reached and not made.
    pub skipped: SkippedWrites,
    /// The writes to the frozen base that were refused. 0 is the isolation gate.
    pub refused: usize,
    /// The base's memos, revisions and `updated_at`s, slot by slot.
    pub fingerprint: BaseFingerprint,
}

/// Why an Engine operation refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// A handle the operation needs gone is still alive: a clone of a layer's database, a layer
    /// of the base, or another handle of the base. The operation changed nothing and did not wait.
    #[error("a database handle or layer that must be gone is still alive")]
    InUse,
    /// A write into an open layer whose database was already lent ([`OpenLayer::db`]): a layer
    /// has one revision, and a query that ran on it, in salsa or in a cache outside it keyed by
    /// the layer's revision (rust-analyzer's trait solver), would look current after the write.
    /// The layer changed nothing; write into a child of it once it is sealed.
    #[error("a write into a layer whose database was already lent")]
    Lent,
}

/// The references to a layer that its own open layer holds: the open layer's and its
/// database's. Any other reference is a live clone of the layer's database.
const OWN_REFERENCES: usize = 2;

/// The frozen base and what every layer of it shares.
struct Base<Db: EngineDb> {
    /// The base handle; a layer's database is a clone of it with the layer set.
    db: Mutex<Db>,
    /// The base seen as a sealed, write-free layer.
    root: Arc<Node<Db::File>>,
    /// The base's revision at the freeze.
    revision: Revision,
    /// The files a layer of this base created. The input a creation makes is shared by every
    /// layer of the base, so a later creation of the same file, in a sibling or a descendant,
    /// sets the text of that input in its own layer instead of creating it again.
    created: Mutex<HashSet<Db::File>>,
}

impl<Db: EngineDb> Base<Db> {
    fn lock(&self) -> MutexGuard<'_, Db> {
        self.db.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn created(&self) -> MutexGuard<'_, HashSet<Db::File>> {
        self.created.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A handle of the base carrying `layer`.
    fn handle(&self, layer: &Arc<Layer>) -> Db {
        let mut db = self.lock().clone();
        fork::set_layer(&mut db, Arc::clone(layer));
        db
    }
}

/// A sealed layer of the tree: its salsa layer, its parent, the files it wrote.
struct Node<F> {
    layer: Arc<Layer>,
    parent: Option<Arc<Node<F>>>,
    files: Vec<F>,
}

/// The Engine's entry point.
#[derive(Debug)]
pub struct Engine;

impl Engine {
    /// Freezes `db` and takes it as the base: it is given back by [`Frozen::thaw`].
    ///
    /// # Contract
    /// - C1: every layer made after `freeze` has a revision above the base's revision at the
    ///   freeze, also when the base was thawed and written since an earlier freeze; so a layer's
    ///   write is newer than every memo of the base.
    ///   tested: tests/layers.rs refreeze_after_base_writes_keeps_layers_above_the_base
    ///
    /// # Panics
    ///
    /// If `db` carries a layer, another handle of its base (a clone) is alive, or its base is
    /// already frozen.
    pub fn freeze<Db: EngineDb>(db: Db) -> Frozen<Db> {
        let revision = fork::freeze(&db);
        let root = fork::root_layer(&db);
        root.seal();
        Frozen {
            base: Arc::new(Base {
                db: Mutex::new(db),
                root: Arc::new(Node {
                    layer: root,
                    parent: None,
                    files: Vec::new(),
                }),
                revision,
                created: Mutex::new(HashSet::new()),
            }),
        }
    }
}

/// A frozen base: the database [`Engine::freeze`] took, until [`Frozen::thaw`].
pub struct Frozen<Db: EngineDb> {
    base: Arc<Base<Db>>,
}

impl<Db: EngineDb> fmt::Debug for Frozen<Db> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frozen")
            .field("revision", &self.base.revision)
            .finish_non_exhaustive()
    }
}

impl<Db: EngineDb> Frozen<Db> {
    /// The base as a sealed, write-free layer: its children are the layers on the base.
    pub fn root(&self) -> SealedLayer<Db> {
        SealedLayer {
            base: Arc::clone(&self.base),
            node: Arc::clone(&self.base.root),
        }
    }

    /// The base's revision at the freeze.
    pub fn base_revision(&self) -> Revision {
        self.base.revision
    }

    /// What the layers did to the base so far (D11, I2). Reads every slot of the base: call it
    /// with no query running on a layer.
    ///
    /// # Contract
    /// - C1: after layers diagnosed rust-analyzer diagnostics, `refused` is 0, the shared writes
    ///   are of the listed kinds only, and every slot of the base fingerprinted at the freeze is
    ///   unchanged.
    ///   tested: tests/v2.rs v2_layers_diagnose_as_their_flat_check
    pub fn isolation(&self) -> Isolation {
        let db = self.base.lock();
        Isolation {
            shared: fork::shared_writes(&*db),
            skipped: fork::skipped_writes(&*db),
            refused: fork::refused_writes(&*db),
            fingerprint: fork::base_fingerprint(&*db),
        }
    }

    /// Thaws the base and gives the database back, its revision raised above every layer
    /// revision handed out, when no layer of it and no other handle of it lives. Otherwise
    /// returns the frozen base with [`EngineError::InUse`] at once, changing nothing.
    pub fn thaw(self) -> Result<Db, (Frozen<Db>, EngineError)> {
        let base = match Arc::try_unwrap(self.base) {
            Ok(base) => base,
            Err(base) => return Err((Frozen { base }, EngineError::InUse)),
        };
        let Base {
            db,
            root,
            revision,
            created,
        } = base;
        let mut db = db.into_inner().unwrap_or_else(PoisonError::into_inner);
        match fork::try_thaw(&mut db) {
            Ok(_) => Ok(db),
            Err(_) => Err((
                Frozen {
                    base: Arc::new(Base {
                        db: Mutex::new(db),
                        root,
                        revision,
                        created,
                    }),
                },
                EngineError::InUse,
            )),
        }
    }
}

/// A layer that takes writes: made by [`SealedLayer::child`], ended by [`OpenLayer::seal`].
///
/// The order the Engine allows is: every write first ([`OpenLayer::write`],
/// [`OpenLayer::create`]), then the queries, through the database [`OpenLayer::db`] lends. Once
/// `db` was called the layer takes no further write ([`EngineError::Lent`]), and while a clone of
/// its database lives no write either ([`EngineError::InUse`]). A query that ran on a layer, in
/// salsa or in a cache outside it keyed by the layer's revision, therefore never sees the layer
/// change under it.
///
/// # Contract
/// - C1: the Engine's API hands out no owned database handle: `db` lends a `LayerDb` guard of an
///   open layer, `read` lends a `&Db` of a throwaway child of a sealed layer; a child and a read
///   exist only for a sealed layer; an open layer cannot be used after `seal`, nor sealed while a
///   guard is borrowed. These misuses do not compile. A clone made through the guard's deref is
///   an owned value the types do not hold back; it is refused by C3, at `seal`, at `thaw` and at
///   the end of `read`.
///   tested: tests/compile_fail.rs sealing_types_refuse_misuse
/// - C2: a write that reaches a sealed layer past the types (through salsa's fork API) panics, in
///   release builds too.
///   tested: tests/layers.rs a_write_reaching_a_sealed_layer_panics
/// - C3: `seal` with a clone of the layer's database alive, and `thaw` with any handle of the
///   base alive, return the value with `InUse` and do not wait; `read`, after `f` returns,
///   checks the throwaway child's handle count and returns `InUse` (not the result of `f`) when
///   a clone outlived `f`.
///   tested: tests/layers.rs seal_refuses_while_a_db_clone_lives
///   tested: tests/layers.rs thaw_refuses_instead_of_waiting
///   tested: tests/layers.rs a_clone_escaping_read_is_refused
/// - C4: writes come before queries. `write` and `create` with a clone of the layer's database
///   alive return `InUse` and do not wait, so no query escaped through the guard runs alongside
///   a write; after `db` was called they return `Lent`; either way the layer is unchanged. A
///   write that still reaches the layer after a query ran in it, past these checks (through
///   salsa's fork API), panics.
///   tested: tests/layers.rs a_write_refuses_while_a_db_clone_lives
///   tested: tests/layers.rs a_write_after_the_database_was_lent_is_refused
pub struct OpenLayer<Db: EngineDb> {
    base: Arc<Base<Db>>,
    parent: Arc<Node<Db::File>>,
    layer: Arc<Layer>,
    db: Box<Db>,
    files: Vec<Db::File>,
    /// Whether `db` was called: from then on the layer takes no write (C4).
    lent: AtomicBool,
}

impl<Db: EngineDb> fmt::Debug for OpenLayer<Db> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenLayer")
            .field("revision", &self.layer.revision())
            .field("files", &self.files)
            .finish_non_exhaustive()
    }
}

impl<Db: EngineDb> OpenLayer<Db> {
    /// Lends the layer's database: a handle whose reads resolve through this layer and its
    /// ancestors. The guard borrows the layer, so the layer cannot be sealed while it lives.
    /// From the first call on, the layer takes no write ([`EngineError::Lent`]).
    pub fn db(&self) -> LayerDb<'_, Db> {
        self.lent.store(true, Ordering::Release);
        LayerDb { db: &self.db }
    }

    /// Refuses a write: `InUse` with a clone of the layer's database alive, `Lent` once `db`
    /// was called (C4).
    fn check_writable(&self) -> Result<(), EngineError> {
        if Arc::strong_count(&self.layer) != OWN_REFERENCES {
            return Err(EngineError::InUse);
        }
        if self.lent.load(Ordering::Acquire) {
            return Err(EngineError::Lent);
        }
        Ok(())
    }

    /// Sets the text of `file` in this layer. Refused, with the layer unchanged, while a clone
    /// of its database lives (`InUse`) or once its database was lent (`Lent`).
    ///
    /// # Contract
    /// - C1: after `write(file, text)` the layer, its children and reads of it see `text` for
    ///   `file`, while the base, the parent and a sibling keep their texts; `files` of the sealed
    ///   layer lists `file` once, however often it was written.
    ///   tested: tests/layers.rs a_write_is_seen_by_its_chain_only
    pub fn write(&mut self, file: Db::File, text: impl Into<Arc<str>>) -> Result<(), EngineError> {
        self.check_writable()?;
        self.db.set_file_text_in_layer(file, text.into());
        if !self.files.contains(&file) {
            self.files.push(file);
        }
        Ok(())
    }

    /// Creates `file`, which the database does not know, with `text` in this layer, without a
    /// write lock on the base. Refused as [`OpenLayer::write`] is (C4 of the type).
    ///
    /// # Contract
    /// - C1: with another handle of the base alive the call returns (it never waits), and the
    ///   layer and its children read `text` for `file`.
    ///   tested: tests/ra.rs creating_a_file_in_a_layer_never_waits
    /// - C2: two layers of one base that create the same file, siblings or a layer and its
    ///   descendant, each read their own text for it; neither panics.
    ///   tested: tests/ra.rs two_layers_create_the_same_file
    ///
    /// A file created by a layer stays in the base's file map after a thaw (rust-analyzer's
    /// `Files` is shared by every handle); creating it again under a later freeze is a write of
    /// a known file, which panics on a layered handle: use [`OpenLayer::write`] for it.
    pub fn create(&mut self, file: Db::File, text: impl Into<Arc<str>>) -> Result<(), EngineError> {
        self.check_writable()?;
        let text = text.into();
        // The registry's lock is held over the creation, so two layers creating the same file
        // at once do not both take the vacant path.
        let mut created = self.base.created();
        if created.insert(file) {
            self.db.create_file_in_layer(file, text);
        } else {
            drop(created);
            self.db.set_file_text_in_layer(file, text);
        }
        if !self.files.contains(&file) {
            self.files.push(file);
        }
        Ok(())
    }

    /// The layer's own counters (memos, stamps, deep verifies, tracked-struct entries).
    pub fn stats(&self) -> fork::LayerStats {
        self.layer.stats()
    }

    /// The layer's revision: unique in the process, above its parent's and the base's.
    pub fn revision(&self) -> Revision {
        self.layer.revision()
    }

    /// Seals the layer: it takes no further write and may have children. With a clone of its
    /// database alive it returns the open layer with [`EngineError::InUse`] at once.
    pub fn seal(self) -> Result<SealedLayer<Db>, (OpenLayer<Db>, EngineError)> {
        if Arc::strong_count(&self.layer) != OWN_REFERENCES {
            return Err((self, EngineError::InUse));
        }
        let OpenLayer {
            base,
            parent,
            layer,
            db,
            files,
            lent: _,
        } = self;
        drop(db);
        layer.seal();
        Ok(SealedLayer {
            base,
            node: Arc::new(Node {
                layer,
                parent: Some(parent),
                files,
            }),
        })
    }
}

/// A borrowed database handle of an [`OpenLayer`].
pub struct LayerDb<'a, Db> {
    db: &'a Db,
}

impl<Db> Deref for LayerDb<'_, Db> {
    type Target = Db;

    fn deref(&self) -> &Db {
        self.db
    }
}

/// A layer that takes no write: the base ([`Frozen::root`]) or a sealed [`OpenLayer`]. Cheap to
/// clone; dropping the last clone (and every child) frees the layer. Its clauses are those of
/// [`OpenLayer`].
pub struct SealedLayer<Db: EngineDb> {
    base: Arc<Base<Db>>,
    node: Arc<Node<Db::File>>,
}

impl<Db: EngineDb> Clone for SealedLayer<Db> {
    fn clone(&self) -> Self {
        SealedLayer {
            base: Arc::clone(&self.base),
            node: Arc::clone(&self.node),
        }
    }
}

impl<Db: EngineDb> fmt::Debug for SealedLayer<Db> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SealedLayer")
            .field("revision", &self.node.layer.revision())
            .field("files", &self.node.files)
            .finish_non_exhaustive()
    }
}

impl<Db: EngineDb> SealedLayer<Db> {
    /// Opens a layer on this one.
    pub fn child(&self) -> OpenLayer<Db> {
        let layer = fork::child_layer(&self.node.layer);
        let db = self.base.handle(&layer);
        OpenLayer {
            base: Arc::clone(&self.base),
            parent: Arc::clone(&self.node),
            layer,
            db: Box::new(db),
            files: Vec::new(),
            lent: AtomicBool::new(false),
        }
    }

    /// Runs `f` on the database of a throwaway, write-free child of this layer, which is dropped
    /// afterwards. When a clone of that database outlives `f`, returns [`EngineError::InUse`]
    /// instead of the result of `f`.
    pub fn read<R>(&self, f: impl FnOnce(&Db) -> R) -> Result<R, EngineError> {
        let child = self.child();
        let result = f(&child.db);
        if Arc::strong_count(&child.layer) != OWN_REFERENCES {
            return Err(EngineError::InUse);
        }
        Ok(result)
    }

    /// The text of `file` as this layer sees it: the newest text its chain set, else the base's.
    /// Reads the input, runs no query.
    pub fn text(&self, file: Db::File) -> Arc<str> {
        self.base.handle(&self.node.layer).file_text(file)
    }

    /// The files the layers of this chain wrote, oldest layer first, each once.
    pub fn files(&self) -> Vec<Db::File> {
        let mut chain: Vec<&Node<Db::File>> =
            std::iter::successors(Some(&*self.node), |node| node.parent.as_deref()).collect();
        chain.reverse();
        let mut out = Vec::new();
        for file in chain.into_iter().flat_map(|node| &node.files) {
            if !out.contains(file) {
                out.push(*file);
            }
        }
        out
    }

    /// The layer's revision: unique in the process, above its parent's and the base's.
    pub fn revision(&self) -> Revision {
        self.node.layer.revision()
    }

    /// The layer's own counters (memos, stamps, deep verifies, tracked-struct entries).
    pub fn stats(&self) -> fork::LayerStats {
        self.node.layer.stats()
    }
}
