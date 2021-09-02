//! Main API entry point for reading and manipulating Metamath databases.
//!
//! A variable of type `Database` represents a loaded database.  You can
//! construct a `Database` object, then cause it to represent a database from a
//! disk file using the `parse` method, then query various analysis results
//! which will be computed on demand.  You can call `parse` again to reload
//! data; the implementation expects there to be minor changes, and optimizes
//! with incremental recomputation.
//!
//! It is also possible to modify a loaded database by opening it (details TBD);
//! while the database is open most analyses cannot be used, but it is permitted
//! to call `Clone::clone` on a `Database` and the type is designed to make that
//! relatively efficient (currently requires the duplication of three large hash
//! tables, this can be optimized).
//!
//! ## On segmentation
//!
//! Existing Metamath verifiers which attempt to maintain a DOM represent it as
//! a flat list of statements.  In order to permit incremental and parallel
//! operation across _all_ phases, we split the list into one or more segments.
//! A **segment** is a run of statements which are parsed together and will
//! always remain contiguous in the logical system.  Segments are generated by
//! the parsing process, and are the main unit of recalculation and parallelism
//! for subsequent passes.  We do not allow grouping constructs to span segment
//! boundaries; since we also disallow top-level `$e` statements, this means
//! that the scope of an `$e` statement is always limited to a single segment.
//!
//! A source file without include statements will be treated as a single segment
//! (except for splitting, see below).  A source file with N include statements
//! will generate N + 1 segments; a new segment is started immediately after
//! each include to allow any segment(s) from the included file to be slotted
//! into the correct order.  Thus segments cannot quite be the parallelism
//! granularity for parsing, because during the parse we don't know the final
//! number of segments; instead each source file is parsed independently, gating
//! rereading and reparsing on the file modification time.
//!
//! As an exception to support parallel processing of large single files (like
//! set.mm at the time of writing), source files larger than 1MiB are
//! automatically split into multiple pieces before parsing.  Each piece tracks
//! the need to recalculate independently, and each piece may generate or or
//! more segments as above.  Pieces are identified using chapter header
//! comments, and are located using a simple word-at-a-time Boyer-Moore search
//! that is much faster than the actual parser (empirically, it is limited by
//! main memory sequential read speed).  _Note that this means that for large
//! files, chapter header comments are effectively illegal inside of grouping
//! statements.  set.mm is fine with that restriction, but it does not match the
//! spec._
//!
//! Each loaded segment is assigned an ID (of type `SegmentId`, an opacified
//! 32-bit integer).  These IDs are **reused** when a segment is replaced with
//! another segment with the same logical sequence position; this allows
//! subsequent passes to interpret the new segment as the inheritor of the
//! previous segment, and reuse caches as applicable.  It then becomes necessary
//! to decide which of two segments is earlier in the logical order; it is not
//! possible to simply use numeric order, as a new segment might need to be
//! added between any two existing segments.  This is the well-studied
//! [order-maintenance problem][OMP]; we currently have a naive algorithm in the
//! `parser::SegmentOrder` structure, but a more sophisticated one could be
//! added later.  We never reuse a `SegmentId` in a way which would cause the
//! relative position of two `SegmentId` values to change; this means that after
//! many edits and incremental reloads the `SegmentOrder` will grow, and it may
//! become necessary to add code later to trigger a global renumbering (which
//! would necesssarily entail recomputation of all passes for all segments, but
//! the amortized complexity need not be bad).
//!
//! [OMP]: https://en.wikipedia.org/wiki/Order-maintenance_problem
//!
//! ## Incremental processing: Readers and Usages
//!
//! A pass will be calculated when its result is needed.  Operation is currently
//! lazy at a pass level, so it is not possible to verify only one segment,
//! although that _might_ change.  The results of a pass are stored in a data
//! structure indexed by some means, each element of which has an associated
//! version number.  When another pass needs to use the result of the first
//! pass, it tracks which elements of the first pass's result are used for each
//! segment, and their associated version numbers; this means that if a small
//! database change is made and the second pass is rerun, it can quickly abort
//! on most segments by checking if the dependencies _of that segment_ have
//! changed, using only the version numbers.
//!
//! This is not yet a rigidly systematized thing; for an example, nameck
//! generates its result as a `nameck::Nameset`, and implements
//! `nameck::NameUsage` objects which scopeck can use to record which names were
//! used scoping a given segment; it also provides `nameck::NameReader` objects
//! which can be used to access the nameset while simultaneously building a
//! usage object that can be used for future checking.
//!
//! ## Parallelism and promises
//!
//! The current parallel processing implementation is fairly simplistic.  If you
//! want to run a number of code fragments in parallel, get a reference to the
//! `Executor` object for the current database, then use it to queue a closure
//! for each task you want to run; the queueing step returns a `Promise` object
//! which can be used to wait for the task to complete.  Generally you want to
//! queue everything, then wait for everything.
//!
//! To improve packing efficiency, jobs are dispatched in descending order of
//! estimated runtime.  This requires an additional argument when queueing.

use crate::diag;
use crate::diag::DiagnosticClass;
use crate::diag::Notation;
use crate::export;
use crate::grammar;
use crate::grammar::Grammar;
use crate::grammar::StmtParse;
use crate::nameck::Nameset;
use crate::outline;
use crate::outline::OutlineNode;
use crate::parser::StatementRef;
use crate::scopeck;
use crate::scopeck::ScopeResult;
use crate::segment_set::SegmentSet;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fmt;
use std::fs::File;
use std::panic;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::thread;
use std::time::Instant;
use crate::verify;
use crate::verify::VerifyResult;

/// Structure for options that affect database processing, and must be constant
/// for the lifetime of the database container.
///
/// Some of these could theoretically support modification.
#[derive(Default,Debug)]
pub struct DbOptions {
    /// If true, the automatic splitting of large files described above is
    /// enabled, with the caveat about chapter comments inside grouping
    /// statements.
    pub autosplit: bool,
    /// If true, time in milliseconds is printed after the completion of each
    /// pass.
    pub timing: bool,
    /// True to print names (determined by a very simple heuristic, see
    /// `parser::guess_buffer_name`) of segments which are recalculated in each
    /// pass.
    pub trace_recalc: bool,
    /// True to record database outline with parts, chapters, and sections.
    pub outline: bool,
    /// True to record detailed usage data needed for incremental operation.
    ///
    /// This will slow down the initial analysis, so don't set it if you won't
    /// use it.  If this is false, any reparse will result in a full
    /// recalculation, so it is always safe but different settings will be
    /// faster for different tasks.
    pub incremental: bool,
    /// Number of jobs to run in parallel at any given time.
    pub jobs: usize,
    /// If true, will parse the statements in addition to preparing the grammar
    pub parse_statements: bool,
}

/// Wraps a heap-allocated closure with a difficulty score which can be used for
/// sorting; this might belong in the standard library as `CompareFirst` or such.
struct Job(usize, Box<dyn FnMut() + Send>);
impl PartialEq for Job {
    fn eq(&self, other: &Job) -> bool {
        self.0 == other.0
    }
}
impl Eq for Job {}
impl PartialOrd for Job {
    fn partial_cmp(&self, other: &Job) -> Option<Ordering> {
        Some(self.0.cmp(&other.0))
    }
}
impl Ord for Job {
    fn cmp(&self, other: &Job) -> Ordering {
        self.0.cmp(&other.0)
    }
}

/// Object which holds the state of the work queue and allows queueing tasks to
/// run on the thread pool.
#[derive(Clone)]
pub struct Executor {
    concurrency: usize,
    // Jobs are kept in a heap so that we can dispatch the biggest one first.
    mutex: Arc<Mutex<BinaryHeap<Job>>>,
    // Condvar used to notify work threads of new work.
    work_cv: Arc<Condvar>,
}

/// Debug printing for `Executor` displays the current count of queued but not
/// dispatched tasks.
impl fmt::Debug for Executor {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let g = self.mutex.lock().unwrap();
        write!(f, "Executor(active={})", g.len())
    }
}

fn queue_work(exec: &Executor, estimate: usize, mut f: Box<dyn FnMut() + Send>) {
    if exec.concurrency <= 1 {
        f();
        return;
    }
    let mut wq = exec.mutex.lock().unwrap();
    wq.push(Job(estimate, f));
    exec.work_cv.notify_one();
}

impl Executor {
    /// Instantiates a new work queue and creates the threads to service it.
    ///
    /// The threads will exit when the `Executor` goes out of scope (not yet
    /// implemented).  In the future, we *may* have process-level coordination
    /// to allow different `Executor`s to share a thread pool, and use per-job
    /// concurrency limits.
    pub fn new(concurrency: usize) -> Executor {
        let mutex = Arc::new(Mutex::new(BinaryHeap::new()));
        let cv = Arc::new(Condvar::new());

        if concurrency > 1 {
            for _ in 0..concurrency {
                let mutex = mutex.clone();
                let cv = cv.clone();
                thread::spawn(move || {
                    loop {
                        let mut task: Job = {
                            let mut mutexg = mutex.lock().unwrap();
                            while mutexg.is_empty() {
                                mutexg = cv.wait(mutexg).unwrap();
                            }
                            mutexg.pop().unwrap()
                        };
                        (task.1)();
                    }
                });
            }
        }

        Executor {
            concurrency: concurrency,
            mutex: mutex,
            work_cv: cv,
        }
    }

    /// Queue a job on this work queue.
    ///
    /// The estimate is meaningless in isolation but jobs with a higher estimate
    /// will be dispatched first, so it should be comparable among jobs that
    /// could simultaneously be in the work queue.
    ///
    /// Returns a `Promise` that can be used to wait for completion of the
    /// queued work.  If the provided task panics, the error will be stored and
    /// rethrown when the promise is awaited.
    pub fn exec<TASK, RV>(&self, estimate: usize, task: TASK) -> Promise<RV>
        where TASK: FnOnce() -> RV,
              TASK: Send + 'static,
              RV: Send + 'static
    {
        let parts = Arc::new((Mutex::new(None), Condvar::new()));

        let partsc = parts.clone();
        let mut tasko = Some(task);
        queue_work(self,
                   estimate,
                   Box::new(move || {
            let mut g = partsc.0.lock().unwrap();
            let taskf = panic::AssertUnwindSafe(tasko.take().expect("should only be called once"));
            *g = Some(panic::catch_unwind(taskf));
            partsc.1.notify_one();
        }));

        Promise::new_once(move || {
            let mut g = parts.0.lock().unwrap();
            while g.is_none() {
                g = parts.1.wait(g).unwrap();
            }
            g.take().unwrap().unwrap()
        })
    }
}

/// A handle for a value which will be available later.
///
/// Promises are normally constructed using `Executor::exec`, which moves
/// computation to a thread pool.  There are several other methods to attach
/// code to promises; these do **not** parallelize, and are intended to do very
/// cheap tasks for interface consistency purposes only.
pub struct Promise<T>(Box<dyn FnMut() -> T + Send>);

impl<T> Promise<T> {
    /// Wait for a value to be available and return it, rethrowing any panic.
    pub fn wait(mut self) -> T {
        (self.0)()
    }

    /// Construct a promise which uses a provided closure to wait for the value
    /// when necessary.
    ///
    /// This does **not** do any parallelism; the provided closure will be
    /// invoked when `wait` is called, on the thread where `wait` is called.  If
    /// you want to run code in parallel, use `Executor::exec`.
    pub fn new_once<FN>(fun: FN) -> Promise<T>
        where FN: FnOnce() -> T + Send + 'static
    {
        let mut funcell = Some(fun);
        // the take hack works around the lack of stable FnBox
        Promise(Box::new(move || (funcell.take().unwrap())()))
    }

    /// Wrap a value which is available now in a promise.
    pub fn new(value: T) -> Self
        where T: Send + 'static
    {
        Promise::new_once(move || value)
    }

    /// Modify a promise with a function, which will be called at `wait` time on
    /// the `wait` thread.
    pub fn map<FN, RV>(self, fun: FN) -> Promise<RV>
        where T: 'static,
              FN: 'static,
              FN: Send + FnOnce(T) -> RV
    {
        Promise::new_once(move || fun(self.wait()))
    }

    /// Convert a collection of promises into a single promise, which waits for
    /// all of its parts.
    pub fn join(promises: Vec<Promise<T>>) -> Promise<Vec<T>>
        where T: 'static
    {
        Promise::new_once(move || promises.into_iter().map(|x| x.wait()).collect())
    }
}

/// Master type of database containers.
///
/// A variable of type `Database` holds a database, i.e. an ordered collection
/// of segments and analysis results for that collection.  Analysis results are
/// generated lazily for each database, and are invalidated on any edit to the
/// database's segments.  If you need to refer to old analysis results while
/// making a sequence of edits, call `Clone::clone` on the database first; this
/// is intended to be a relatively cheap operation.
///
/// More specifically, cloning a `Database` object does essentially no work
/// until it is necessary to run an analysis pass on one clone or the other;
/// then if the analysis pass has a result index which is normally updated in
/// place, such as the hash table of statement labels constructed by nameck,
/// that table must be duplicated so that it can be updated for one database
/// without affecting the other.
pub struct Database {
    options: Arc<DbOptions>,
    segments: Option<Arc<SegmentSet>>,
    /// We track the "current" and "previous" for all known passes, so that each
    /// pass can use its most recent results for optimized incremental
    /// processing.  Any change to the segment vector zeroizes the current
    /// fields but not the previous fields.
    prev_nameset: Option<Arc<Nameset>>,
    nameset: Option<Arc<Nameset>>,
    prev_scopes: Option<Arc<ScopeResult>>,
    scopes: Option<Arc<ScopeResult>>,
    prev_verify: Option<Arc<VerifyResult>>,
    verify: Option<Arc<VerifyResult>>,
    outline: Option<Arc<OutlineNode>>,
    grammar: Option<Arc<Grammar>>,
    stmt_parse: Option<Arc<StmtParse>>,
}

fn time<R, F: FnOnce() -> R>(opts: &DbOptions, name: &str, f: F) -> R {
    let now = Instant::now();
    let ret = f();
    if opts.timing {
        // no as_msecs :(
        println!("{} {}ms", name, (now.elapsed() * 1000).as_secs());
    }
    ret
}

impl Drop for Database {
    fn drop(&mut self) {
        time(&self.options.clone(), "free", move || {
            self.prev_verify = None;
            self.verify = None;
            self.prev_scopes = None;
            self.scopes = None;
            self.prev_nameset = None;
            self.nameset = None;
            self.segments = None;
            self.outline = None;
        });
    }
}

impl Database {
    /// Constructs a new database object representing an empty set of segments.
    ///
    /// Use `parse` to load it with data.  Currently this eagerly starts the
    /// threadpool, but that may change.
    pub fn new(options: DbOptions) -> Database {
        let options = Arc::new(options);
        let exec = Executor::new(options.jobs);
        Database {
            segments: Some(Arc::new(SegmentSet::new(options.clone(), &exec))),
            options: options,
            nameset: None,
            scopes: None,
            verify: None,
            outline: None,
            grammar: None,
            stmt_parse: None,
            prev_nameset: None,
            prev_scopes: None,
            prev_verify: None,
        }
    }

    /// Replaces the content of a database in memory with the parsed content of
    /// one or more input files.
    ///
    /// To load data from disk files, pass the pathname as `start` and leave
    /// `text` empty.  `start` and any references arising from file inclusions
    /// will be processed relative to the current directory; we _may_ add a base
    /// directory option later.
    ///
    /// The database object will remember the name and OS modification time of
    /// all files read to construct its current state, and will skip rereading
    /// them if the modification change has not changed on the next call to
    /// `parse`.  If your filesystem has poor modification time granulatity,
    /// beware of possible lost updates if you modify a file and the timestamp
    /// does not change.
    ///
    /// To parse data already resident in program memory, pass an arbitrary name
    /// as `start` and then pass a pair in `text` mapping that name to the
    /// buffer to parse.  Any file inclusions found in the buffer can be
    /// resolved from additional pairs in `text`; file inclusions which are
    /// _not_ found in `text` will be resolved on disk relative to the current
    /// directory as above (this feature has [an uncertain future][FALLBACK]).
    ///
    /// [FALLBACK]: https://github.com/sorear/smetamath-rs/issues/18
    ///
    /// All analysis passes will be invalidated; they will not immediately be
    /// rerun, but will be when next requested.  If the database is not
    /// currently empty, the files loaded are assumed to be similar to the
    /// current database content and incremental processing will be used as
    /// appropriate.
    pub fn parse(&mut self, start: String, text: Vec<(String, Vec<u8>)>) {
        time(&self.options.clone(), "parse", || {
            Arc::make_mut(self.segments.as_mut().unwrap()).read(start, text);
            self.nameset = None;
            self.scopes = None;
            self.verify = None;
            self.outline = None;
            self.grammar = None;
        });
    }

    /// Obtains a reference to the current parsed data.
    ///
    /// Unlike the other accessors, this is not lazy (subject to change when the
    /// modification API goes in.)
    pub fn parse_result(&mut self) -> &Arc<SegmentSet> {
        self.segments.as_ref().unwrap()
    }

    /// Calculates and returns the name to definition lookup table.
    pub fn name_result(&mut self) -> &Arc<Nameset> {
        if self.nameset.is_none() {
            time(&self.options.clone(), "nameck", || {
                if self.prev_nameset.is_none() {
                    self.prev_nameset = Some(Arc::new(Nameset::new()));
                }
                let pr = self.parse_result().clone();
                {
                    let ns = Arc::make_mut(self.prev_nameset.as_mut().unwrap());
                    ns.update(&pr);
                }
                self.nameset = self.prev_nameset.clone();
            });
        }

        self.nameset.as_ref().unwrap()
    }

    /// Calculates and returns the frames for this database, i.e. the actual
    /// logical system.
    ///
    /// All logical properties of the database (as opposed to surface syntactic
    /// properties) can be obtained from this object.
    pub fn scope_result(&mut self) -> &Arc<ScopeResult> {
        if self.scopes.is_none() {
            self.name_result();
            time(&self.options.clone(), "scopeck", || {
                if self.prev_scopes.is_none() {
                    self.prev_scopes = Some(Arc::new(ScopeResult::default()));
                }

                let parse = self.parse_result().clone();
                let name = self.name_result().clone();
                {
                    let ns = Arc::make_mut(self.prev_scopes.as_mut().unwrap());
                    scopeck::scope_check(ns, &parse, &name);
                }
                self.scopes = self.prev_scopes.clone();
            });
        }

        self.scopes.as_ref().unwrap()
    }

    /// Calculates and returns verification information for the database.
    ///
    /// This is an optimized verifier which returns no useful information other
    /// than error diagnostics.  It does not save any parsed proof data.
    pub fn verify_result(&mut self) -> &Arc<VerifyResult> {
        if self.verify.is_none() {
            self.name_result();
            self.scope_result();
            time(&self.options.clone(), "verify", || {
                if self.prev_verify.is_none() {
                    self.prev_verify = Some(Arc::new(VerifyResult::default()));
                }

                let parse = self.parse_result().clone();
                let scope = self.scope_result().clone();
                let name = self.name_result().clone();
                {
                    let ver = Arc::make_mut(self.prev_verify.as_mut().unwrap());
                    verify::verify(ver, &parse, &name, &scope);
                }
                self.verify = self.prev_verify.clone();
            });
        }
        self.verify.as_ref().unwrap()
    }

    /// Returns the root node of the outline
    pub fn outline_result(&mut self) -> &Arc<OutlineNode> {
        if self.outline.is_none() {
            time(&self.options.clone(), "outline", || {
                let parse = self.parse_result().clone();
                let mut outline = OutlineNode::default();
                outline::build_outline(&mut outline, &parse);
                self.outline = Some(Arc::new(outline));
            })
        }
        self.outline.as_ref().unwrap()
    }

    /// Builds and returns the grammar
    pub fn grammar_result(&mut self) -> &Arc<Grammar> {
        if self.grammar.is_none() {
            self.name_result();
            self.scope_result();
            time(&self.options.clone(), "grammar", || {
                let parse = self.parse_result().clone();
                let name = self.name_result().clone();
                let mut grammar = Grammar::default();
                grammar::build_grammar(&mut grammar, &parse, &name);
                self.grammar = Some(Arc::new(grammar));
            })
        }
        self.grammar.as_ref().unwrap()
    }

    /// Parses the statements using the grammar
    pub fn stmt_parse_result(&mut self) -> &Arc<StmtParse> {
        if self.stmt_parse.is_none() {
            self.name_result();
            self.scope_result();
            time(&self.options.clone(), "stmt_parse", || {
                let parse = self.parse_result().clone();
                let name = self.name_result().clone();
                let grammar = self.grammar_result().clone();
                let mut stmt_parse = StmtParse::default();
                grammar::parse_statements(&mut stmt_parse, &parse, &name, &grammar);
                self.stmt_parse = Some(Arc::new(stmt_parse));
            })
        }
        self.stmt_parse.as_ref().unwrap()
    }

    /// A getter method which does not build the outline
    pub fn get_outline(&self) -> &Option<Arc<OutlineNode>> {
        &self.outline
    }

    /// Get a statement by label.
    pub fn statement(&mut self, name: &str) -> Option<StatementRef> {
        let lookup = self.name_result().lookup_label(name.as_bytes())?;
        Some(self.parse_result().statement(lookup.address))
    }

    /// Export an mmp file for a given statement.
    pub fn export(&mut self, stmt: String) {
        time(&self.options.clone(), "export", || {
            let parse = self.parse_result().clone();
            let scope = self.scope_result().clone();
            let name = self.name_result().clone();
            let sref = self.statement(&stmt)
                .expect(format!("Label {} did not correspond to an existing statement",
                                &stmt)
                    .as_ref());

            File::create(format!("{}.mmp", stmt.clone()))
                .map_err(export::ExportError::Io)
                .and_then(|mut file| export::export_mmp(&parse, &name, &scope, sref, &mut file))
                .unwrap()
        })
    }

    /// Export the grammar of this database in DOT format.
    #[cfg(feature = "dot")]
    pub fn export_grammar_dot(&mut self) {
        time(&self.options.clone(), "export_grammar_dot", || {
            let name = self.name_result().clone();
            let grammar = self.grammar_result().clone();

            File::create("grammar.dot")
                .map_err(export::ExportError::Io)
                .and_then(|mut file| grammar.export_dot(&name, &mut file))
                .unwrap()
        })
    }

    /// Dump the grammar of this database.
    pub fn print_grammar(&mut self) {
        time(&self.options.clone(), "print_grammar", || {
            let name = self.name_result().clone();
            let grammar = self.grammar_result().clone();
            grammar.dump(&name);
        })
    }

    /// Dump the formulas of this database.
    pub fn print_formula(&mut self) {
        time(&self.options.clone(), "print_formulas", || {
            let parse = self.parse_result().clone();
            let name = self.name_result().clone();
            let stmt_parse = self.stmt_parse_result().clone();
            stmt_parse.dump(&parse, &name);
        })
    }

    /// Dump the outline of this database.
    pub fn print_outline(&mut self) {
        time(&self.options.clone(), "print_outline", || {
            let root_node = self.outline_result().clone();
            self.print_outline_node(&root_node, 0);
        })
    }

    /// Dump the outline of this database.
    fn print_outline_node(&mut self, node: &OutlineNode, indent: usize) {
        // let indent = (node.level as usize) * 3
        println!("{:indent$} {:?} {:?}", "", node.level, node.get_name(), indent = indent);
        for child in node.children.iter() {
            self.print_outline_node(&child, indent + 1);
        }        
    }

    /// Runs one or more passes and collects all errors they generate.
    ///
    /// Passes are identified by the `types` argument and are not inclusive; if
    /// you ask for Verify, you will not get Parse unless you specifically ask
    /// for that as well.
    ///
    /// Currently there is no way to incrementally fetch diagnostics, so this
    /// will be a bit slow if there are thousands of errors.
    pub fn diag_notations(&mut self, types: Vec<DiagnosticClass>) -> Vec<Notation> {
        let mut diags = Vec::new();
        if types.contains(&DiagnosticClass::Parse) {
            diags.extend(self.parse_result().parse_diagnostics());
        }
        if types.contains(&DiagnosticClass::Scope) {
            diags.extend(self.scope_result().diagnostics());
        }
        if types.contains(&DiagnosticClass::Verify) {
            diags.extend(self.verify_result().diagnostics());
        }
        if types.contains(&DiagnosticClass::Grammar) {
            diags.extend(self.grammar_result().diagnostics());
        }
        if types.contains(&DiagnosticClass::StmtParse) {
            diags.extend(self.stmt_parse_result().diagnostics());
        }
        time(&self.options.clone(),
             "diag",
             || diag::to_annotations(self.parse_result(), diags))
    }
}
