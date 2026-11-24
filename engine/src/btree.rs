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
//! Most changes touch one cell of a page: they are made by moving the
//! bytes after it within the page (`Edit`), on the page itself if the
//! writer owns it and on a copy otherwise. Only a change that no longer
//! fits takes the page apart into its cells to split it.
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

    /// Shows a page's bytes to `read`, which keeps nothing of them. For a
    /// holder that has the page at hand this takes no share in it, and a
    /// page at the top of a tree is shared by every thread that reads:
    /// each share taken and given back is a write to memory they all
    /// watch. `read` must not ask this holder for pages.
    fn view(&self, id: PageId, read: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        read(&self.page(id)?)
    }
}

pub trait PagesMut: Pages {
    /// Stores a new page, visible to this writer only, and names it.
    fn alloc(&mut self, buf: Vec<u8>) -> Result<PageId>;
    /// Whether this writer made the page and may still replace it.
    fn is_private(&self, id: PageId) -> bool;
    fn replace(&mut self, id: PageId, buf: Vec<u8>);
    /// Says this writer's tree no longer uses the page.
    fn free(&mut self, id: PageId);
    /// Changes a page this writer made, where it lies. Returns false,
    /// having changed nothing, if that cannot be done now: the page is
    /// not in memory, or someone still reads the bytes as they are.
    fn edit(&mut self, _id: PageId, _change: &mut dyn FnMut(&mut [u8])) -> bool {
        false
    }
    /// `alloc` for a page that is already in the form pages are kept in.
    fn alloc_page(&mut self, page: Page) -> Result<PageId> {
        self.alloc(page.to_vec())
    }
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
        // The key's length is in the cell: where the cell ends is not
        // needed to find it. Lengths below 128, which nearly all are,
        // take one byte.
        let cell = &self.page[self.offset(i)..];
        let mut pos = if self.is_leaf() { 0 } else { 4 };
        let klen = match cell[pos] {
            short if short < 0x80 => {
                pos += 1;
                short as usize
            }
            _ => get_varint(cell, &mut pos).unwrap_or(0) as usize,
        };
        if self.is_leaf() {
            if cell[pos] < 0x80 {
                pos += 1;
            } else {
                get_varint(cell, &mut pos);
            }
        }
        &cell[pos..pos + klen]
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

/// A change to one cell of a page.
enum Edit<'c> {
    /// A new cell, to be the `usize`th.
    Insert(usize, &'c [u8]),
    Replace(usize, &'c [u8]),
    Remove(usize),
}

impl Node<'_> {
    /// Where cell `i` ends.
    fn cell_end(&self, i: usize) -> usize {
        if i + 1 < self.len() {
            self.offset(i + 1)
        } else {
            self.end()
        }
    }

    /// Whether the page still holds its cells after `edit`.
    fn fits(&self, edit: &Edit) -> bool {
        let used = self.end() - HEADER;
        match edit {
            Edit::Insert(_, cell) => used + cell.len() + 2 <= CAPACITY,
            Edit::Replace(i, cell) => {
                used + cell.len() - (self.cell_end(*i) - self.offset(*i)) <= CAPACITY
            }
            Edit::Remove(_) => true,
        }
    }
}

/// Makes `edit` in a page's bytes, which have room for it: the cells
/// after the one changed move up or down, and their offsets with them.
fn apply(buf: &mut [u8], edit: &Edit) {
    let n = get_u16(buf, 6) as usize;
    let end = get_u16(buf, 8) as usize;
    let offset = |buf: &[u8], i: usize| get_u16(buf, HEADER + 2 * i) as usize;
    let start = HEADER + 2 * n;
    match *edit {
        Edit::Insert(i, cell) => {
            let at = if i < n { offset(buf, i) } else { end };
            // The cells from here on make room for the new one and for
            // its offset; those before it only for the offset.
            buf.copy_within(at..end, at + 2 + cell.len());
            buf.copy_within(start..at, start + 2);
            buf[at + 2..at + 2 + cell.len()].copy_from_slice(cell);
            // From the last down: each takes the place after its own.
            for j in (i + 1..=n).rev() {
                let old = offset(buf, j - 1);
                put_u16(buf, HEADER + 2 * j, (old + 2 + cell.len()) as u16);
            }
            put_u16(buf, HEADER + 2 * i, (at + 2) as u16);
            for j in 0..i {
                let old = offset(buf, j);
                put_u16(buf, HEADER + 2 * j, (old + 2) as u16);
            }
            put_u16(buf, 6, (n + 1) as u16);
            put_u16(buf, 8, (end + 2 + cell.len()) as u16);
        }
        Edit::Replace(i, cell) => {
            let at = offset(buf, i);
            let old_end = if i + 1 < n { offset(buf, i + 1) } else { end };
            let new_end = at + cell.len();
            buf.copy_within(old_end..end, new_end);
            buf[at..new_end].copy_from_slice(cell);
            for j in i + 1..n {
                let old = offset(buf, j);
                put_u16(buf, HEADER + 2 * j, (old + new_end - old_end) as u16);
            }
            let page_end = end + new_end - old_end;
            if page_end < end {
                buf[page_end..end].fill(0);
            }
            put_u16(buf, 8, page_end as u16);
        }
        Edit::Remove(i) => {
            let at = offset(buf, i);
            let old_end = if i + 1 < n { offset(buf, i + 1) } else { end };
            let gone = old_end - at;
            // From the first up: each takes the place before its own.
            for j in 0..i {
                let old = offset(buf, j);
                put_u16(buf, HEADER + 2 * j, (old - 2) as u16);
            }
            for j in i..n - 1 {
                let old = offset(buf, j + 1);
                put_u16(buf, HEADER + 2 * j, (old - 2 - gone) as u16);
            }
            buf.copy_within(start..at, start - 2);
            buf.copy_within(old_end..end, at - 2);
            let page_end = end - 2 - gone;
            buf[page_end..end].fill(0);
            put_u16(buf, 6, (n - 1) as u16);
            put_u16(buf, 8, page_end as u16);
        }
    }
}

/// Makes a change that fits in page `id`, whose bytes are `page`: on the
/// page itself if the writer owns it, on a copy otherwise.
fn change<P: PagesMut>(p: &mut P, id: PageId, page: Page, edit: &Edit) -> Result<Outcome> {
    if p.is_private(id) {
        // Given up here, the bytes are the writer's alone to change,
        // unless a cursor somewhere still reads them.
        drop(page);
        if p.edit(id, &mut |buf| apply(buf, edit)) {
            return Ok(Outcome::Same);
        }
        let mut buf = p.page(id)?.to_vec();
        apply(&mut buf, edit);
        p.replace(id, buf);
        return Ok(Outcome::Same);
    }
    // One copy, made in the form the writer keeps, and changed there.
    let mut copy: Page = Page::from(&page[..]);
    drop(page);
    apply(
        std::sync::Arc::get_mut(&mut copy).expect("a page just made has one holder"),
        edit,
    );
    let new = p.alloc_page(copy)?;
    p.free(id);
    Ok(Outcome::Now(vec![(Vec::new(), new)]))
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
fn replace_child<P: PagesMut>(p: &mut P, id: PageId, page: Page, i: usize, pages: Vec<(Vec<u8>, PageId)>) -> Result<Outcome> {
    let node = Node::new(&page, id)?;
    match pages.as_slice() {
        // The child moved: its cell names the new page.
        [(_, child)] => {
            let cell = branch_cell(*child, node.key(i));
            return change(p, id, page, &Edit::Replace(i, &cell));
        }
        // The child is gone, and others remain.
        [] if node.len() > 1 => return change(p, id, page, &Edit::Remove(i)),
        _ => {}
    }
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
    let mut found = None;
    get_with(p, root, key, &mut |value| found = Some(value.to_vec()))?;
    Ok(found)
}

/// Shows `key`'s value to `on_value`, where it lies in its page, if the
/// tree has the key. Returns whether it has.
pub fn get_with<P: Pages + ?Sized>(p: &P, root: PageId, key: &[u8], on_value: &mut dyn FnMut(&[u8])) -> Result<bool> {
    if root == NO_PAGE {
        return Ok(false);
    }
    let mut id = root;
    loop {
        let mut child = None;
        let mut found = false;
        p.view(id, &mut |page| {
            let node = Node::new(page, id)?;
            if node.is_leaf() {
                let (i, hit) = node.search(key);
                if hit {
                    on_value(node.value(i));
                    found = true;
                }
            } else {
                child = Some(node.child(node.child_for(key)));
            }
            Ok(())
        })?;
        match child {
            Some(next) => id = next,
            None => return Ok(found),
        }
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
        let edit = if found {
            Edit::Replace(i, &cell)
        } else {
            Edit::Insert(i, &cell)
        };
        if node.fits(&edit) {
            return Ok((change(p, id, page, &edit)?, found));
        }
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
        Outcome::Now(pages) => Ok((replace_child(p, id, page, i, pages)?, existed)),
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
        if node.len() > 1 {
            return Ok((change(p, id, page, &Edit::Remove(i))?, true));
        }
        // Its last entry: the page goes.
        return Ok((write_node(p, id, KIND_LEAF, &[], false)?, true));
    }
    let i = node.child_for(key);
    let (outcome, found) = delete_rec(p, node.child(i), key)?;
    match outcome {
        Outcome::Same => Ok((Outcome::Same, found)),
        Outcome::Now(pages) => Ok((replace_child(p, id, page, i, pages)?, found)),
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

/// The numbers of every page of a tree, added to `out`, without reading
/// its leaves: a tree is as deep everywhere, so once the leftmost way
/// down has shown how deep, the pages a branch one above the leaves
/// names are known to be leaves. To free a tree that was read by
/// someone else, or that is not worth reading again.
pub fn page_ids<P: Pages + ?Sized>(p: &P, root: PageId, out: &mut Vec<PageId>) -> Result<()> {
    if root == NO_PAGE {
        return Ok(());
    }
    // How many levels of branches lie above the leaves.
    let mut branches = 0;
    let mut id = root;
    loop {
        let page = p.page(id)?;
        let node = Node::new(&page, id)?;
        if node.is_leaf() {
            break;
        }
        if node.len() == 0 {
            return Err(Error::corrupt(format!("tree page {id} is empty")));
        }
        branches += 1;
        id = node.child(0);
    }
    fn rec<P: Pages + ?Sized>(p: &P, id: PageId, branches: usize, out: &mut Vec<PageId>) -> Result<()> {
        out.push(id);
        if branches == 0 {
            return Ok(());
        }
        let page = p.page(id)?;
        let node = Node::new(&page, id)?;
        if node.is_leaf() {
            return Err(Error::corrupt(format!("page {id} is a leaf where the tree has a branch")));
        }
        for i in 0..node.len() {
            rec(p, node.child(i), branches - 1, out)?;
        }
        Ok(())
    }
    rec(p, root, branches, out)
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

// -------------------------------------------------------------- building

/// Builds a tree from entries given in rising key order, writing each
/// page once and leaving every page but the last of its level full. Far
/// cheaper than as many `put`s, each of which rewrites a page.
#[derive(Default)]
pub struct Builder {
    /// The page being filled at each level, leaves first.
    levels: Vec<Level>,
    last: Vec<u8>,
    /// Where a page is put together before it is stored.
    scratch: Vec<u8>,
}

/// A page being filled: its cells one after another, as they will lie.
#[derive(Default)]
struct Level {
    cells: Vec<u8>,
    /// Where each cell starts in `cells`.
    starts: Vec<u16>,
    /// The lowest key under the page.
    low: Vec<u8>,
}

impl Level {
    /// The room the page's cells and their offsets take.
    fn used(&self) -> usize {
        self.cells.len() + 2 * self.starts.len()
    }
}

impl Builder {
    pub fn new() -> Builder {
        Builder::default()
    }

    pub fn push<P: PagesMut>(&mut self, p: &mut P, key: &[u8], value: &[u8]) -> Result<()> {
        check_sizes(key, value)?;
        if !self.levels.is_empty() && key <= self.last.as_slice() {
            return Err(Error::corrupt("a tree was built from keys out of order"));
        }
        self.last.clear();
        self.last.extend_from_slice(key);
        let size = varint_len(key.len() as u64) + varint_len(value.len() as u64) + key.len() + value.len();
        let at = self.room(p, 0, size, key)?;
        put_varint(&mut at.cells, key.len() as u64);
        put_varint(&mut at.cells, value.len() as u64);
        at.cells.extend_from_slice(key);
        at.cells.extend_from_slice(value);
        Ok(())
    }

    /// Makes room for a cell of `size` bytes at a level, first writing
    /// out the level's page if the cell does not fit in it, and notes
    /// where the cell starts. `low` is the lowest key under the cell.
    fn room<P: PagesMut>(&mut self, p: &mut P, level: usize, size: usize, low: &[u8]) -> Result<&mut Level> {
        if self.levels.len() == level {
            self.levels.push(Level::default());
        }
        if self.levels[level].used() + size + 2 > CAPACITY {
            self.write(p, level)?;
        }
        let at = &mut self.levels[level];
        if at.starts.is_empty() {
            at.low.clear();
            at.low.extend_from_slice(low);
        }
        at.starts.push(at.cells.len() as u16);
        Ok(at)
    }

    /// Writes the page a level is filling and tells the level above.
    fn write<P: PagesMut>(&mut self, p: &mut P, level: usize) -> Result<PageId> {
        let at = &mut self.levels[level];
        let first = HEADER + 2 * at.starts.len();
        let page = &mut self.scratch;
        page.clear();
        page.resize(HEADER, 0);
        page[4] = if level == 0 { KIND_LEAF } else { KIND_BRANCH };
        put_u16(page, 6, at.starts.len() as u16);
        put_u16(page, 8, (first + at.cells.len()) as u16);
        for start in &at.starts {
            page.extend_from_slice(&((first + usize::from(*start)) as u16).to_le_bytes());
        }
        page.extend_from_slice(&at.cells);
        page.resize(PAGE_SIZE, 0);
        let id = p.alloc_page(Page::from(&page[..]))?;
        let low = std::mem::take(&mut at.low);
        at.cells.clear();
        at.starts.clear();
        let up = self.room(p, level + 1, 4 + varint_len(low.len() as u64) + low.len(), &low)?;
        // The first key of a branch is never read: it is stored empty.
        let key: &[u8] = if up.starts.len() == 1 { &[] } else { &low };
        up.cells.extend_from_slice(&id.to_le_bytes());
        put_varint(&mut up.cells, key.len() as u64);
        up.cells.extend_from_slice(key);
        // Kept, to hold the next page's lowest key without allocating.
        self.levels[level].low = low;
        Ok(id)
    }

    /// The tree's root: `NO_PAGE` if nothing was pushed.
    pub fn finish<P: PagesMut>(mut self, p: &mut P) -> Result<PageId> {
        if self.levels.is_empty() {
            return Ok(NO_PAGE);
        }
        let mut level = 0;
        loop {
            // A page that is all there is of the top level is the root.
            let id = self.write(p, level)?;
            if level + 2 == self.levels.len() && self.levels[level + 1].starts.len() == 1 {
                return Ok(id);
            }
            level += 1;
        }
    }
}

// --------------------------------------------------------------- cursor

/// A position in a tree that can step to the next or previous entry. It
/// reads the tree as of the root it was given, whatever is written after.
///
/// It holds no borrow of the pages: each move is handed them. So a tree
/// can be read through one of these while another tree is written.
pub struct Scan {
    root: PageId,
    /// The pages from the root down to a leaf, each with the index of the
    /// cell the scan is at or went down through. Empty when the scan is
    /// off either end.
    path: Vec<(Page, usize)>,
}

impl Scan {
    pub fn new(root: PageId) -> Scan {
        Scan { root, path: Vec::new() }
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

    fn descend<P: Pages + ?Sized>(&mut self, pages: &P, mut id: PageId, pick: &dyn Fn(Node) -> usize) -> Result<()> {
        loop {
            let page = pages.page(id)?;
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

    pub fn first<P: Pages + ?Sized>(&mut self, pages: &P) -> Result<()> {
        self.path.clear();
        if self.root != NO_PAGE {
            self.descend(pages, self.root, &|_| 0)?;
        }
        Ok(())
    }

    pub fn last<P: Pages + ?Sized>(&mut self, pages: &P) -> Result<()> {
        self.path.clear();
        if self.root != NO_PAGE {
            self.descend(pages, self.root, &|node| node.len() - 1)?;
        }
        Ok(())
    }

    /// Moves to the first entry whose key is `key` or greater.
    pub fn seek<P: Pages + ?Sized>(&mut self, pages: &P, key: &[u8]) -> Result<()> {
        self.path.clear();
        if self.root == NO_PAGE {
            return Ok(());
        }
        self.descend(pages, self.root, &|node| if node.is_leaf() { node.search(key).0 } else { node.child_for(key) })?;
        let (node, i) = self.leaf().unwrap();
        if i == node.len() {
            // Past this leaf's last entry: the next leaf's first, if any.
            self.path.last_mut().unwrap().1 = i - 1;
            self.next(pages)?;
        }
        Ok(())
    }

    /// Moves to the last entry whose key is below `key`, or is `key` itself
    /// when `inclusive`.
    pub fn seek_back<P: Pages + ?Sized>(&mut self, pages: &P, key: &[u8], inclusive: bool) -> Result<()> {
        self.seek(pages, key)?;
        if !self.valid() {
            return self.last(pages);
        }
        if !(inclusive && self.key() == Some(key)) {
            self.prev(pages)?;
        }
        Ok(())
    }

    pub fn next<P: Pages + ?Sized>(&mut self, pages: &P) -> Result<()> {
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
                return self.descend(pages, child, &|_| 0);
            }
            self.path.pop();
        }
        Ok(())
    }

    pub fn prev<P: Pages + ?Sized>(&mut self, pages: &P) -> Result<()> {
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
                return self.descend(pages, child, &|node| node.len() - 1);
            }
            self.path.pop();
        }
        Ok(())
    }
}

/// A [`Scan`] together with the pages it reads.
pub struct Cursor<'p, P: Pages + ?Sized> {
    pages: &'p P,
    scan: Scan,
}

impl<'p, P: Pages + ?Sized> Cursor<'p, P> {
    pub fn new(pages: &'p P, root: PageId) -> Self {
        Cursor { pages, scan: Scan::new(root) }
    }

    pub fn valid(&self) -> bool {
        self.scan.valid()
    }

    pub fn key(&self) -> Option<&[u8]> {
        self.scan.key()
    }

    pub fn value(&self) -> Option<&[u8]> {
        self.scan.value()
    }

    pub fn first(&mut self) -> Result<()> {
        self.scan.first(self.pages)
    }

    pub fn last(&mut self) -> Result<()> {
        self.scan.last(self.pages)
    }

    /// Moves to the first entry whose key is `key` or greater.
    pub fn seek(&mut self, key: &[u8]) -> Result<()> {
        self.scan.seek(self.pages, key)
    }

    /// Moves to the last entry whose key is below `key`, or is `key` itself
    /// when `inclusive`.
    pub fn seek_back(&mut self, key: &[u8], inclusive: bool) -> Result<()> {
        self.scan.seek_back(self.pages, key, inclusive)
    }

    pub fn next(&mut self) -> Result<()> {
        self.scan.next(self.pages)
    }

    pub fn prev(&mut self) -> Result<()> {
        self.scan.prev(self.pages)
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
        fn edit(&mut self, id: PageId, change: &mut dyn FnMut(&mut [u8])) -> bool {
            assert!(self.private.contains(&id));
            match self.pages.get_mut(&id).and_then(Arc::get_mut) {
                Some(buf) => {
                    change(buf);
                    true
                }
                None => false,
            }
        }
    }

    /// A cell changed where it lies leaves the page byte for byte as
    /// building it from its cells would.
    #[test]
    fn a_page_edited_in_place_equals_the_page_rebuilt() {
        let mut rng = Rng::new(7);
        for round in 0..300 {
            let kind = if round % 2 == 0 {
                KIND_LEAF
            } else {
                KIND_BRANCH
            };
            let mut cells: Vec<Vec<u8>> = Vec::new();
            let mut page = build(kind, &[]);
            for _ in 0..400 {
                let cell: Vec<u8> = (0..rng.below(if round % 3 == 0 { 600 } else { 40 }) + 1)
                    .map(|_| rng.next() as u8)
                    .collect();
                let pick = rng.below(10);
                let edit = if cells.is_empty() || pick < 5 {
                    Edit::Insert(rng.below(cells.len() as u64 + 1) as usize, &cell)
                } else if pick < 8 {
                    Edit::Replace(rng.below(cells.len() as u64) as usize, &cell)
                } else {
                    Edit::Remove(rng.below(cells.len() as u64) as usize)
                };
                let node = Node { page: &page };
                let fits = node.fits(&edit);
                let mut after = cells.clone();
                match &edit {
                    Edit::Insert(i, cell) => after.insert(*i, cell.to_vec()),
                    Edit::Replace(i, cell) => after[*i] = cell.to_vec(),
                    Edit::Remove(i) => {
                        after.remove(*i);
                    }
                }
                let slices: Vec<&[u8]> = after.iter().map(|c| c.as_slice()).collect();
                assert_eq!(
                    fits,
                    size(&slices) <= CAPACITY,
                    "whether an edit fits is whether its cells do"
                );
                if !fits {
                    continue;
                }
                apply(&mut page, &edit);
                assert!(
                    page == build(kind, &slices),
                    "round {round}: an edited page differs from the same cells built"
                );
                cells = after;
            }
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
    fn a_built_tree_reads_like_one_made_of_puts() {
        let mut rng = Rng::new(3);
        for count in [0usize, 1, 2, 7, 300, 5_000, 40_000] {
            let mut p = MemPages::new();
            let mut model = BTreeMap::new();
            while model.len() < count {
                // Keys and values of every size, some as large as fit.
                let mut key = random_key(&mut rng, 1 << 40);
                if rng.chance(50) {
                    key.resize(MAX_KEY - rng.below(40) as usize, 7);
                }
                let room = max_value(key.len());
                let value = if rng.chance(20) { vec![1u8; room] } else { vec![2u8; rng.below(room.min(60) as u64 + 1) as usize] };
                model.insert(key, value);
            }
            let mut builder = Builder::new();
            for (key, value) in &model {
                builder.push(&mut p, key, value).unwrap();
            }
            let root = builder.finish(&mut p).unwrap();
            assert_eq!(root == NO_PAGE, count == 0);
            check(&p, root, &model);
            // It is a tree like any other from here on.
            let mut root = root;
            for (key, _) in model.iter().step_by(3) {
                root = delete(&mut p, root, key).unwrap().0;
            }
            let rest: BTreeMap<Vec<u8>, Vec<u8>> = model.iter().enumerate().filter(|(n, _)| n % 3 != 0).map(|(_, (k, v))| (k.clone(), v.clone())).collect();
            check(&p, root, &rest);
            destroy(&mut p, root, &mut |_, _| Ok(())).unwrap();
            assert!(p.pages.is_empty());
        }
        let mut p = MemPages::new();
        let mut builder = Builder::new();
        builder.push(&mut p, b"b", b"").unwrap();
        assert!(builder.push(&mut p, b"a", b"").is_err(), "keys must rise");
    }

    #[test]
    fn the_pages_of_a_built_tree_are_found_without_reading_its_leaves() {
        for count in [0u32, 1, 5, 400, 40_000] {
            let mut p = MemPages::new();
            let mut builder = Builder::new();
            for n in 0..count {
                builder.push(&mut p, &n.to_be_bytes(), &[7; 40]).unwrap();
            }
            let root = builder.finish(&mut p).unwrap();
            let mut found = Vec::new();
            page_ids(&p, root, &mut found).unwrap();
            let mut walked = Vec::new();
            walk(
                &p,
                root,
                &mut |id| {
                    walked.push(id);
                    Ok(())
                },
                &mut |_, _| Ok(()),
            )
            .unwrap();
            found.sort();
            walked.sort();
            assert_eq!(found, walked, "{count} entries");
            assert_eq!(found.len(), p.pages.len());
        }
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
