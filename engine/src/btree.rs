//! A copy-on-write B+tree of byte-string keys and values.
//!
//! A tree is named by its root page. A change never touches a page another
//! reader can see: it writes new pages along the path from the leaf to the
//! root and hands back the new root, so the old root still names the tree
//! as it was. Pages a writer made itself in the same transaction are
//! replaced in place.
//!
//! A tree page is the header, an array of 16-bit cell offsets, then the
//! cells packed in key order. Header bytes 6..8 hold the cell count and
//! 8..10 the end of the last cell.
//!
//! ```text
//! leaf cell    varint key length, varint value length, key, value
//! branch cell  child page (u32), varint key length, key
//! ```
//!
//! A branch cell's key is the lowest key its child may hold. The first
//! cell's key is never read: that child takes everything below the second
//! key. Leaves are never empty and hold no links to their neighbours, so a
//! cursor keeps the path it came down by.

use crate::error::{Error, Result};
use crate::page::*;

/// Room for cells and their offsets in one page.
const CAPACITY: usize = PAGE_SIZE - HEADER;
/// The largest cell. Four always fit in a page, so a split can never leave
/// a page that overflows.
pub const MAX_CELL: usize = CAPACITY / 4 - 2;
/// The longest key: nearly a whole cell, as a branch holds a key and a
/// page number and an index entry a key and little else. A table row with
/// a key this long has almost no room left beside it.
pub const MAX_KEY: usize = 1900;

/// The longest value that can sit beside a key of `key_len` bytes.
pub const fn max_value(key_len: usize) -> usize {
    MAX_CELL - key_len - varint_len(key_len as u64) - 2
}

pub trait Pages {
    fn page(&self, id: PageId) -> Result<Page>;
}

pub trait PagesMut: Pages {
    /// Stores a new page, visible to this writer only, and names it.
    fn alloc(&mut self, buf: Vec<u8>) -> Result<PageId>;
    /// Whether this writer made the page and may still replace it.
    fn is_private(&self, id: PageId) -> bool;
    fn replace(&mut self, id: PageId, buf: Vec<u8>);
    /// Says this writer's tree no longer uses the page.
    fn free(&mut self, id: PageId);
}

// ---------------------------------------------------------------- nodes

#[derive(Clone, Copy)]
struct Node<'a> {
    page: &'a [u8],
}

impl<'a> Node<'a> {
    fn new(page: &'a [u8], id: PageId) -> Result<Self> {
        let k = kind(page);
        if k != KIND_LEAF && k != KIND_BRANCH {
            return Err(Error::corrupt(format!("page {id} is not a tree page (kind {k})")));
        }
        Ok(Node { page })
    }
    fn is_leaf(&self) -> bool {
        kind(self.page) == KIND_LEAF
    }
    fn len(&self) -> usize {
        get_u16(self.page, 6) as usize
    }
    fn offset(&self, i: usize) -> usize {
        get_u16(self.page, HEADER + 2 * i) as usize
    }
    fn end(&self) -> usize {
        get_u16(self.page, 8) as usize
    }
    fn cell(&self, i: usize) -> &'a [u8] {
        let end = if i + 1 < self.len() { self.offset(i + 1) } else { self.end() };
        &self.page[self.offset(i)..end]
    }
    fn cells(&self) -> Vec<&'a [u8]> {
        (0..self.len()).map(|i| self.cell(i)).collect()
    }
    fn key(&self, i: usize) -> &'a [u8] {
        let cell = self.cell(i);
        if self.is_leaf() {
            leaf_parts(cell).0
        } else {
            branch_key(cell)
        }
    }
    fn value(&self, i: usize) -> &'a [u8] {
        leaf_parts(self.cell(i)).1
    }
    fn child(&self, i: usize) -> PageId {
        get_u32(self.cell(i), 0)
    }
    /// In a leaf: where `key` is, or where it would go.
    fn search(&self, key: &[u8]) -> (usize, bool) {
        let (mut lo, mut hi) = (0, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.key(mid).cmp(key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return (mid, true),
            }
        }
        (lo, false)
    }
    /// In a branch: the child whose range holds `key`.
    fn child_for(&self, key: &[u8]) -> usize {
        let (mut lo, mut hi) = (1, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.key(mid) <= key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo - 1
    }
}

fn leaf_parts(cell: &[u8]) -> (&[u8], &[u8]) {
    let mut pos = 0;
    let klen = get_varint(cell, &mut pos).unwrap_or(0) as usize;
    let vlen = get_varint(cell, &mut pos).unwrap_or(0) as usize;
    (&cell[pos..pos + klen], &cell[pos + klen..pos + klen + vlen])
}

fn branch_key(cell: &[u8]) -> &[u8] {
    let mut pos = 4;
    let klen = get_varint(cell, &mut pos).unwrap_or(0) as usize;
    &cell[pos..pos + klen]
}

fn leaf_cell(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut cell = Vec::with_capacity(key.len() + value.len() + 4);
    put_varint(&mut cell, key.len() as u64);
    put_varint(&mut cell, value.len() as u64);
    cell.extend_from_slice(key);
    cell.extend_from_slice(value);
    cell
}

fn branch_cell(child: PageId, key: &[u8]) -> Vec<u8> {
    let mut cell = Vec::with_capacity(key.len() + 6);
    cell.extend_from_slice(&child.to_le_bytes());
    put_varint(&mut cell, key.len() as u64);
    cell.extend_from_slice(key);
    cell
}

fn build(kind: u8, cells: &[&[u8]]) -> Vec<u8> {
    let mut buf = new_buf(kind);
    put_u16(&mut buf, 6, cells.len() as u16);
    let mut at = HEADER + 2 * cells.len();
    for (i, cell) in cells.iter().enumerate() {
        put_u16(&mut buf, HEADER + 2 * i, at as u16);
        buf[at..at + cell.len()].copy_from_slice(cell);
        at += cell.len();
    }
    put_u16(&mut buf, 8, at as u16);
    buf
}

fn size(cells: &[&[u8]]) -> usize {
    cells.iter().map(|c| c.len() + 2).sum()
}

/// Splits cells that overflow one page into runs that each fit, as even
/// as the cell sizes allow.
fn partition(cells: &[&[u8]]) -> Vec<std::ops::Range<usize>> {
    let total = size(cells);
    let mut groups = total.div_ceil(CAPACITY).max(2);
    loop {
        let target = total / groups;
        let mut ranges = Vec::with_capacity(groups);
        let (mut start, mut used) = (0, 0);
        let mut fits = true;
        for (i, cell) in cells.iter().enumerate() {
            let need = cell.len() + 2;
            let last_group = ranges.len() + 1 == groups;
            if i > start && !last_group && (used >= target || used + need > CAPACITY) {
                ranges.push(start..i);
                start = i;
                used = 0;
            }
            used += need;
            fits &= used <= CAPACITY;
        }
        ranges.push(start..cells.len());
        if fits {
            return ranges;
        }
        groups += 1;
    }
}

/// What a change did to a subtree, as its parent must see it.
enum Outcome {
    /// Still the same page number.
    Same,
    /// Now these pages, each with the lowest key it may hold. The first
    /// takes the place of the old page and its key is not used. Empty when
    /// the subtree is gone.
    Now(Vec<(Vec<u8>, PageId)>),
}

/// Writes a page's new cells: in place if the writer owns the page, to a
/// new page otherwise, to several if they no longer fit in one.
fn write_node<P: PagesMut>(p: &mut P, id: PageId, kind: u8, cells: &[&[u8]], appended: bool) -> Result<Outcome> {
    if cells.is_empty() {
        p.free(id);
        return Ok(Outcome::Now(Vec::new()));
    }
    let private = p.is_private(id);
    if size(cells) <= CAPACITY {
        let buf = build(kind, cells);
        if private {
            p.replace(id, buf);
            return Ok(Outcome::Same);
        }
        let new = p.alloc(buf)?;
        p.free(id);
        return Ok(Outcome::Now(vec![(Vec::new(), new)]));
    }
    // A key added at the far end is most often the next of a rising
    // sequence: leave the full page as it is and start a new one, so pages
    // filled in order stay full.
    let ranges = if appended && size(&cells[..cells.len() - 1]) <= CAPACITY {
        vec![0..cells.len() - 1, cells.len() - 1..cells.len()]
    } else {
        partition(cells)
    };
    let mut pages = Vec::with_capacity(ranges.len());
    for (n, range) in ranges.into_iter().enumerate() {
        let group = &cells[range];
        let first_key = if kind == KIND_LEAF { leaf_parts(group[0]).0 } else { branch_key(group[0]) };
        let buf = if kind == KIND_BRANCH && n > 0 {
            // The first key of a branch is never read: store it empty.
            let head = branch_cell(get_u32(group[0], 0), &[]);
            let mut owned: Vec<&[u8]> = group.to_vec();
            owned[0] = &head;
            build(kind, &owned)
        } else {
            build(kind, group)
        };
        let page_id = if n == 0 && private {
            p.replace(id, buf);
            id
        } else {
            p.alloc(buf)?
        };
        pages.push((first_key.to_vec(), page_id));
    }
    if !private {
        p.free(id);
    }
    Ok(Outcome::Now(pages))
}

/// Rewrites a branch after its child `i` became `pages`.
fn replace_child<P: PagesMut>(p: &mut P, id: PageId, node: Node, i: usize, pages: Vec<(Vec<u8>, PageId)>) -> Result<Outcome> {
    let old_key = node.key(i);
    let owned: Vec<Vec<u8>> =
        pages.iter().enumerate().map(|(n, (key, child))| branch_cell(*child, if n == 0 { old_key } else { key })).collect();
    let mut cells = node.cells();
    cells.splice(i..=i, owned.iter().map(|c| c.as_slice()));
    write_node(p, id, KIND_BRANCH, &cells, false)
}

fn check_sizes(key: &[u8], value: &[u8]) -> Result<()> {
    if key.len() > MAX_KEY {
        return Err(Error::KeyTooLong(key.len()));
    }
    if value.len() > max_value(key.len()) {
        return Err(Error::corrupt(format!("value of {} bytes does not fit beside its key", value.len())));
    }
    Ok(())
}

// ------------------------------------------------------------ operations

pub fn get<P: Pages + ?Sized>(p: &P, root: PageId, key: &[u8]) -> Result<Option<Vec<u8>>> {
    if root == NO_PAGE {
        return Ok(None);
    }
    let mut id = root;
    loop {
        let page = p.page(id)?;
        let node = Node::new(&page, id)?;
        if node.is_leaf() {
            let (i, found) = node.search(key);
            return Ok(found.then(|| node.value(i).to_vec()));
        }
        id = node.child(node.child_for(key));
    }
}

/// Sets `key` to `value`. Returns the tree's new root and whether the key
/// was already there.
pub fn put<P: PagesMut>(p: &mut P, root: PageId, key: &[u8], value: &[u8]) -> Result<(PageId, bool)> {
    check_sizes(key, value)?;
    if root == NO_PAGE {
        let cell = leaf_cell(key, value);
        return Ok((p.alloc(build(KIND_LEAF, &[&cell]))?, false));
    }
    let (outcome, existed) = put_rec(p, root, key, value)?;
    let root = match outcome {
        Outcome::Same => root,
        Outcome::Now(pages) if pages.len() == 1 => pages[0].1,
        Outcome::Now(pages) => {
            let owned: Vec<Vec<u8>> =
                pages.iter().enumerate().map(|(n, (key, child))| branch_cell(*child, if n == 0 { &[] } else { key })).collect();
            let cells: Vec<&[u8]> = owned.iter().map(|c| c.as_slice()).collect();
            p.alloc(build(KIND_BRANCH, &cells))?
        }
    };
    Ok((root, existed))
}

fn put_rec<P: PagesMut>(p: &mut P, id: PageId, key: &[u8], value: &[u8]) -> Result<(Outcome, bool)> {
    let page = p.page(id)?;
    let node = Node::new(&page, id)?;
    if node.is_leaf() {
        let (i, found) = node.search(key);
        let cell = leaf_cell(key, value);
        let mut cells = node.cells();
        if found {
            cells[i] = &cell;
        } else {
            cells.insert(i, &cell);
        }
        let appended = !found && i + 1 == cells.len();
        return Ok((write_node(p, id, KIND_LEAF, &cells, appended)?, found));
    }
    let i = node.child_for(key);
    let (outcome, existed) = put_rec(p, node.child(i), key, value)?;
    match outcome {
        Outcome::Same => Ok((Outcome::Same, existed)),
        Outcome::Now(pages) => Ok((replace_child(p, id, node, i, pages)?, existed)),
    }
}

/// Removes `key`. Returns the tree's new root and whether the key was there.
pub fn delete<P: PagesMut>(p: &mut P, root: PageId, key: &[u8]) -> Result<(PageId, bool)> {
    if root == NO_PAGE {
        return Ok((root, false));
    }
    let (outcome, found) = delete_rec(p, root, key)?;
    let mut root = match outcome {
        Outcome::Same => root,
        Outcome::Now(pages) => pages.first().map_or(NO_PAGE, |(_, id)| *id),
    };
    // A branch left with one child adds nothing: the child becomes the root.
    while root != NO_PAGE {
        let page = p.page(root)?;
        let node = Node::new(&page, root)?;
        if node.is_leaf() || node.len() > 1 {
            break;
        }
        let child = node.child(0);
        p.free(root);
        root = child;
    }
    Ok((root, found))
}

fn delete_rec<P: PagesMut>(p: &mut P, id: PageId, key: &[u8]) -> Result<(Outcome, bool)> {
    let page = p.page(id)?;
    let node = Node::new(&page, id)?;
    if node.is_leaf() {
        let (i, found) = node.search(key);
        if !found {
            return Ok((Outcome::Same, false));
        }
        let mut cells = node.cells();
        cells.remove(i);
        return Ok((write_node(p, id, KIND_LEAF, &cells, false)?, true));
    }
    let i = node.child_for(key);
    let (outcome, found) = delete_rec(p, node.child(i), key)?;
    match outcome {
        Outcome::Same => Ok((Outcome::Same, found)),
        Outcome::Now(pages) => Ok((replace_child(p, id, node, i, pages)?, found)),
    }
}

/// Frees every page of a tree, and calls `on_value` with each value first
/// so the caller can free what the values point to.
pub fn destroy<P: PagesMut>(p: &mut P, root: PageId, on_value: &mut dyn FnMut(&mut P, &[u8]) -> Result<()>) -> Result<()> {
    if root == NO_PAGE {
        return Ok(());
    }
    let page = p.page(root)?;
    let node = Node::new(&page, root)?;
    for i in 0..node.len() {
        if node.is_leaf() {
            on_value(p, node.value(i))?;
        } else {
            destroy(p, node.child(i), on_value)?;
        }
    }
    p.free(root);
    Ok(())
}

/// Every page of a tree, for the consistency check. Calls `on_page` with
/// each page number and `on_entry` with each key and value in order.
pub fn walk<P: Pages + ?Sized>(
    p: &P,
    root: PageId,
    on_page: &mut dyn FnMut(PageId) -> Result<()>,
    on_entry: &mut dyn FnMut(&[u8], &[u8]) -> Result<()>,
) -> Result<()> {
    if root == NO_PAGE {
        return Ok(());
    }
    fn rec<P: Pages + ?Sized>(
        p: &P,
        id: PageId,
        low: Option<&[u8]>,
        on_page: &mut dyn FnMut(PageId) -> Result<()>,
        on_entry: &mut dyn FnMut(&[u8], &[u8]) -> Result<()>,
    ) -> Result<()> {
        on_page(id)?;
        let page = p.page(id)?;
        let node = Node::new(&page, id)?;
        if node.len() == 0 {
            return Err(Error::corrupt(format!("tree page {id} is empty")));
        }
        for i in 0..node.len() {
            if node.is_leaf() {
                if low.is_some_and(|low| node.key(i) < low) {
                    return Err(Error::corrupt(format!("page {id} holds a key below its range")));
                }
                if i > 0 && node.key(i - 1) >= node.key(i) {
                    return Err(Error::corrupt(format!("page {id} has keys out of order")));
                }
                on_entry(node.key(i), node.value(i))?;
            } else {
                if i > 1 && node.key(i - 1) >= node.key(i) {
                    return Err(Error::corrupt(format!("page {id} has keys out of order")));
                }
                let child_low = if i == 0 { low } else { Some(node.key(i)) };
                rec(p, node.child(i), child_low, on_page, on_entry)?;
            }
        }
        Ok(())
    }
    rec(p, root, None, on_page, on_entry)
}

// --------------------------------------------------------------- cursor

/// A position in a tree that can step to the next or previous entry. It
/// reads the tree as of the root it was given, whatever is written after.
pub struct Cursor<'p, P: Pages + ?Sized> {
    pages: &'p P,
    root: PageId,
    /// The pages from the root down to a leaf, each with the index of the
    /// cell the cursor is at or went down through. Empty when the cursor
    /// is off either end.
    path: Vec<(Page, usize)>,
}

impl<'p, P: Pages + ?Sized> Cursor<'p, P> {
    pub fn new(pages: &'p P, root: PageId) -> Self {
        Cursor { pages, root, path: Vec::new() }
    }

    pub fn valid(&self) -> bool {
        !self.path.is_empty()
    }

    fn leaf(&self) -> Option<(Node<'_>, usize)> {
        let (page, i) = self.path.last()?;
        Some((Node { page }, *i))
    }

    pub fn key(&self) -> Option<&[u8]> {
        self.leaf().map(|(node, i)| node.key(i))
    }

    pub fn value(&self) -> Option<&[u8]> {
        self.leaf().map(|(node, i)| node.value(i))
    }

    fn descend(&mut self, mut id: PageId, pick: &dyn Fn(Node) -> usize) -> Result<()> {
        loop {
            let page = self.pages.page(id)?;
            let node = Node::new(&page, id)?;
            let i = pick(node);
            let next = (!node.is_leaf()).then(|| node.child(i));
            self.path.push((page, i));
            match next {
                Some(child) => id = child,
                None => return Ok(()),
            }
        }
    }

    pub fn first(&mut self) -> Result<()> {
        self.path.clear();
        if self.root != NO_PAGE {
            self.descend(self.root, &|_| 0)?;
        }
        Ok(())
    }

    pub fn last(&mut self) -> Result<()> {
        self.path.clear();
        if self.root != NO_PAGE {
            self.descend(self.root, &|node| node.len() - 1)?;
        }
        Ok(())
    }

    /// Moves to the first entry whose key is `key` or greater.
    pub fn seek(&mut self, key: &[u8]) -> Result<()> {
        self.path.clear();
        if self.root == NO_PAGE {
            return Ok(());
        }
        self.descend(self.root, &|node| if node.is_leaf() { node.search(key).0 } else { node.child_for(key) })?;
        let (node, i) = self.leaf().unwrap();
        if i == node.len() {
            // Past this leaf's last entry: the next leaf's first, if any.
            self.path.last_mut().unwrap().1 = i - 1;
            self.next()?;
        }
        Ok(())
    }

    /// Moves to the last entry whose key is below `key`, or is `key` itself
    /// when `inclusive`.
    pub fn seek_back(&mut self, key: &[u8], inclusive: bool) -> Result<()> {
        self.seek(key)?;
        if !self.valid() {
            return self.last();
        }
        if !(inclusive && self.key() == Some(key)) {
            self.prev()?;
        }
        Ok(())
    }

    pub fn next(&mut self) -> Result<()> {
        let Some((page, i)) = self.path.last_mut() else { return Ok(()) };
        if *i + 1 < (Node { page }).len() {
            *i += 1;
            return Ok(());
        }
        self.path.pop();
        while let Some((page, i)) = self.path.last_mut() {
            let node = Node { page };
            if *i + 1 < node.len() {
                *i += 1;
                let child = node.child(*i);
                return self.descend(child, &|_| 0);
            }
            self.path.pop();
        }
        Ok(())
    }

    pub fn prev(&mut self) -> Result<()> {
        let Some((_, i)) = self.path.last_mut() else { return Ok(()) };
        if *i > 0 {
            *i -= 1;
            return Ok(());
        }
        self.path.pop();
        while let Some((page, i)) = self.path.last_mut() {
            if *i > 0 {
                *i -= 1;
                let child = (Node { page }).child(*i);
                return self.descend(child, &|node| node.len() - 1);
            }
            self.path.pop();
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::vfs::Rng;
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::sync::Arc;

    /// Pages in memory, with the bookkeeping to tell which a "transaction"
    /// owns and which pages a tree leaks.
    #[derive(Default)]
    pub(crate) struct MemPages {
        pub pages: HashMap<PageId, Page>,
        pub private: HashSet<PageId>,
        pub next: PageId,
        pub frees: Vec<PageId>,
    }

    impl MemPages {
        pub fn new() -> Self {
            MemPages { next: FIRST_DATA_PAGE, ..Default::default() }
        }
        /// Ends the "transaction": every page becomes shared.
        pub fn commit(&mut self) {
            self.private.clear();
        }
    }

    impl Pages for MemPages {
        fn page(&self, id: PageId) -> Result<Page> {
            self.pages.get(&id).cloned().ok_or_else(|| Error::corrupt(format!("page {id} read after free")))
        }
    }

    impl PagesMut for MemPages {
        fn alloc(&mut self, buf: Vec<u8>) -> Result<PageId> {
            let id = self.next;
            self.next += 1;
            self.pages.insert(id, Arc::from(buf));
            self.private.insert(id);
            Ok(id)
        }
        fn is_private(&self, id: PageId) -> bool {
            self.private.contains(&id)
        }
        fn replace(&mut self, id: PageId, buf: Vec<u8>) {
            assert!(self.private.contains(&id));
            self.pages.insert(id, Arc::from(buf));
        }
        fn free(&mut self, id: PageId) {
            assert!(self.pages.remove(&id).is_some(), "page {id} freed twice");
            self.private.remove(&id);
            self.frees.push(id);
        }
    }

    fn entries(p: &MemPages, root: PageId) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        let mut c = Cursor::new(p, root);
        c.first().unwrap();
        while c.valid() {
            out.push((c.key().unwrap().to_vec(), c.value().unwrap().to_vec()));
            c.next().unwrap();
        }
        out
    }

    fn check(p: &MemPages, root: PageId, model: &BTreeMap<Vec<u8>, Vec<u8>>) {
        let mut seen = HashSet::new();
        let mut walked = Vec::new();
        walk(
            p,
            root,
            &mut |id| {
                assert!(seen.insert(id), "page {id} reached twice");
                Ok(())
            },
            &mut |k, v| {
                walked.push((k.to_vec(), v.to_vec()));
                Ok(())
            },
        )
        .unwrap();
        let want: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(walked, want);
        assert_eq!(seen.len(), p.pages.len(), "every page belongs to the tree");
    }

    fn random_key(rng: &mut Rng, space: u64) -> Vec<u8> {
        let n = rng.below(space);
        let mut key = format!("key{n:08}").into_bytes();
        // A few long keys, to split branches unevenly.
        if n % 13 == 0 {
            key.resize(200 + (n % 700) as usize, b'k');
        }
        key
    }

    fn random_value(rng: &mut Rng) -> Vec<u8> {
        let len = match rng.below(10) {
            0 => 0,
            1 => 900 + rng.below(100) as usize,
            _ => rng.below(120) as usize,
        };
        (0..len).map(|_| rng.next() as u8).collect()
    }

    #[test]
    fn matches_a_sorted_map_through_random_changes() {
        for seed in 0..8 {
            let mut rng = Rng::new(seed);
            let mut p = MemPages::new();
            let mut root = NO_PAGE;
            let mut model = BTreeMap::new();
            let space = [50, 500, 5000][seed as usize % 3];
            for step in 0..6000 {
                let key = random_key(&mut rng, space);
                if rng.below(3) == 0 {
                    let (new_root, found) = delete(&mut p, root, &key).unwrap();
                    assert_eq!(found, model.remove(&key).is_some());
                    root = new_root;
                } else {
                    let value = random_value(&mut rng);
                    let (new_root, existed) = put(&mut p, root, &key, &value).unwrap();
                    assert_eq!(existed, model.insert(key.clone(), value).is_some());
                    root = new_root;
                }
                assert_eq!(get(&p, root, &key).unwrap(), model.get(&key).cloned());
                if rng.below(40) == 0 {
                    p.commit();
                }
                if step % 500 == 499 {
                    check(&p, root, &model);
                }
            }
            check(&p, root, &model);
            // Emptying the tree gives every page back.
            for key in model.keys().cloned().collect::<Vec<_>>() {
                root = delete(&mut p, root, &key).unwrap().0;
            }
            assert_eq!(root, NO_PAGE);
            assert!(p.pages.is_empty());
        }
    }

    #[test]
    fn an_old_root_still_reads_the_tree_as_it_was() {
        let mut rng = Rng::new(42);
        // Freed pages stay readable here, as they do for a pinned reader.
        struct Keep(MemPages, HashMap<PageId, Page>);
        impl Pages for Keep {
            fn page(&self, id: PageId) -> Result<Page> {
                self.0.page(id).or_else(|_| Ok(self.1[&id].clone()))
            }
        }
        impl PagesMut for Keep {
            fn alloc(&mut self, buf: Vec<u8>) -> Result<PageId> {
                self.0.alloc(buf)
            }
            fn is_private(&self, id: PageId) -> bool {
                self.0.is_private(id)
            }
            fn replace(&mut self, id: PageId, buf: Vec<u8>) {
                self.0.replace(id, buf)
            }
            fn free(&mut self, id: PageId) {
                let page = self.0.pages[&id].clone();
                self.1.insert(id, page);
                self.0.free(id);
            }
        }
        let mut p = Keep(MemPages::new(), HashMap::new());
        let mut root = NO_PAGE;
        let mut model = BTreeMap::new();
        let mut snapshots = Vec::new();
        for step in 0..3000 {
            let key = random_key(&mut rng, 800);
            if rng.below(4) == 0 {
                root = delete(&mut p, root, &key).unwrap().0;
                model.remove(&key);
            } else {
                let value = random_value(&mut rng);
                root = put(&mut p, root, &key, &value).unwrap().0;
                model.insert(key, value);
            }
            if step % 300 == 0 {
                p.0.commit();
                snapshots.push((root, model.clone()));
            }
        }
        for (old_root, old_model) in snapshots {
            let mut got = Vec::new();
            let mut c = Cursor::new(&p, old_root);
            c.first().unwrap();
            while c.valid() {
                got.push((c.key().unwrap().to_vec(), c.value().unwrap().to_vec()));
                c.next().unwrap();
            }
            let want: Vec<_> = old_model.into_iter().collect();
            assert_eq!(got, want);
        }
    }

    #[test]
    fn cursors_seek_and_step_both_ways() {
        let mut p = MemPages::new();
        let mut root = NO_PAGE;
        let mut model = BTreeMap::new();
        for n in (0..4000u32).step_by(2) {
            let key = format!("{n:06}").into_bytes();
            root = put(&mut p, root, &key, &n.to_le_bytes()).unwrap().0;
            model.insert(key, n.to_le_bytes().to_vec());
        }
        let all = entries(&p, root);
        assert_eq!(all.len(), 2000);

        let mut rng = Rng::new(3);
        for _ in 0..2000 {
            let probe = format!("{:06}", rng.below(4100)).into_bytes();
            let mut c = Cursor::new(&p, root);
            c.seek(&probe).unwrap();
            let want = model.range(probe.clone()..).next();
            assert_eq!(c.key(), want.map(|(k, _)| k.as_slice()));

            for inclusive in [true, false] {
                c.seek_back(&probe, inclusive).unwrap();
                let want = if inclusive { model.range(..=probe.clone()).next_back() } else { model.range(..probe.clone()).next_back() };
                assert_eq!(c.key(), want.map(|(k, _)| k.as_slice()), "seek_back {probe:?} {inclusive}");
            }
        }

        // All the way down, then all the way back up.
        let mut c = Cursor::new(&p, root);
        c.last().unwrap();
        let mut back = Vec::new();
        while c.valid() {
            back.push(c.key().unwrap().to_vec());
            c.prev().unwrap();
        }
        back.reverse();
        assert_eq!(back, model.keys().cloned().collect::<Vec<_>>());

        let mut empty = Cursor::new(&p, NO_PAGE);
        empty.seek(b"x").unwrap();
        assert!(!empty.valid());
        empty.seek_back(b"x", true).unwrap();
        assert!(!empty.valid());
    }

    #[test]
    fn rising_keys_fill_their_pages() {
        let mut p = MemPages::new();
        let mut root = NO_PAGE;
        let value = [7u8; 100];
        for n in 0..20_000u32 {
            root = put(&mut p, root, &n.to_be_bytes(), &value).unwrap().0;
        }
        let per_page = CAPACITY / (4 + 100 + 2 + 2);
        let leaves = 20_000 / per_page + 1;
        assert!(p.pages.len() < leaves + leaves / 10 + 4, "{} pages for {leaves} full leaves", p.pages.len());
    }

    #[test]
    fn refuses_keys_and_values_that_cannot_fit() {
        let mut p = MemPages::new();
        assert!(matches!(put(&mut p, NO_PAGE, &[0u8; MAX_KEY + 1], b""), Err(Error::KeyTooLong(_))));
        let key = [1u8; MAX_KEY];
        let (root, _) = put(&mut p, NO_PAGE, &key, &vec![2u8; max_value(MAX_KEY)]).unwrap();
        assert!(put(&mut p, root, b"k", &vec![0u8; max_value(1) + 1]).is_err());
        // The largest cells still split cleanly.
        let mut root = root;
        for n in 0..200u32 {
            let mut key = vec![3u8; MAX_KEY];
            key[..4].copy_from_slice(&n.to_be_bytes());
            root = put(&mut p, root, &key, &vec![4u8; max_value(MAX_KEY)]).unwrap().0;
        }
        assert_eq!(entries(&p, root).len(), 201);
    }
}
