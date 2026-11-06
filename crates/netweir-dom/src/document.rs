use std::cell::{Cell, UnsafeCell};
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;

use netweir_html as html;

use crate::index::Index;
use crate::xml::{self, XmlNames};

/// A parsed HTML document.
///
/// Built by Netweir's parser following the HTML Standard, so a page parses the way a
/// browser would parse it: missing `<html>`, `<head>`, `<body>` and `<tbody>`
/// elements are added, and misnested tags are repaired.
pub struct Document {
    /// The tree. Readers share it; an edit, which has the document to
    /// itself (see `edit`), is the only thing that changes it.
    tree: UnsafeCell<html::Document>,
    /// For an XML document, its namespaces (the HTML tree has no place
    /// for them).
    /// Only an edit changes them, as with the index.
    xml: UnsafeCell<Option<Box<XmlNames>>>,
    /// A flat, document-order index of every node, built on first use and
    /// dropped by every edit (which has the document to itself).
    index: UnsafeCell<OnceLock<Index>>,
    /// Nodes an edit left without a parent: removed ones, and new ones not
    /// yet placed. The index covers their subtrees too, so a handle to one
    /// keeps working. Pruned whenever the index is built.
    detached: Mutex<Vec<u32>>,
    /// Readers holding a [`Reading`] taken the slow way, or `WRITING`
    /// while an edit runs; `WAITING` while an edit waits for readers to
    /// finish.
    access: AtomicU32,
    /// Readers holding a [`Reading`] taken the fast way, counted on a
    /// line of their thread's own: a count every thread changed would
    /// bounce one cache line between their cores on every read, and
    /// threads reading one page would barely scale. A reader only reads
    /// `access`, which stays in every core's cache until an edit comes.
    readers: [Slot; SLOTS],
    /// Readers held back by an edit (running, or waiting and deferred to):
    /// the high half counts edits finished, the low half the readers held
    /// back since the last one finished. Finishing an edit takes the count
    /// and starts a new one, so a turn is for exactly those readers.
    queue: AtomicU64,
    /// Readers of the current turn not yet in.
    turn_left: AtomicU32,
    /// Readers a turn let in past a waiting edit (not counting ones whose
    /// patience ran out), and the most any turn let in.
    #[cfg(test)]
    turn_entries: AtomicU32,
    #[cfg(test)]
    most_turn_entries: AtomicU32,
    /// Wake-ups sent, so a test can tell a sleep nobody woke from one that
    /// outlasted a slow reader.
    #[cfg(test)]
    wakes: AtomicU32,
    /// Where a waiting edit sleeps until the last reader wakes it.
    gate: Mutex<()>,
    woken: Condvar,
}

const WRITING: u32 = 1 << 31;
const WAITING: u32 = 1 << 30;
/// Set by an edit that finished with readers held back: they go next, and
/// no edit starts until the last of them is in. Readers arriving later wait
/// for the next turn. Without it, an editor working back to back takes the
/// document again before a yielding reader wakes.
const TURN: u32 = 1 << 29;
const READERS: u32 = TURN - 1;

/// Reader counts per document, each on its own cache line (128 bytes, the
/// line of Apple's chips, two of x86's). A thread uses one slot for every
/// document; more threads than slots share them, which is still correct.
const SLOTS: usize = 16;

#[repr(align(128))]
struct Slot(AtomicU32);

/// How many times a new reader yields to a waiting edit before reading
/// anyway. Without it, readers that follow one another closely could keep
/// an edit out forever; with no limit, a reader that holds Python's GIL
/// could wait for an edit that waits for a reader needing the GIL.
const PATIENCE: u32 = 2000;

// SAFETY: reading never changes the tree. Edits change it, and their
// callers promise that other threads reading at the time hold a
// `Reading`, which an edit waits for. No string handed out ever moves:
// the parser's are fixed once it's done, and an edit's get allocations of
// their own (netweir-html's push_stable).
unsafe impl Send for Document {}
unsafe impl Sync for Document {}

thread_local! {
    /// How many `Reading`s this thread holds, of any document. An edit
    /// from inside one fails: on the same document it would wait for
    /// itself, and on another, two threads doing it crosswise would wait
    /// for each other. A plain counter in thread-local storage, so threads
    /// never touch each other's memory.
    static HELD: Cell<u32> = const { Cell::new(0) };
    /// This thread's reader slot plus one, handed out in turn on first
    /// use; 0 until then. Lazy by hand: a lazily initialised thread-local
    /// checks a state flag on every access, and this is on every read.
    static SLOT: Cell<usize> = const { Cell::new(0) };
}

fn my_slot() -> usize {
    SLOT.with(|s| match s.get() {
        0 => {
            let slot = NEXT_SLOT.fetch_add(1, Ordering::Relaxed) % SLOTS;
            s.set(slot + 1);
            slot
        }
        n => n - 1,
    })
}

static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);

/// Holds off edits to a document while it lives. See [`Document::read`].
pub struct Reading<'a> {
    doc: &'a Document,
    /// The slot it's counted in, if it came in the fast way.
    slot: Option<&'a Slot>,
    // The thread-local bookkeeping must be undone on the same thread.
    _not_send: PhantomData<*const ()>,
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        HELD.with(|h| h.set(h.get() - 1));
        if let Some(slot) = self.slot {
            // SeqCst, against the edit that sets WAITING and then looks at
            // the slots: either it sees this reader gone, or this reader
            // sees it waiting and wakes it. No reader can tell if it's the
            // last, so each wakes it (only while an edit waits).
            slot.0.fetch_sub(1, Ordering::SeqCst);
            if self.doc.access.load(Ordering::SeqCst) & WAITING != 0 {
                self.doc.wake();
            }
            return;
        }
        let before = self.doc.access.fetch_sub(1, Ordering::Release);
        // The last reader out wakes an edit waiting for it.
        if before & WAITING != 0 && before & READERS == 1 && self.doc.slots_empty() {
            self.doc.wake();
        }
    }
}

/// Why an edit wasn't made. Nothing changes when one fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// The edit makes no sense on these nodes, or a name isn't valid.
    Invalid(String),
    /// This thread holds a [`Reading`] of the document, so editing would
    /// wait for itself.
    Reading,
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditError::Invalid(why) => f.write_str(why),
            EditError::Reading => {
                f.write_str("can't edit a document while this thread is running a query")
            }
        }
    }
}

impl std::error::Error for EditError {}

/// What an edit puts in the tree: a node of the document, or new text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Content<'s> {
    Node(NodeId),
    Text(&'s str),
}

impl From<NodeId> for Content<'_> {
    fn from(id: NodeId) -> Self {
        Content::Node(id)
    }
}

fn invalid<T>(why: impl Into<String>) -> Result<T, EditError> {
    Err(EditError::Invalid(why.into()))
}

/// Elements written without an end tag.
const VOID: [&str; 18] = [
    "area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "img", "input",
    "keygen", "link", "meta", "param", "source", "track", "wbr",
];

impl Document {
    pub fn parse(html: &str) -> Document {
        Document::parse_within(html, Duration::MAX).expect("an unlimited budget cannot run out")
    }

    /// Like [`Document::parse`], but gives up once `budget` has passed.
    ///
    /// The HTML5 spec makes some inputs take time quadratic in their size.
    /// A crawler fetching pages it does not control should parse with a
    /// budget.
    pub fn parse_within(html: &str, budget: Duration) -> Result<Document, ParseTimeout> {
        let options = html::Options {
            scripting: false,
            budget: (budget != Duration::MAX).then_some(budget),
            ..html::Options::default()
        };
        let mut tree = match html::parse_with(html, options) {
            Ok(tree) => tree,
            Err(html::ParseError::Timeout { parsed, .. }) => {
                return Err(ParseTimeout { budget, parsed });
            }
            // ponytail: over 1 GB of HTML is reported as running out of
            // time at the start; a ParseError enum when anyone parses that.
            Err(html::ParseError::TooLarge { .. }) => {
                return Err(ParseTimeout { budget, parsed: 0 });
            }
        };
        // As lxml and Beautiful Soup show them: a template's contents are
        // its children.
        tree.adopt_template_contents();
        Ok(Document::wrap(tree, None))
    }

    pub(crate) fn tree(&self) -> &html::Document {
        // SAFETY: only an edit changes the tree, with no reader about.
        unsafe { &*self.tree.get() }
    }

    /// The tree to change.
    ///
    /// # Safety
    /// Only inside an edit, which has the document to itself.
    #[allow(clippy::mut_from_ref)]
    unsafe fn tree_mut(&self) -> &mut html::Document {
        unsafe { &mut *self.tree.get() }
    }

    /// The document's index. Building it writes each node's `user` field
    /// once, inside the OnceLock, before any reader can see the result.
    pub(crate) fn index(&self) -> &Index {
        // SAFETY: only an edit replaces the OnceLock, and an edit runs with
        // no reader on another thread and none of this crate's code holding
        // the reference on this one.
        unsafe { &*self.index.get() }.get_or_init(|| Index::build(self.tree(), &self.roots()))
    }

    /// The document node, then every node that is the top of a subtree
    /// outside it.
    fn roots(&self) -> Vec<u32> {
        let tree = self.tree();
        let mut detached = self.detached.lock().unwrap_or_else(|e| e.into_inner());
        detached.sort_unstable();
        detached.dedup();
        // A node put back in a tree is indexed with it.
        detached.retain(|&n| tree.parent(n).is_none());
        std::iter::once(tree.root())
            .chain(detached.iter().copied())
            .collect()
    }

    fn order_of(&self, id: u32) -> u32 {
        self.index().order(id)
    }

    /// The document node. Its children are the doctype and `<html>`.
    pub fn root(&self) -> Node<'_> {
        Node { doc: self, id: 0 }
    }

    /// Holds off edits until the returned value is dropped. A thread that
    /// reads the document while another may edit it must hold one; reading
    /// on the thread that edits needs none. Cheap: one atomic add, and no
    /// waiting unless an edit is running.
    pub fn read(&self) -> Reading<'_> {
        // The fast way, when no edit is running, waiting or letting in the
        // readers it held back: count this reader in its thread's slot,
        // then check that still holds. SeqCst on both, against an edit
        // doing the same the other way round (see Reading's drop).
        let slot = &self.readers[my_slot()];
        slot.0.fetch_add(1, Ordering::SeqCst);
        if self.access.load(Ordering::SeqCst) & (WRITING | WAITING | TURN) == 0 {
            HELD.with(|h| h.set(h.get() + 1));
            return Reading {
                doc: self,
                slot: Some(slot),
                _not_send: PhantomData,
            };
        }
        // An edit is about: step back, and take the way that queues
        // behind it fairly. An edit that saw this count waits for it to
        // go, so tell it.
        slot.0.fetch_sub(1, Ordering::SeqCst);
        if self.access.load(Ordering::SeqCst) & WAITING != 0 {
            self.wake();
        }
        // A read nested in one this thread holds never waits: an edit it
        // would let in may be waiting for the outer read to end.
        let nested = HELD.with(|h| h.get()) > 0;
        let mut patience = if nested { 0 } else { PATIENCE };
        // The edit count when this reader was held back, if it was.
        let mut ticket: Option<u32> = None;
        loop {
            let now = self.access.load(Ordering::Relaxed);
            // Let a waiting edit go first, unless this reader's turn has
            // come: it was held back before the last edit finished.
            let my_turn =
                now & TURN != 0 && ticket.is_some_and(|t| self.edits_done() == t.wrapping_add(1));
            let defer = now & WAITING != 0 && !my_turn && patience > 0;
            if now & WRITING == 0
                && !defer
                && self
                    .access
                    .compare_exchange_weak(now, now + 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            {
                // Counted unless the reader's patience ran out, which is
                // the deliberate limit on how long any reader yields.
                #[cfg(test)]
                if now & TURN != 0 && now & WAITING != 0 && patience > 0 {
                    let n = self.turn_entries.fetch_add(1, Ordering::Relaxed) + 1;
                    self.most_turn_entries.fetch_max(n, Ordering::Relaxed);
                }
                break;
            }
            // Losing a race to another reader isn't being held back.
            if ticket.is_none() && !nested && (now & WRITING != 0 || defer) {
                ticket = Some((self.queue.fetch_add(1, Ordering::Relaxed) >> 32) as u32);
            }
            // An edit is quick and needs nothing from this thread.
            patience = patience.saturating_sub(1);
            std::thread::yield_now();
        }
        // No edit can finish while this reader is in, so the count can't
        // change under these two steps.
        if let Some(t) = ticket {
            if self.edits_done() == t {
                // In before any edit ran: no longer held back.
                self.queue.fetch_sub(1, Ordering::Relaxed);
            } else if self.turn_left.fetch_sub(1, Ordering::Relaxed) == 1 {
                // The last of the turn ends it, and wakes an edit that
                // slept through it.
                let before = self.access.fetch_and(!TURN, Ordering::Relaxed);
                if before & WAITING != 0 {
                    self.wake();
                }
            }
        }
        HELD.with(|h| h.set(h.get() + 1));
        Reading {
            doc: self,
            slot: None,
            _not_send: PhantomData,
        }
    }

    /// No reader is in by the fast way. Called after setting WAITING, so a
    /// reader counted after this looks sees it and steps back.
    fn slots_empty(&self) -> bool {
        self.readers.iter().all(|s| s.0.load(Ordering::SeqCst) == 0)
    }

    /// Runs `edit` with the document to itself, then drops the index, which
    /// the next query rebuilds.
    fn edit<R>(&self, edit: impl FnOnce() -> Result<R, EditError>) -> Result<R, EditError> {
        if HELD.with(|h| h.get()) > 0 {
            return Err(EditError::Reading);
        }
        let mut tries = 0u32;
        loop {
            // Ask new readers to hold back, then take the document once
            // the ones reading are done.
            let now = self.access.fetch_or(WAITING, Ordering::SeqCst) | WAITING;
            if now == WAITING
                && self.slots_empty()
                && self
                    .access
                    .compare_exchange_weak(now, WRITING, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            {
                break;
            }
            // Readers can take a while (a find_all calling a Python
            // function per node), so after a few yields, sleep until the
            // last one wakes us. A condition variable rather than a timed
            // sleep: Windows rounds short sleeps up to a millisecond or more.
            tries += 1;
            if tries < 64 {
                std::thread::yield_now();
                continue;
            }
            let gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            // A reader leaving after this check has to take `gate` to wake
            // us, which it can only do once we're waiting.
            // Set WAITING again rather than only looking: an edit that got
            // in first cleared it, and then whoever leaves next (that edit,
            // or the last reader) wouldn't know to wake us.
            if self.access.fetch_or(WAITING, Ordering::SeqCst) & !WAITING != 0
                || !self.slots_empty()
            {
                #[cfg(test)]
                let wakes = self.wakes.load(Ordering::Relaxed);
                // The timeout only covers a wake-up this reasoning missed.
                let (_gate, waited) = self
                    .woken
                    .wait_timeout(gate, Duration::from_millis(1))
                    .unwrap_or_else(|e| e.into_inner());
                #[cfg(test)]
                let _gate = self.check_wake(_gate, waited.timed_out(), wakes);
                #[cfg(not(test))]
                let _ = waited;
            }
        }
        struct Done<'a>(&'a Document);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                // SAFETY: this thread has the document to itself.
                unsafe { *self.0.index.get() = OnceLock::new() };
                // Readers held back while this edit ran or waited get the
                // next turn; later ones count towards the turn after. A
                // WAITING bit set by another edit meanwhile is kept, so
                // whoever ends the turn knows to wake it.
                let queue = &self.0.queue;
                let mut q = queue.load(Ordering::Relaxed);
                let next = |q: u64| u64::from(((q >> 32) as u32).wrapping_add(1)) << 32;
                while let Err(now) =
                    queue.compare_exchange_weak(q, next(q), Ordering::Relaxed, Ordering::Relaxed)
                {
                    q = now;
                }
                let held = q as u32;
                self.0.turn_left.store(held, Ordering::Relaxed);
                let turn = if held > 0 { TURN } else { 0 };
                #[cfg(test)]
                self.0.turn_entries.store(0, Ordering::Relaxed);
                let before = self.0.access.fetch_xor(WRITING | turn, Ordering::Release);
                if before & WAITING != 0 && turn == 0 {
                    self.0.wake();
                }
            }
        }
        let _done = Done(self);
        edit()
    }

    /// After a sleep: counts it, and if it ran out, whether it was a lost
    /// wake-up. A sleep outlasting a reader that holds the page is expected
    /// (a busy machine deschedules readers). A lost one is a sleep that ran
    /// out with the way clear (no reader in, no turn, no edit) and no wake
    /// sent since it began. A reader that has just left may be waiting for
    /// `gate` to send its wake, so the sleep is given 100 ms more for it
    /// first.
    #[cfg(test)]
    fn check_wake<'g>(
        &self,
        mut gate: std::sync::MutexGuard<'g, ()>,
        timed_out: bool,
        wakes: u32,
    ) -> std::sync::MutexGuard<'g, ()> {
        tests::WAITS.fetch_add(1, Ordering::Relaxed);
        if !timed_out {
            return gate;
        }
        tests::TIMEOUTS.fetch_add(1, Ordering::Relaxed);
        let clear = |d: &Document| {
            d.access.load(Ordering::Relaxed) & (READERS | TURN | WRITING) == 0 && d.slots_empty()
        };
        if !clear(self) || self.wakes.load(Ordering::Relaxed) != wakes {
            return gate;
        }
        let deadline = Instant::now() + Duration::from_millis(100);
        while self.wakes.load(Ordering::Relaxed) == wakes {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                tests::LOST.fetch_add(1, Ordering::Relaxed);
                break;
            }
            gate = self
                .woken
                .wait_timeout(gate, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        gate
    }

    fn edits_done(&self) -> u32 {
        (self.queue.load(Ordering::Relaxed) >> 32) as u32
    }

    fn wake(&self) {
        let _gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(test)]
        self.wakes.fetch_add(1, Ordering::Relaxed);
        self.woken.notify_all();
    }

    fn mark_detached(&self, n: u32) {
        self.detached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(n);
    }

    /// Takes `node` (and everything in it) out of its tree. A handle to
    /// it, or to anything in it, keeps working on the removed subtree,
    /// which queries from the document no longer see. Its memory is freed
    /// with the document. Removing a node that has no parent does nothing.
    ///
    /// # Safety
    /// This and every other edit needs ids from this document, and any
    /// other thread reading the document meanwhile must hold a
    /// [`Document::read`]. Edits wait for those to be dropped.
    pub unsafe fn remove(&self, node: NodeId) -> Result<(), EditError> {
        let n = node.get();
        self.edit(|| {
            if n == 0 {
                return invalid("the document itself can't be removed");
            }
            let tree = unsafe { self.tree_mut() };
            if tree.parent(n).is_some() {
                tree.remove(n);
                self.mark_detached(n);
            }
            Ok(())
        })
    }

    /// Puts `items`, in order, into `parent`: at position `at` among its
    /// children (counting text; negative counts from the end; past either
    /// end is the start or the end), or at the end. A node that is in a
    /// tree is taken out of it first. The position is found inside the
    /// edit, so no other thread's edit can move it in between.
    ///
    /// # Safety
    /// As for [`Document::remove`].
    pub unsafe fn insert<'s, C: Copy + Into<Content<'s>>>(
        &self,
        parent: NodeId,
        at: Option<isize>,
        items: &[C],
    ) -> Result<(), EditError> {
        let p = parent.get();
        self.edit(|| {
            let before = at.and_then(|at| {
                let children: Vec<u32> = self.tree().children(p).collect();
                let i = if at < 0 {
                    at + children.len() as isize
                } else {
                    at
                };
                children.get(i.max(0) as usize).copied()
            });
            unsafe { self.link(p, before, items) }
        })
    }

    /// Puts `with` where `node` is, and removes `node`.
    ///
    /// # Safety
    /// As for [`Document::remove`].
    pub unsafe fn replace<'s, C: Copy + Into<Content<'s>>>(
        &self,
        node: NodeId,
        with: &[C],
    ) -> Result<(), EditError> {
        let n = node.get();
        self.edit(|| {
            let Some(parent) = self.tree().parent(n) else {
                return invalid("a node with no parent can't be replaced");
            };
            if with.iter().any(|&c| c.into() == Content::Node(node)) {
                return invalid("a node can't be replaced with itself");
            }
            unsafe { self.link(parent, Some(n), with)? };
            unsafe { self.tree_mut() }.remove(n);
            self.mark_detached(n);
            Ok(())
        })
    }

    /// Replaces `node` with its children, leaving it removed and empty.
    ///
    /// # Safety
    /// As for [`Document::remove`].
    pub unsafe fn unwrap(&self, node: NodeId) -> Result<(), EditError> {
        let n = node.get();
        self.edit(|| {
            let tree = unsafe { self.tree_mut() };
            let Some(parent) = tree.parent(n) else {
                return invalid("a node with no parent can't be unwrapped");
            };
            while let Some(child) = tree.first_child(n) {
                tree.insert_before(parent, child, Some(n))
                    .expect("a child moves to beside its parent");
            }
            tree.remove(n);
            self.mark_detached(n);
            Ok(())
        })
    }

    /// Replaces everything in the element `node` with one text node.
    ///
    /// # Safety
    /// As for [`Document::remove`].
    pub unsafe fn set_text(&self, node: NodeId, text: &str) -> Result<(), EditError> {
        let n = node.get();
        self.edit(|| {
            if Node::from_raw(self, n).kind() != NodeKind::Element {
                return invalid("only an element's text can be set");
            }
            let tag = Node::from_raw(self, n).tag().unwrap_or_default();
            if VOID.contains(&tag) {
                return invalid(format!("<{tag}> can't hold text"));
            }
            let tree = unsafe { self.tree_mut() };
            let t = tree.create_text(text);
            while let Some(child) = tree.first_child(n) {
                tree.remove(child);
                self.mark_detached(child);
            }
            tree.append(n, t).expect("an element takes text");
            Ok(())
        })
    }

    /// A new element, not yet in the tree: place it with
    /// [`Document::insert`]. In HTML names are lowercased; in XML they're
    /// kept as given, and the element has no namespace.
    ///
    /// # Safety
    /// As for [`Document::remove`].
    pub unsafe fn create_element(
        &self,
        tag: &str,
        attrs: &[(&str, &str)],
    ) -> Result<NodeId, EditError> {
        let valid_tag = tag.starts_with(|c: char| c.is_ascii_alphabetic())
            && tag
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
        if !valid_tag {
            return invalid(format!("{tag:?} isn't a valid tag name"));
        }
        // What the HTML spec allows in an attribute name.
        let bad_attr = attrs.iter().find(|(name, _)| {
            name.is_empty()
                || name.chars().any(|c| {
                    c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '>' | '/' | '=')
                })
        });
        if let Some((name, _)) = bad_attr {
            return invalid(format!("{name:?} isn't a valid attribute name"));
        }
        if self.is_xml() {
            return self.create_xml_element(tag, attrs);
        }
        self.edit(|| {
            let tree = unsafe { self.tree_mut() };
            let n = tree.create_element(html::Ns::Html, tag);
            for (name, value) in attrs {
                tree.set_attr(n, name, value);
            }
            self.mark_detached(n);
            Ok(NodeId(n as usize))
        })
    }

    /// [`Document::create_element`] in an XML document.
    fn create_xml_element(&self, tag: &str, attrs: &[(&str, &str)]) -> Result<NodeId, EditError> {
        // A prefix would need a namespace to go with it.
        if !xml::is_ncname(tag) {
            return invalid(format!(
                "{tag:?} isn't a valid XML element name (without a prefix)"
            ));
        }
        for (i, (name, _)) in attrs.iter().enumerate() {
            if !xml::is_ncname(name) {
                return invalid(format!(
                    "{name:?} isn't a valid XML attribute name (without a prefix)"
                ));
            }
            if attrs[..i].iter().any(|(other, _)| other == name) {
                return invalid(format!("the attribute {name} is given twice"));
            }
        }
        self.edit(|| {
            let tree = unsafe { self.tree_mut() };
            let n = tree.create_element(html::Ns::Xml, tag);
            for (name, value) in attrs {
                tree.add_attr(n, html::AttrNs::None, name, value);
            }
            self.mark_detached(n);
            Ok(NodeId(n as usize))
        })
    }

    /// A new text node, not yet in the tree.
    ///
    /// # Safety
    /// As for [`Document::remove`].
    pub unsafe fn create_text(&self, text: &str) -> Result<NodeId, EditError> {
        self.edit(|| {
            let n = unsafe { self.tree_mut() }.create_text(text);
            self.mark_detached(n);
            Ok(NodeId(n as usize))
        })
    }

    /// Puts `items` into `parent` before its child `before` (or at the
    /// end), without taking the document. Checks every node first and
    /// makes the text last, so a failure changes nothing.
    unsafe fn link<'s, C: Copy + Into<Content<'s>>>(
        &self,
        parent: u32,
        before: Option<u32>,
        items: &[C],
    ) -> Result<(), EditError> {
        let kind = |n: u32| Node::from_raw(self, n).kind();
        if !matches!(kind(parent), NodeKind::Element | NodeKind::Document) {
            return invalid("only an element or the document can have children");
        }
        let tree = self.tree();
        if before.is_some_and(|b| tree.parent(b) != Some(parent)) {
            return invalid("the node to insert before isn't a child of the parent");
        }
        let given = || {
            items.iter().filter_map(|&c| match c.into() {
                Content::Node(id) => Some(id.get()),
                Content::Text(_) => None,
            })
        };
        for n in given() {
            if !matches!(
                kind(n),
                NodeKind::Element | NodeKind::Text | NodeKind::Comment
            ) {
                return invalid("only elements, text and comments can be inserted");
            }
            // A node can't go inside itself.
            let mut up = Some(parent);
            while let Some(u) = up {
                if u == n {
                    return invalid("a node can't be put inside itself");
                }
                up = tree.parent(u);
            }
        }
        // Inserting before a node that is itself being moved means before
        // the first one after it that isn't.
        let mut before = before;
        while let Some(b) = before.filter(|b| given().any(|n| n == *b)) {
            before = tree.next_sibling(b);
        }
        let tree = unsafe { self.tree_mut() };
        for &item in items {
            let n = match item.into() {
                Content::Node(id) => id.get(),
                Content::Text(text) => tree.create_text(text),
            };
            tree.insert_before(parent, n, before)
                .expect("checked above");
        }
        Ok(())
    }

    /// Rebuilds a node from an id returned by [`Node::id`].
    ///
    /// # Safety
    /// `id` must come from a node of this same document.
    pub unsafe fn node(&self, id: NodeId) -> Node<'_> {
        Node {
            doc: self,
            id: id.get(),
        }
    }
}

impl Document {
    fn wrap(tree: html::Document, xml: Option<Box<XmlNames>>) -> Document {
        Document {
            tree: UnsafeCell::new(tree),
            xml: UnsafeCell::new(xml),
            index: UnsafeCell::new(OnceLock::new()),
            detached: Mutex::new(Vec::new()),
            access: AtomicU32::new(0),
            readers: std::array::from_fn(|_| Slot(AtomicU32::new(0))),
            queue: AtomicU64::new(0),
            turn_left: AtomicU32::new(0),
            #[cfg(test)]
            turn_entries: AtomicU32::new(0),
            #[cfg(test)]
            most_turn_entries: AtomicU32::new(0),
            #[cfg(test)]
            wakes: AtomicU32::new(0),
            gate: Mutex::new(()),
            woken: Condvar::new(),
        }
    }

    /// An empty XML document, for the XML parser to fill.
    pub(crate) fn new_xml() -> Document {
        Document::wrap(html::Document::empty(), Some(Box::default()))
    }

    /// The tree, to build an XML document into before anyone reads it.
    pub(crate) fn tree_to_build(&mut self) -> &mut html::Document {
        self.tree.get_mut()
    }

    /// Whether this is an XML document (see [`Document::parse_xml`]) rather
    /// than an HTML one.
    pub fn is_xml(&self) -> bool {
        self.xml_names().is_some()
    }

    pub(crate) fn xml_names(&self) -> Option<&XmlNames> {
        // SAFETY: as for the index, only an edit changes it.
        unsafe { &*self.xml.get() }.as_deref()
    }

    pub(crate) fn xml_names_mut(&mut self) -> &mut XmlNames {
        self.xml
            .get_mut()
            .as_deref_mut()
            .expect("only an XML document has XML names")
    }

    /// Takes every element and attribute out of its namespace, and drops
    /// the namespace declarations, so that queries can use plain names:
    /// `//entry` rather than `//atom:entry`. Prefixes go too (`dc:title`
    /// becomes `title`); where that leaves an element two attributes of
    /// one name, the one that had the prefix wins. Scrapy's (parsel's)
    /// `remove_namespaces()`. Does nothing to an HTML document.
    ///
    /// # Safety
    /// As for [`Document::remove`].
    pub unsafe fn remove_namespaces(&self) -> Result<(), EditError> {
        if !self.is_xml() {
            return Ok(());
        }
        self.edit(|| {
            for root in self.roots() {
                let top = Node {
                    doc: self,
                    id: root,
                };
                let elements: Vec<u32> = std::iter::once(top)
                    .chain(top.descendants())
                    .filter(|n| n.kind() == NodeKind::Element)
                    .map(|n| n.id)
                    .collect();
                for n in elements {
                    unsafe { strip_prefixes(self, n) };
                }
            }
            // SAFETY: this edit has the document to itself.
            if let Some(names) = unsafe { &mut *self.xml.get() } {
                names.clear();
            }
            Ok(())
        })
    }
}

/// Renames an element and its attributes without their prefixes. Only an
/// edit may call it.
unsafe fn strip_prefixes(doc: &Document, n: u32) {
    let tree = unsafe { doc.tree_mut() };
    let qname = tree.lower_name(n).unwrap_or("").to_string();
    if let Some((_, local)) = qname.split_once(':') {
        tree.rename(n, local);
    }
    let attrs: Vec<(String, String)> = tree
        .attrs_written(n)
        .map(|(_, name, value)| (name.to_string(), value.to_string()))
        .collect();
    // Each name's attribute once the prefixes are gone: the plain ones
    // first, then the prefixed in order, each taking the place of any
    // attribute already of that name.
    let local = |name: &str| name.split_once(':').map_or(name, |(_, l)| l).to_string();
    let mut by_name: std::collections::HashMap<String, usize> = attrs
        .iter()
        .enumerate()
        .filter(|(_, (name, _))| !name.contains(':'))
        .map(|(i, (name, _))| (name.clone(), i))
        .collect();
    let mut gone = vec![false; attrs.len()];
    for (i, (name, _)) in attrs.iter().enumerate() {
        if name.contains(':')
            && let Some(old) = by_name.insert(local(name), i)
        {
            gone[old] = true;
        }
    }
    let names: Vec<String> = attrs.iter().map(|(name, _)| local(name)).collect();
    let kept: Vec<(html::AttrNs, &str, &str)> = attrs
        .iter()
        .zip(&names)
        .zip(&gone)
        .filter(|(_, gone)| !**gone)
        .map(|(((_, value), name), _)| (html::AttrNs::None, name.as_str(), value.as_str()))
        .collect();
    tree.set_attrs(n, &kept);
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Document")
    }
}

/// Returned by [`Document::parse_within`] when parsing ran out of time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseTimeout {
    pub budget: Duration,
    /// Bytes parsed before giving up.
    pub parsed: usize,
}

impl fmt::Display for ParseTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "parsing took longer than {:?} (gave up after {} bytes)",
            self.budget, self.parsed
        )
    }
}

impl std::error::Error for ParseTimeout {}

/// A node's identity, stable for the life of its document. Lets code that
/// cannot hold a borrow (such as the Python bindings) refer to a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(usize);

impl NodeId {
    fn get(self) -> u32 {
        self.0 as u32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Document,
    Doctype,
    Element,
    Text,
    Comment,
    /// `<?target data?>`, in XML documents (HTML parsing makes them
    /// comments).
    ProcessingInstruction,
    Other,
}

#[derive(Clone, Copy)]
pub struct Node<'a> {
    pub(crate) doc: &'a Document,
    pub(crate) id: u32,
}

impl<'a> Node<'a> {
    pub(crate) fn from_raw(doc: &'a Document, id: u32) -> Node<'a> {
        Node { doc, id }
    }

    fn tree(&self) -> &'a html::Document {
        self.doc.tree()
    }

    fn wrap(&self, id: Option<u32>) -> Option<Node<'a>> {
        id.map(|id| Node { doc: self.doc, id })
    }

    pub fn id(&self) -> NodeId {
        NodeId(self.id as usize)
    }

    pub fn kind(&self) -> NodeKind {
        match self.tree().kind(self.id) {
            html::Kind::Element { .. } => NodeKind::Element,
            html::Kind::Text(_) | html::Kind::Cdata(_) => NodeKind::Text,
            html::Kind::Comment(_) => NodeKind::Comment,
            html::Kind::ProcessingInstruction { .. } => NodeKind::ProcessingInstruction,
            html::Kind::Document => NodeKind::Document,
            html::Kind::Doctype { .. } => NodeKind::Doctype,
            html::Kind::Fragment => NodeKind::Other,
        }
    }

    /// The tag name, for elements: lowercase in HTML, and in XML as
    /// written, without its prefix.
    pub fn tag(&self) -> Option<&'a str> {
        let name = self.tree().lower_name(self.id)?;
        if self.doc.is_xml() {
            return Some(name.split_once(':').map_or(name, |(_, local)| local));
        }
        Some(name)
    }

    /// An element's name with its prefix, as written (`atom:entry`); the
    /// same as [`Node::tag`] in HTML.
    pub fn qualified_name(&self) -> &'a str {
        self.tree().lower_name(self.id).unwrap_or("")
    }

    /// The namespace URI of an element in an XML document; None outside
    /// any namespace, and always for HTML.
    pub fn namespace(&self) -> Option<&'a str> {
        let names = self.doc.xml_names()?;
        match names.ns_of(self.id as usize) {
            0 => None,
            id => Some(names.uri(id)),
        }
    }

    /// The namespace id of an element (0 for none, and in HTML), for
    /// comparing with [`XmlNames::id_of`].
    pub(crate) fn ns_id(&self) -> u32 {
        self.doc
            .xml_names()
            .map_or(0, |names| names.ns_of(self.id as usize))
    }

    /// The target of a processing instruction (`xml-stylesheet`).
    pub fn pi_target(&self) -> Option<&'a str> {
        match self.tree().kind(self.id) {
            html::Kind::ProcessingInstruction { target, .. } => Some(target),
            _ => None,
        }
    }

    /// An element's attributes with what XPath needs of them: the name
    /// (as written in XML, as [`Node::attrs`] gives it in HTML), its
    /// namespace id (0 for none, and in HTML) and the value.
    pub(crate) fn xml_attrs(&self) -> Vec<(&'a str, u32, &'a str)> {
        self.tree()
            .attrs(self.id)
            .map(|(ns, name, value)| {
                let ns = match ns {
                    html::AttrNs::Host(id) => id,
                    _ => 0,
                };
                (name, ns, value)
            })
            .collect()
    }

    pub fn attr(&self, name: &str) -> Option<&'a str> {
        let mut attrs = self.tree().attrs(self.id);
        if self.doc.is_xml() {
            return attrs.find(|(_, n, _)| *n == name).map(|(_, _, v)| v);
        }
        // HTML names are lowercase; the one asked for may not be.
        attrs
            .find(|(_, n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, _, v)| v)
    }

    /// Every attribute as `(name, value)`, in source order. In XML a name
    /// keeps its prefix as written (`xml:lang`). In HTML, lowercase as
    /// tokenized (SVG's `viewbox`, as lxml has it), and a namespaced one
    /// by its local name (`href` for `xlink:href`).
    pub fn attrs(&self) -> Vec<(&'a str, &'a str)> {
        self.tree()
            .attrs(self.id)
            .map(|(_, name, value)| (name, value))
            .collect()
    }

    /// The node's own text, for text and comment nodes, and a processing
    /// instruction's data.
    pub fn data(&self) -> Option<&'a str> {
        match self.tree().kind(self.id) {
            html::Kind::Text(t) | html::Kind::Cdata(t) | html::Kind::Comment(t) => Some(t),
            html::Kind::ProcessingInstruction { data, .. } => Some(data),
            _ => None,
        }
    }

    /// An id for this node's name. Two elements share an id exactly when
    /// their lowercase names (without an XML prefix) are equal.
    pub(crate) fn tag_id(&self) -> usize {
        let index = self.doc.index();
        index.tag_at(index.order(self.id))
    }

    /// The id elements named `name` (compared lowercased) carry in this
    /// node's document; `None` if no element has that name.
    pub(crate) fn tag_id_named(&self, name: &str) -> Option<usize> {
        self.doc.index().tag_id_named(name)
    }

    pub fn next_sibling(&self) -> Option<Node<'a>> {
        self.wrap(self.tree().next_sibling(self.id))
    }

    pub fn prev_sibling(&self) -> Option<Node<'a>> {
        self.wrap(self.tree().prev_sibling(self.id))
    }

    pub fn first_child(&self) -> Option<Node<'a>> {
        self.wrap(self.tree().first_child(self.id))
    }

    pub fn last_child(&self) -> Option<Node<'a>> {
        self.wrap(self.tree().last_child(self.id))
    }

    /// The node after this one in document order: its first child, or the
    /// next sibling of it or its nearest ancestor that has one.
    pub fn next_in_order(&self) -> Option<Node<'a>> {
        if let Some(child) = self.first_child() {
            return Some(child);
        }
        let mut at = *self;
        loop {
            if let Some(next) = at.next_sibling() {
                return Some(next);
            }
            at = at.parent()?;
        }
    }

    /// The node before this one in document order: the last node inside its
    /// previous sibling, or else its parent.
    pub fn prev_in_order(&self) -> Option<Node<'a>> {
        let Some(mut at) = self.prev_sibling() else {
            return self.parent();
        };
        while let Some(last) = at.last_child() {
            at = last;
        }
        Some(at)
    }

    /// The node's position in document order: smaller comes first.
    pub fn order(&self) -> u32 {
        self.doc.order_of(self.id)
    }

    /// The node as HTML (as XML, in an XML document): an element with its
    /// attributes and everything in it, a text node escaped, a comment as
    /// `<!--...-->`.
    pub fn html(&self) -> String {
        if let Some(names) = self.doc.xml_names() {
            return xml::serialize(*self, names);
        }
        self.tree().outer_html(self.id)
    }

    /// The node alone as HTML: an element's start tag, escaped text, a
    /// comment or a doctype.
    fn html_alone(&self) -> String {
        self.tree().html_alone(self.id)
    }

    /// The node as indented HTML, one tag or piece of text to a line, one
    /// space of indent a level, as Beautiful Soup's `prettify()` lays it
    /// out. Text is trimmed and whitespace-only text left out, except in
    /// `<pre>`, `<textarea>`, `<script>` and `<style>`, which are written
    /// as they are.
    pub fn pretty(&self) -> String {
        if let Some(names) = self.doc.xml_names() {
            return xml::pretty(*self, names);
        }
        let mut out = String::new();
        self.pretty_into(0, &mut out);
        out
    }

    fn pretty_into(&self, depth: usize, out: &mut String) {
        // Where whitespace matters, or the content is raw text.
        const AS_IS: [&str; 9] = [
            "pre",
            "textarea",
            "script",
            "style",
            "xmp",
            "iframe",
            "noembed",
            "noframes",
            "plaintext",
        ];
        let line = |out: &mut String, text: &str| {
            out.extend(std::iter::repeat_n(' ', depth));
            out.push_str(text);
            out.push('\n');
        };
        match (self.kind(), self.tag()) {
            (NodeKind::Document, _) => {
                for child in self.children() {
                    child.pretty_into(depth, out);
                }
            }
            (NodeKind::Element, Some(tag)) if AS_IS.contains(&tag) => line(out, &self.html()),
            (NodeKind::Element, Some(tag)) => {
                let start = self.html_alone();
                line(out, &start);
                if VOID.contains(&tag) {
                    return;
                }
                for child in self.children() {
                    child.pretty_into(depth + 1, out);
                }
                // The name as the start tag has it: `tag()` lowercases
                // SVG's `foreignObject`.
                let name = start[1..]
                    .split(|c: char| c.is_ascii_whitespace() || c == '>')
                    .next()
                    .unwrap_or(tag);
                line(out, &format!("</{name}>"));
            }
            (NodeKind::Text, _) => {
                let text = self.html_alone();
                if !text.trim().is_empty() {
                    line(out, text.trim());
                }
            }
            _ => line(out, &self.html_alone()),
        }
    }

    pub fn parent(&self) -> Option<Node<'a>> {
        self.wrap(self.tree().parent(self.id))
    }

    pub fn children(&self) -> impl Iterator<Item = Node<'a>> + use<'a> {
        // The tree is looked up afresh at each step: an edit on this
        // thread may change it between them.
        let doc = self.doc;
        let mut next = doc.tree().first_child(self.id);
        std::iter::from_fn(move || {
            let id = next?;
            next = doc.tree().next_sibling(id);
            Some(Node { doc, id })
        })
    }

    /// Every node below this one, in document order.
    pub fn descendants(&self) -> impl Iterator<Item = Node<'a>> + use<'a> {
        let doc = self.doc;
        let top = self.id;
        let mut next = doc.tree().first_child(top);
        std::iter::from_fn(move || {
            let tree = doc.tree();
            let current = next?;
            next = tree.first_child(current);
            let mut up = Some(current);
            while next.is_none() {
                let Some(u) = up.filter(|&u| u != top) else {
                    break;
                };
                next = tree.next_sibling(u);
                up = tree.parent(u);
                // An edit on this thread took `current` out of the tree
                // mid-walk: stop rather than climb past its old place.
                if up.is_none() {
                    break;
                }
            }
            Some(Node { doc, id: current })
        })
    }

    /// The text nodes below this node, in order, leaving out what is code
    /// rather than reading matter: the contents of `<script>`, `<style>`
    /// and `<template>` (Beautiful Soup's `get_text()` does the same). On a
    /// script or style element itself, gives its own text.
    pub fn readable_text_pieces(&self) -> Vec<&'a str> {
        const CODE: [&str; 3] = ["script", "style", "template"];
        // In XML no element is code.
        if self.doc.is_xml() {
            return self.text_pieces();
        }
        if self.tag().is_some_and(|t| CODE.contains(&t)) || self.kind() == NodeKind::Text {
            return self.text_pieces();
        }
        let mut out = Vec::new();
        let mut next = self.first_child();
        while let Some(n) = next {
            let skip = n.tag().is_some_and(|t| CODE.contains(&t));
            if !skip && n.kind() == NodeKind::Text {
                out.extend(n.data());
            }
            // Descend unless skipping; otherwise move on past this subtree.
            next = if !skip { n.first_child() } else { None }.or_else(|| {
                let mut at = n;
                loop {
                    if at.id == self.id {
                        return None;
                    }
                    if let Some(s) = at.next_sibling() {
                        return Some(s);
                    }
                    at = at.parent()?;
                    if at.id == self.id {
                        return None;
                    }
                }
            });
        }
        out
    }

    /// Every text node's text below (or at) this node, in order.
    fn text_pieces(&self) -> Vec<&'a str> {
        std::iter::once(*self)
            .chain(self.descendants())
            .filter(|n| n.kind() == NodeKind::Text)
            .filter_map(|n| n.data())
            .collect()
    }

    /// All text below this node, joined. Comments are not text.
    pub fn text(&self) -> String {
        if let (NodeKind::Text, Some(d)) = (self.kind(), self.data()) {
            return d.to_string();
        }
        self.descendants()
            .filter(|n| n.kind() == NodeKind::Text)
            .filter_map(|n| n.data())
            .collect()
    }
}

impl PartialEq for Node<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && std::ptr::eq(self.doc, other.doc)
    }
}

impl fmt::Debug for Node<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind() {
            NodeKind::Element => write!(f, "<{}>", self.tag().unwrap_or("?")),
            NodeKind::Text => write!(f, "{:?}", self.data().unwrap_or("")),
            kind => write!(f, "{kind:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    /// Edits whose sleep ran out instead of being woken.
    pub(super) static TIMEOUTS: AtomicUsize = AtomicUsize::new(0);
    /// Edits that slept at all, so the test knows it tested something.
    pub(super) static WAITS: AtomicUsize = AtomicUsize::new(0);
    /// Sleeps that ran out with the way clear and no wake-up ever sent.
    pub(super) static LOST: AtomicUsize = AtomicUsize::new(0);

    /// Several edits waiting at once, against short and long reads: each
    /// must be woken, never left to sleep out its timeout.
    /// Readers arriving in a stream, each read a new one, against an edit
    /// waiting to get in: a turn lets in at most the readers that were
    /// waiting when it began (one a thread), never the newcomers after them,
    /// so the edit's wait has a bound.
    #[test]
    fn a_readers_turn_admits_only_the_readers_already_waiting() {
        const READERS: u32 = 8;
        let doc = Arc::new(Document::parse(&"<p>x</p>".repeat(200)));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                let (doc, stop) = (doc.clone(), stop.clone());
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let _reading = doc.read();
                        std::hint::black_box(doc.root().first_child());
                    }
                })
            })
            .collect();
        let mut slowest = Duration::ZERO;
        for _ in 0..2000 {
            let start = Instant::now();
            // SAFETY: the readers hold a Reading.
            unsafe {
                let p = doc.create_element("p", &[]).unwrap();
                doc.remove(p).unwrap();
            }
            slowest = slowest.max(start.elapsed() / 2);
        }
        stop.store(true, Ordering::Relaxed);
        for r in readers {
            r.join().unwrap();
        }
        let most = doc.most_turn_entries.load(Ordering::Relaxed);
        assert!(most <= READERS, "one turn let in {most} readers");
        assert!(
            slowest < Duration::from_millis(200),
            "an edit waited {slowest:?}"
        );
    }

    /// A reader that came in the fast way, counted only in its thread's
    /// slot, holds an edit off just as one counted in `access` does.
    #[test]
    fn an_edit_waits_for_a_reader_counted_in_its_slot() {
        let doc = Arc::new(Document::parse("<p>x</p>"));
        let reading = doc.read();
        assert!(
            reading.slot.is_some(),
            "no edit was about, yet it came in slowly"
        );
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let edit = {
            let (doc, done) = (doc.clone(), done.clone());
            std::thread::spawn(move || {
                // SAFETY: the reader holds a Reading.
                unsafe {
                    let p = doc.create_element("p", &[]).unwrap();
                    doc.remove(p).unwrap();
                }
                done.store(true, Ordering::SeqCst);
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !done.load(Ordering::SeqCst),
            "the edit ran while a reader was in"
        );
        drop(reading);
        edit.join().unwrap();
        assert!(done.load(Ordering::SeqCst));
    }

    /// More threads than slots, so readers share them, against edits: each
    /// read sees a whole tree (the page, plus at most one paragraph per
    /// editor mid-edit), and no edit sleeps unwoken.
    #[test]
    fn readers_sharing_slots_and_edits_agree() {
        const EDITORS: usize = 4;
        const THREADS: usize = SLOTS + 8;
        let doc = Arc::new(Document::parse(&"<p>x</p>".repeat(100)));
        let count = |d: &Document| {
            d.root()
                .descendants()
                .filter(|n| n.tag() == Some("p"))
                .count()
        };
        let base = count(&doc);
        let body = doc.root().descendants().find(|n| n.tag() == Some("body"));
        let body = body.unwrap().id();
        let lost = LOST.load(Ordering::Relaxed);
        let threads: Vec<_> = (0..THREADS)
            .map(|t| {
                let doc = doc.clone();
                std::thread::spawn(move || {
                    for _ in 0..2_000 {
                        if t < EDITORS {
                            // SAFETY: the readers hold a Reading.
                            unsafe {
                                let p = doc.create_element("p", &[]).unwrap();
                                doc.insert(body, None, &[p]).unwrap();
                                doc.remove(p).unwrap();
                            }
                        } else {
                            let _reading = doc.read();
                            let n = count(&doc);
                            assert!((base..=base + EDITORS).contains(&n), "read {n} paragraphs");
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(count(&doc), base);
        assert_eq!(
            LOST.load(Ordering::Relaxed),
            lost,
            "a waiting edit was never woken"
        );
    }

    #[test]
    fn waiting_edits_are_always_woken() {
        // How often edits have to sleep depends on the machine: one pass
        // over these mixes made 58 sleeps on a macOS runner. Passes repeat
        // until there have been enough to say something, for a minute at
        // most.
        let enough = Instant::now() + Duration::from_secs(60);
        while WAITS.load(Ordering::Relaxed) <= 100 && Instant::now() < enough {
            woken_pass();
        }
        let (waits, timeouts, lost) = (
            WAITS.load(Ordering::Relaxed),
            TIMEOUTS.load(Ordering::Relaxed),
            LOST.load(Ordering::Relaxed),
        );
        // Sleeps that outlast a descheduled reader are fine (CI machines
        // have fewer cores than this test has threads); one nobody woke
        // isn't.
        println!("{waits} sleeps, {timeouts} ran out, {lost} wake-ups lost");
        assert!(waits > 100, "edits never had to sleep");
        assert_eq!(lost, 0, "{lost} of {waits} sleeps were never woken");
    }

    /// Writers and readers, short reads and long, in turn.
    fn woken_pass() {
        for (writers, long_reads) in [(2, false), (4, false), (2, true), (4, true)] {
            let doc = Arc::new(Document::parse(&"<p>x</p>".repeat(200)));
            let body = doc.root().descendants().find(|n| n.tag() == Some("body"));
            let body = body.unwrap().id();
            let threads: Vec<_> = (0..8)
                .map(|t| {
                    let doc = doc.clone();
                    std::thread::spawn(move || {
                        for _ in 0..10_000 {
                            if t < writers {
                                // SAFETY: the readers hold a Reading.
                                unsafe {
                                    let p = doc.create_element("p", &[]).unwrap();
                                    doc.insert(body, None, &[p]).unwrap();
                                    doc.remove(p).unwrap();
                                }
                            } else {
                                let _reading = doc.read();
                                if long_reads {
                                    std::hint::black_box(doc.root().descendants().count());
                                }
                            }
                        }
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }
        }
    }
}
