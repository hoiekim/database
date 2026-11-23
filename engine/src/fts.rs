//! Full-text indexes.
//!
//! An inverted index: for each word, the rows that hold it. Rows are named
//! in it by small numbers given in order of indexing, so that a word's
//! rows are a rising list that packs into two or three bytes each.
//!
//! Adding a row to the list of every word in it would be hundreds of
//! writes all over a tree, and the lists of common words would be
//! rewritten for ever. So the index is built the way a log-structured
//! store is, out of parts that are written once, front to back:
//!
//! - A new or changed row is **pending**: its words are stored beside the
//!   index, in one place, and a search reads the pending rows as they are.
//! - When enough are pending they become a **segment**: a small inverted
//!   index of their own, in a tree of its own, written in key order.
//! - When four segments of about one size have gathered they are merged
//!   into one, which takes a single pass over each.
//!
//! A search looks a word up in each segment, of which there are a few per
//! size, and so a number that grows with the logarithm of the index.
//!
//! A row that is deleted or changed is only forgotten: its number no
//! longer leads to a row, and the lists that hold it are cleaned when
//! their segment is next merged, or at once when half a segment is dead.
//!
//! The index's own tree holds, by the first byte of the key:
//!
//! ```text
//! 00                           the state: the next number, and the
//!                              trees of the pending rows and of each
//!                              segment
//! 01, primary key              the row's number
//! 02, number (8 bytes)         the row's primary key
//! ```
//!
//! the tree of pending rows holds each one's leading keys and words, in
//! as many parts as they need:
//!
//! ```text
//! primary key, part (2 bytes)
//! ```
//!
//! and a segment's tree holds runs of a word's rows:
//!
//! ```text
//! leading keys, word, first number     for each row: its number as the
//!                                      difference from the one before,
//!                                      then its score
//! ```
//!
//! A score is how much a row matches a word: the word's count in each
//! field times the field's weight, summed, in sixteenths.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use database_protocol::error::{DbError, ErrorCode};
use database_protocol::schema::IndexKind;
use database_protocol::value::Value;

use crate::btree::{self, Cursor, Pages, PagesMut, Scan};
use crate::catalog::Table;
use crate::error::{Error, Result};
use crate::eval::{eval, holds, Ctx, Frame, Scope};
use crate::exec::{encode_prefix, index_parts};
use crate::keys;
use crate::page::{get_varint, put_varint, PageId, NO_PAGE};
use crate::row::Row;
use crate::text;

const STATE: u8 = 0x00;
const NUMBER_OF: u8 = 0x01;
const KEY_OF: u8 = 0x02;

/// Pending rows become a segment once their words take this much room. A
/// search reads all of them, so this bounds what they add to a search.
static PENDING_BYTES: AtomicU64 = AtomicU64::new(256 * 1024);

/// Changes how much room pending rows may take before they become a
/// segment. Tests lower it to make segments of a handful of rows.
pub fn set_pending_bytes(bytes: u64) {
    PENDING_BYTES.store(bytes, Ordering::Relaxed);
}
/// Segments are merged when this many of about one size have gathered.
/// More would make writing cheaper in principle, as each entry is
/// rewritten once per step of size, and searching dearer, as there are
/// more segments to look in. Eight was tried: the bulk load in `bench/`
/// was no faster for it, and searches were a little slower.
const FANOUT: usize = 4;
/// Sizes are told apart in steps of four times, from this many entries.
const LEVEL_BASE: u64 = 8192;
/// However sizes fall, there are never more segments than this.
const MAX_SEGMENTS: usize = 40;
/// A run of a word's rows is cut when it reaches this many bytes.
const RUN_BYTES: usize = 1024;
/// Words longer than this are not indexed: they are not words.
pub const MAX_TERM: usize = 64;
/// The longest leading keys an indexed row may have.
const MAX_LEAD: usize = 1024;
/// Scores are kept in sixteenths.
const SCORE_UNIT: f64 = 16.0;

fn malformed() -> Error {
    Error::corrupt("a full-text index is malformed")
}

fn tagged(tag: u8, rest: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(rest.len() + 1);
    key.push(tag);
    key.extend_from_slice(rest);
    key
}

fn varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    get_varint(buf, pos).ok_or_else(malformed)
}

// ----------------------------------------------------------------- state

/// One segment: a tree of runs, for the rows numbered `low..=high`.
#[derive(Clone, Debug, PartialEq)]
struct Segment {
    root: PageId,
    low: u64,
    high: u64,
    /// Entries in its runs, dead rows' included.
    entries: u64,
    /// Rows it was written with.
    rows: u64,
    /// How many of those have since been deleted or changed.
    dead: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct State {
    next: u64,
    /// The tree of pending rows.
    pending: PageId,
    pending_rows: u64,
    pending_bytes: u64,
    /// Oldest first, so their number ranges rise.
    segments: Vec<Segment>,
}

fn load<P: Pages + ?Sized>(p: &P, root: PageId) -> Result<State> {
    let Some(value) = btree::get(p, root, &[STATE])? else {
        return Ok(State { next: 1, pending: NO_PAGE, pending_rows: 0, pending_bytes: 0, segments: Vec::new() });
    };
    let mut pos = 0;
    let next = varint(&value, &mut pos)?;
    let pending = PageId::try_from(varint(&value, &mut pos)?).map_err(|_| malformed())?;
    let pending_rows = varint(&value, &mut pos)?;
    let pending_bytes = varint(&value, &mut pos)?;
    let count = varint(&value, &mut pos)?;
    let mut segments = Vec::new();
    for _ in 0..count {
        let root = PageId::try_from(varint(&value, &mut pos)?).map_err(|_| malformed())?;
        let low = varint(&value, &mut pos)?;
        let high = low + varint(&value, &mut pos)?;
        let entries = varint(&value, &mut pos)?;
        let rows = varint(&value, &mut pos)?;
        let dead = varint(&value, &mut pos)?;
        segments.push(Segment { root, low, high, entries, rows, dead });
    }
    Ok(State { next, pending, pending_rows, pending_bytes, segments })
}

fn store<P: PagesMut>(p: &mut P, root: PageId, state: &State) -> Result<PageId> {
    let mut value = Vec::new();
    put_varint(&mut value, state.next);
    put_varint(&mut value, u64::from(state.pending));
    put_varint(&mut value, state.pending_rows);
    put_varint(&mut value, state.pending_bytes);
    put_varint(&mut value, state.segments.len() as u64);
    for s in &state.segments {
        put_varint(&mut value, u64::from(s.root));
        put_varint(&mut value, s.low);
        put_varint(&mut value, s.high - s.low);
        put_varint(&mut value, s.entries);
        put_varint(&mut value, s.rows);
        put_varint(&mut value, s.dead);
    }
    Ok(btree::put(p, root, &[STATE], &value)?.0)
}

// ----------------------------------------------------------------- words

/// What a row gives the index: the bytes of its leading keys, and each of
/// its words with a score, in order of the words.
#[derive(Clone, Debug, PartialEq)]
pub struct Words {
    pub lead: Vec<u8>,
    pub terms: Vec<(String, u32)>,
}

/// A row's words, or `None` if the index leaves the row out: its filter
/// does, or the row has no words.
pub fn words_of(ctx: &Ctx, table: &Table, index: usize, row: &Row) -> Result<Option<Words>> {
    let def = &table.indexes[index].def;
    let IndexKind::FullText { config, fields } = &def.kind else { return Err(malformed()) };
    let frame = [Frame { alias: &table.name, table, row: Some(row) }];
    let scope = Scope { frames: &frame, ..Scope::default() };
    if let Some(filter) = &def.filter {
        if !holds(ctx, &scope, filter)? {
            return Ok(None);
        }
    }
    let mut weights: HashMap<String, f64> = HashMap::new();
    for field in fields {
        match eval(ctx, &scope, &field.expr)? {
            Value::Null => {}
            Value::Text(text) => {
                for (term, count) in text::terms(&text, *config) {
                    if term.len() <= MAX_TERM {
                        *weights.entry(term).or_insert(0.0) += field.weight * f64::from(count);
                    }
                }
            }
            other => return Err(Error::Db(DbError::type_mismatch(format!("index {:?} expects text, got {}", def.name, other.type_name())))),
        }
    }
    if weights.is_empty() {
        return Ok(None);
    }
    let lead = def.keys.iter().map(|key| eval(ctx, &scope, &key.expr)).collect::<Result<Vec<_>>>()?;
    let lead = encode_prefix(lead, &index_parts(table, index))?;
    if lead.len() > MAX_LEAD {
        return Err(Error::db(ErrorCode::KeyTooLong, format!("the leading keys of index {:?} take {} bytes, and at most {MAX_LEAD} fit", def.name, lead.len())));
    }
    let mut terms: Vec<(String, u32)> = weights.into_iter().map(|(term, w)| (term, (w * SCORE_UNIT).round().clamp(1.0, 65535.0) as u32)).collect();
    terms.sort();
    Ok(Some(Words { lead, terms }))
}

fn encode_words(words: &Words) -> Vec<u8> {
    let mut out = Vec::with_capacity(words.lead.len() + words.terms.len() * 10 + 8);
    put_varint(&mut out, words.lead.len() as u64);
    out.extend_from_slice(&words.lead);
    put_varint(&mut out, words.terms.len() as u64);
    for (term, score) in &words.terms {
        out.push(term.len() as u8);
        out.extend_from_slice(term.as_bytes());
        put_varint(&mut out, u64::from(*score));
    }
    out
}

/// The leading keys of stored words, and where the terms start.
fn stored_lead(buf: &[u8]) -> Result<(&[u8], usize)> {
    let mut pos = 0;
    let len = varint(buf, &mut pos)? as usize;
    let lead = buf.get(pos..pos + len).ok_or_else(malformed)?;
    Ok((lead, pos + len))
}

/// Calls `f` with each stored term and its score.
fn stored_terms(buf: &[u8], mut pos: usize, f: &mut dyn FnMut(&[u8], u32)) -> Result<()> {
    let count = varint(buf, &mut pos)?;
    for _ in 0..count {
        let len = usize::from(*buf.get(pos).ok_or_else(malformed)?);
        let term = buf.get(pos + 1..pos + 1 + len).ok_or_else(malformed)?;
        pos += 1 + len;
        let score = varint(buf, &mut pos)? as u32;
        f(term, score);
    }
    Ok(())
}

fn decode_words(buf: &[u8]) -> Result<Words> {
    let (lead, pos) = stored_lead(buf)?;
    let mut terms = Vec::new();
    let mut bad = false;
    stored_terms(buf, pos, &mut |term, score| match std::str::from_utf8(term) {
        Ok(term) => terms.push((term.to_string(), score)),
        Err(_) => bad = true,
    })?;
    if bad {
        return Err(malformed());
    }
    Ok(Words { lead: lead.to_vec(), terms })
}

fn word_key(lead: &[u8], term: &str) -> Vec<u8> {
    let mut key = lead.to_vec();
    keys::encode(&mut key, &Value::Text(term.to_string()), false);
    key
}

// ------------------------------------------------------------------ runs

fn decode_run(first: u64, value: &[u8], out: &mut Vec<(u64, u32)>) -> Result<()> {
    let mut pos = 0;
    let mut at = first;
    while pos < value.len() {
        at += varint(value, &mut pos)?;
        let score = varint(value, &mut pos)? as u32;
        out.push((at, score));
    }
    Ok(())
}

/// The word a run's key starts with, and the number it ends with.
fn split_run_key(key: &[u8]) -> Result<(&[u8], u64)> {
    let at = key.len().checked_sub(8).ok_or_else(malformed)?;
    Ok((&key[..at], u64::from_be_bytes(key[at..].try_into().unwrap())))
}

/// Writes one word's rows, given in rising order, as runs.
struct RunWriter<'w> {
    word: &'w [u8],
    limit: usize,
    first: u64,
    last: u64,
    buf: Vec<u8>,
}

impl<'w> RunWriter<'w> {
    fn new(word: &'w [u8]) -> Self {
        let limit = RUN_BYTES.min(btree::max_value(word.len() + 8).saturating_sub(16));
        RunWriter { word, limit, first: 0, last: 0, buf: Vec::with_capacity(RUN_BYTES + 16) }
    }

    fn push<P: PagesMut>(&mut self, p: &mut P, tree: &mut btree::Builder, number: u64, score: u32) -> Result<()> {
        if self.buf.is_empty() {
            self.first = number;
            self.last = number;
        }
        put_varint(&mut self.buf, number - self.last);
        put_varint(&mut self.buf, u64::from(score));
        self.last = number;
        if self.buf.len() >= self.limit {
            self.finish(p, tree)?;
        }
        Ok(())
    }

    fn finish<P: PagesMut>(&mut self, p: &mut P, tree: &mut btree::Builder) -> Result<()> {
        if !self.buf.is_empty() {
            let mut key = Vec::with_capacity(self.word.len() + 8);
            key.extend_from_slice(self.word);
            key.extend_from_slice(&self.first.to_be_bytes());
            tree.push(p, &key, &self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }
}

// --------------------------------------------------------------- pending

fn pending_key(pk: &[u8], part: u16) -> Vec<u8> {
    let mut key = Vec::with_capacity(pk.len() + 2);
    key.extend_from_slice(pk);
    key.extend_from_slice(&part.to_be_bytes());
    key
}

/// The keys of a pending row's parts in the tree of pending rows, and
/// the bytes they hold in all.
fn pending_parts<P: Pages + ?Sized>(p: &P, root: PageId, pk: &[u8]) -> Result<(Vec<Vec<u8>>, u64)> {
    let start = pending_key(pk, 0);
    let mut cursor = Cursor::new(p, root);
    cursor.seek(&start)?;
    let (mut keys, mut bytes) = (Vec::new(), 0);
    while let (Some(key), Some(value)) = (cursor.key(), cursor.value()) {
        if key.len() != start.len() || key[..key.len() - 2] != start[..start.len() - 2] {
            break;
        }
        bytes += (key.len() + value.len()) as u64;
        keys.push(key.to_vec());
        cursor.next()?;
    }
    Ok((keys, bytes))
}

/// Calls `f` with each pending row's primary key and stored words, given
/// the tree of pending rows.
fn for_each_pending<P: Pages + ?Sized>(p: &P, root: PageId, f: &mut dyn FnMut(&[u8], &[u8]) -> Result<()>) -> Result<()> {
    let mut cursor = Cursor::new(p, root);
    cursor.first()?;
    let mut pk: Vec<u8> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut open = false;
    while let (Some(key), Some(value)) = (cursor.key(), cursor.value()) {
        let at = key.len().checked_sub(2).ok_or_else(malformed)?;
        if open && key[..at] != pk[..] {
            f(&pk, &buf)?;
            open = false;
        }
        if !open {
            pk.clear();
            pk.extend_from_slice(&key[..at]);
            buf.clear();
            open = true;
        }
        buf.extend_from_slice(value);
        cursor.next()?;
    }
    if open {
        f(&pk, &buf)?;
    }
    Ok(())
}

// --------------------------------------------------------------- changes

/// Adds a row that is not in the index.
pub fn add<P: PagesMut>(p: &mut P, mut root: PageId, pk: &[u8], words: &Words) -> Result<PageId> {
    if pk.len() + 2 > btree::MAX_KEY {
        return Err(Error::db(ErrorCode::KeyTooLong, format!("a primary key of {} bytes is too long for a full-text index", pk.len())));
    }
    let mut state = load(p, root)?;
    let stored = encode_words(words);
    let room = btree::max_value(pk.len() + 2);
    for (part, bytes) in stored.chunks(room).enumerate() {
        let part = u16::try_from(part).map_err(|_| Error::db(ErrorCode::OutOfRange, "a row has too many words for a full-text index"))?;
        let key = pending_key(pk, part);
        state.pending_bytes += (key.len() + bytes.len()) as u64;
        state.pending = btree::put(p, state.pending, &key, bytes)?.0;
    }
    state.pending_rows += 1;
    if state.pending_bytes >= PENDING_BYTES.load(Ordering::Relaxed) {
        root = flush(p, root, &mut state)?;
        maintain(p, root, &mut state)?;
    }
    store(p, root, &state)
}

/// Takes a row out of the index, if it is in it.
pub fn remove<P: PagesMut>(p: &mut P, mut root: PageId, pk: &[u8]) -> Result<PageId> {
    if root == NO_PAGE {
        return Ok(root);
    }
    let mut state = load(p, root)?;
    let (parts, bytes) = pending_parts(p, state.pending, pk)?;
    if !parts.is_empty() {
        for key in &parts {
            state.pending = btree::delete(p, state.pending, key)?.0;
        }
        state.pending_rows = state.pending_rows.saturating_sub(1);
        state.pending_bytes = state.pending_bytes.saturating_sub(bytes);
        return store(p, root, &state);
    }
    let Some(number) = btree::get(p, root, &tagged(NUMBER_OF, pk))? else { return Ok(root) };
    let number = varint(&number, &mut 0)?;
    root = btree::delete(p, root, &tagged(NUMBER_OF, pk))?.0;
    root = btree::delete(p, root, &tagged(KEY_OF, &number.to_be_bytes()))?.0;
    // Its entries stay in their segment until that is next rewritten.
    if let Some(segment) = state.segments.iter_mut().find(|s| s.low <= number && number <= s.high) {
        segment.dead += 1;
        if segment.dead * 2 > segment.rows {
            maintain(p, root, &mut state)?;
        }
    }
    store(p, root, &state)
}

/// Frees the index's trees.
pub fn destroy<P: PagesMut>(p: &mut P, root: PageId) -> Result<()> {
    if root == NO_PAGE {
        return Ok(());
    }
    let state = load(p, root)?;
    for segment in state.segments {
        btree::destroy(p, segment.root, &mut |_, _| Ok(()))?;
    }
    btree::destroy(p, state.pending, &mut |_, _| Ok(()))?;
    btree::destroy(p, root, &mut |_, _| Ok(()))
}

/// Makes a segment of the pending rows.
fn flush<P: PagesMut>(p: &mut P, mut root: PageId, state: &mut State) -> Result<PageId> {
    let mut rows: Vec<(Vec<u8>, Words)> = Vec::new();
    for_each_pending(p, state.pending, &mut |pk, stored| {
        rows.push((pk.to_vec(), decode_words(stored)?));
        Ok(())
    })?;
    // Every word's rows, gathered by word and then put in key order, so
    // that the segment's tree is written front to back.
    let low = state.next;
    let mut by_word: HashMap<(&[u8], &str), Vec<(u64, u32)>> = HashMap::new();
    for (n, (_, words)) in rows.iter().enumerate() {
        for (term, score) in &words.terms {
            by_word.entry((&words.lead, term)).or_default().push((low + n as u64, *score));
        }
    }
    let mut lists: Vec<(Vec<u8>, Vec<(u64, u32)>)> = by_word.into_iter().map(|((lead, term), list)| (word_key(lead, term), list)).collect();
    lists.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    btree::destroy(p, state.pending, &mut |_, _| Ok(()))?;
    state.pending = NO_PAGE;
    for (n, (pk, _)) in rows.iter().enumerate() {
        let number = low + n as u64;
        let mut value = Vec::new();
        put_varint(&mut value, number);
        root = btree::put(p, root, &tagged(NUMBER_OF, pk), &value)?.0;
        root = btree::put(p, root, &tagged(KEY_OF, &number.to_be_bytes()), pk)?.0;
    }
    let mut tree = btree::Builder::new();
    let mut entries = 0u64;
    for (word, list) in &lists {
        let mut writer = RunWriter::new(word);
        for (number, score) in list {
            writer.push(p, &mut tree, *number, *score)?;
        }
        writer.finish(p, &mut tree)?;
        entries += list.len() as u64;
    }
    let tree = tree.finish(p)?;
    if !rows.is_empty() {
        state.next = low + rows.len() as u64;
        state.segments.push(Segment { root: tree, low, high: state.next - 1, entries, rows: rows.len() as u64, dead: 0 });
    }
    state.pending_rows = 0;
    state.pending_bytes = 0;
    Ok(root)
}

/// Which step of size a segment is on: each is four times the last.
fn level(segment: &Segment) -> u32 {
    (segment.entries / LEVEL_BASE).max(1).ilog2() / 2
}

/// Rewrites segments that are mostly dead, and merges the newest ones
/// while enough of them are of a size.
fn maintain<P: PagesMut>(p: &mut P, root: PageId, state: &mut State) -> Result<()> {
    loop {
        if let Some(i) = state.segments.iter().position(|s| s.dead * 2 > s.rows) {
            merge(p, root, state, i, i + 1)?;
            continue;
        }
        let count = state.segments.len();
        let Some(newest) = state.segments.last() else { break };
        let at = level(newest);
        let alike = state.segments.iter().rev().take_while(|s| level(s) <= at).count();
        if alike >= FANOUT {
            merge(p, root, state, count - alike, count)?;
        } else if count > MAX_SEGMENTS {
            merge(p, root, state, 0, count)?;
        } else {
            break;
        }
    }
    Ok(())
}

/// Merges the segments `from..to`, which are neighbours in age, into one
/// without their dead rows.
fn merge<P: PagesMut>(p: &mut P, root: PageId, state: &mut State, from: usize, to: usize) -> Result<()> {
    let (low, high) = (state.segments[from].low, state.segments[to - 1].high);
    // Which numbers in their range still lead to a row.
    let mut live = vec![0u64; ((high - low) / 64 + 1) as usize];
    let mut rows = 0u64;
    {
        let mut cursor = Cursor::new(p, root);
        cursor.seek(&tagged(KEY_OF, &low.to_be_bytes()))?;
        while let Some(key) = cursor.key() {
            if key[0] != KEY_OF || key.len() != 9 {
                break;
            }
            let number = u64::from_be_bytes(key[1..].try_into().unwrap());
            if number > high {
                break;
            }
            live[((number - low) / 64) as usize] |= 1 << ((number - low) % 64);
            rows += 1;
            cursor.next()?;
        }
    }
    let mut scans: Vec<Scan> = state.segments[from..to].iter().map(|s| Scan::new(s.root)).collect();
    for scan in &mut scans {
        scan.first(p)?;
    }
    let mut tree = btree::Builder::new();
    let mut entries = 0u64;
    let mut word: Vec<u8> = Vec::new();
    let mut run: Vec<(u64, u32)> = Vec::new();
    loop {
        // The least word any segment has left.
        let mut any = false;
        for scan in &scans {
            if let Some(key) = scan.key() {
                let (w, _) = split_run_key(key)?;
                if !any || w < word.as_slice() {
                    word.clear();
                    word.extend_from_slice(w);
                    any = true;
                }
            }
        }
        if !any {
            break;
        }
        // Its rows from each segment in turn: older segments hold lower
        // numbers, so the whole list rises.
        let mut writer = RunWriter::new(&word);
        for scan in &mut scans {
            loop {
                run.clear();
                match (scan.key(), scan.value()) {
                    (Some(key), Some(value)) if key.len() == word.len() + 8 && key.starts_with(&word) => decode_run(split_run_key(key)?.1, value, &mut run)?,
                    _ => break,
                }
                for (number, score) in &run {
                    let n = number.checked_sub(low).filter(|_| *number <= high).ok_or_else(malformed)?;
                    if live[(n / 64) as usize] & (1 << (n % 64)) != 0 {
                        writer.push(p, &mut tree, *number, *score)?;
                        entries += 1;
                    }
                }
                scan.next(p)?;
            }
        }
        writer.finish(p, &mut tree)?;
    }
    drop(scans);
    let tree = tree.finish(p)?;
    for segment in &state.segments[from..to] {
        btree::destroy(p, segment.root, &mut |_, _| Ok(()))?;
    }
    let merged = (rows > 0).then_some(Segment { root: tree, low, high, entries, rows, dead: 0 });
    state.segments.splice(from..to, merged);
    Ok(())
}

// ---------------------------------------------------------------- search

/// The rows whose indexed text holds every word of `query`, each with
/// its rank, in primary key order.
pub fn search(ctx: &Ctx, table: &Table, index: usize, lead: &[u8], query: &str, match_prefix: bool) -> Result<Vec<(Vec<u8>, f64)>> {
    let IndexKind::FullText { config, .. } = &table.indexes[index].def.kind else { return Err(malformed()) };
    let wanted = text::query_terms(query, *config);
    let root = table.index_roots[index];
    if wanted.is_empty() || root == NO_PAGE {
        return Ok(Vec::new());
    }
    let state = load(ctx.pages, root)?;
    // Indexed rows: each word's lists, narrowed word by word.
    let mut matched: Option<HashMap<u64, u32>> = None;
    let mut run: Vec<(u64, u32)> = Vec::new();
    for term in &wanted {
        let mut start = lead.to_vec();
        if match_prefix {
            keys::encode_text_prefix(&mut start, term);
        } else {
            keys::encode(&mut start, &Value::Text(term.clone()), false);
        }
        let mut found: HashMap<u64, u32> = HashMap::new();
        let mut steps = 0u32;
        for segment in &state.segments {
            let mut cursor = Cursor::new(ctx.pages, segment.root);
            cursor.seek(&start)?;
            while let (Some(key), Some(value)) = (cursor.key(), cursor.value()) {
                if !key.starts_with(&start) {
                    break;
                }
                run.clear();
                decode_run(split_run_key(key)?.1, value, &mut run)?;
                for (number, score) in &run {
                    if matched.as_ref().is_none_or(|m| m.contains_key(number)) {
                        *found.entry(*number).or_insert(0) += score;
                    }
                }
                steps += 1;
                if steps % 64 == 0 {
                    ctx.check_deadline()?;
                }
                cursor.next()?;
            }
        }
        if let Some(previous) = &matched {
            for (number, score) in &mut found {
                *score += previous[number];
            }
        }
        matched = Some(found);
    }
    let mut numbers: Vec<(u64, u32)> = matched.unwrap_or_default().into_iter().collect();
    numbers.sort_unstable();
    let mut out: Vec<(Vec<u8>, f64)> = Vec::with_capacity(numbers.len());
    for (n, (number, score)) in numbers.into_iter().enumerate() {
        if n % 256 == 255 {
            ctx.check_deadline()?;
        }
        // A number that leads nowhere is a row since deleted or changed.
        if let Some(pk) = btree::get(ctx.pages, root, &tagged(KEY_OF, &number.to_be_bytes()))? {
            out.push((pk, f64::from(score) / SCORE_UNIT));
        }
    }
    // Pending rows: their stored words, read as they are.
    if state.pending_rows > 0 {
        let mut scores = vec![0u32; wanted.len()];
        for_each_pending(ctx.pages, state.pending, &mut |pk, stored| {
            let (stored_lead, pos) = stored_lead(stored)?;
            if stored_lead != lead {
                return Ok(());
            }
            scores.iter_mut().for_each(|s| *s = 0);
            stored_terms(stored, pos, &mut |term, score| {
                for (wanted, sum) in wanted.iter().zip(&mut scores) {
                    let hit = if match_prefix { term.starts_with(wanted.as_bytes()) } else { term == wanted.as_bytes() };
                    if hit {
                        *sum += score;
                    }
                }
            })?;
            if scores.iter().all(|s| *s > 0) {
                out.push((pk.to_vec(), f64::from(scores.iter().sum::<u32>()) / SCORE_UNIT));
            }
            Ok(())
        })?;
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

// ----------------------------------------------------------------- check

/// A row's score for a word in a segment, if the segment lists it.
fn listed<P: Pages + ?Sized>(p: &P, segment: &Segment, word: &[u8], number: u64) -> Result<Option<u32>> {
    let mut key = word.to_vec();
    key.extend_from_slice(&number.to_be_bytes());
    let mut cursor = Cursor::new(p, segment.root);
    cursor.seek_back(&key, true)?;
    let mut run = Vec::new();
    match (cursor.key(), cursor.value()) {
        (Some(k), Some(value)) if k.len() == key.len() && k.starts_with(word) => decode_run(split_run_key(k)?.1, value, &mut run)?,
        _ => return Ok(None),
    }
    Ok(run.iter().find(|(n, _)| *n == number).map(|(_, score)| *score))
}

/// Checks the index against its table, and calls `on_page` with every
/// page of its trees.
pub fn verify(ctx: &Ctx, table: &Table, index: usize, on_page: &mut dyn FnMut(PageId) -> Result<()>) -> Result<()> {
    let root = table.index_roots[index];
    let name = &table.indexes[index].def.name;
    let wrong = |what: &str| Error::corrupt(format!("full-text index {name:?}: {what}"));
    let state = load(ctx.pages, root)?;
    // Its own tree: numbers and keys lead to each other.
    let (mut pending, mut numbered, mut keyed) = (0u64, 0u64, 0u64);
    btree::walk(ctx.pages, root, on_page, &mut |key, value| {
        match key[0] {
            STATE => {}
            NUMBER_OF => {
                numbered += 1;
                let number = varint(value, &mut 0)?;
                if number >= state.next || btree::get(ctx.pages, root, &tagged(KEY_OF, &number.to_be_bytes()))?.as_deref() != Some(&key[1..]) {
                    return Err(wrong("a row's number does not lead back to it"));
                }
            }
            KEY_OF => keyed += 1,
            _ => return Err(wrong("its tree holds an entry of no known kind")),
        }
        Ok(())
    })?;
    btree::walk(ctx.pages, state.pending, on_page, &mut |key, _| {
        if key.ends_with(&[0, 0]) {
            pending += 1;
        }
        Ok(())
    })?;
    if pending != state.pending_rows || numbered != keyed {
        return Err(wrong("its counts do not add up"));
    }
    // Its segments: rising lists, each inside its segment's numbers.
    let mut above = 0u64;
    for segment in &state.segments {
        if segment.low <= above || segment.high < segment.low || segment.high >= state.next {
            return Err(wrong("its segments' numbers are out of order"));
        }
        above = segment.high;
        let mut entries = 0u64;
        let mut last: Option<(Vec<u8>, u64)> = None;
        let mut run = Vec::new();
        btree::walk(ctx.pages, segment.root, on_page, &mut |key, value| {
            let (word, first) = split_run_key(key)?;
            run.clear();
            decode_run(first, value, &mut run)?;
            for (n, (number, _)) in run.iter().enumerate() {
                let rising = match &last {
                    Some((w, before)) if w.as_slice() == word => *number > *before,
                    _ => true,
                };
                if !rising || (n == 0 && *number != first) || *number < segment.low || *number > segment.high {
                    return Err(wrong("a word's rows are out of order or outside their segment"));
                }
                last = Some((word.to_vec(), *number));
            }
            entries += run.len() as u64;
            Ok(())
        })?;
        if entries != segment.entries {
            return Err(wrong("a segment miscounts its entries"));
        }
    }
    // Every row with words is in it once, with exactly those words.
    let mut held = 0u64;
    let mut rows = Cursor::new(ctx.pages, table.root);
    rows.first()?;
    while let (Some(pk), Some(value)) = (rows.key(), rows.value()) {
        let words = words_of(ctx, table, index, &Row::parse(value.to_vec())?)?;
        let (parts, _) = pending_parts(ctx.pages, state.pending, pk)?;
        let number = btree::get(ctx.pages, root, &tagged(NUMBER_OF, pk))?;
        match (words, parts.is_empty(), number) {
            (None, true, None) => {}
            (Some(words), false, None) => {
                held += 1;
                let mut stored = Vec::new();
                for key in &parts {
                    stored.extend(btree::get(ctx.pages, state.pending, key)?.ok_or_else(malformed)?);
                }
                if decode_words(&stored)? != words {
                    return Err(wrong("a pending row's words are not its row's"));
                }
            }
            (Some(words), true, Some(number)) => {
                held += 1;
                let number = varint(&number, &mut 0)?;
                let segment = state.segments.iter().find(|s| s.low <= number && number <= s.high).ok_or_else(|| wrong("a row's number is in no segment"))?;
                for (term, score) in &words.terms {
                    if listed(ctx.pages, segment, &word_key(&words.lead, term), number)? != Some(*score) {
                        return Err(wrong("a word of a row is not listed for it"));
                    }
                }
            }
            _ => return Err(wrong("it disagrees with a row of its table")),
        }
        rows.next()?;
    }
    if held != pending + numbered {
        return Err(wrong("it holds rows that are not in the table"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::tests::MemPages;
    use crate::vfs::Rng;
    use std::collections::BTreeMap;

    fn words(lead: u8, terms: &[(&str, u32)]) -> Words {
        let mut terms: Vec<(String, u32)> = terms.iter().map(|(t, s)| (t.to_string(), *s)).collect();
        terms.sort();
        Words { lead: vec![lead], terms }
    }

    /// The rows listed for a word, read the way a search reads them.
    fn rows_with(p: &MemPages, root: PageId, lead: u8, term: &str) -> Vec<(Vec<u8>, u32)> {
        let state = load(p, root).unwrap();
        let start = word_key(&[lead], term);
        let mut out = Vec::new();
        for segment in &state.segments {
            let mut cursor = Cursor::new(p, segment.root);
            cursor.seek(&start).unwrap();
            while let (Some(key), Some(value)) = (cursor.key(), cursor.value()) {
                if !key.starts_with(&start) {
                    break;
                }
                let mut run = Vec::new();
                decode_run(split_run_key(key).unwrap().1, value, &mut run).unwrap();
                for (number, score) in run {
                    if let Some(pk) = btree::get(p, root, &tagged(KEY_OF, &number.to_be_bytes())).unwrap() {
                        out.push((pk, score));
                    }
                }
                cursor.next().unwrap();
            }
        }
        for_each_pending(p, state.pending, &mut |pk, stored| {
            let w = decode_words(stored)?;
            if w.lead == [lead] {
                if let Some((_, score)) = w.terms.iter().find(|(t, _)| t == term) {
                    out.push((pk.to_vec(), *score));
                }
            }
            Ok(())
        })
        .unwrap();
        out.sort();
        out
    }

    #[test]
    fn rows_are_found_while_pending_and_after_they_are_merged() {
        let mut p = MemPages::new();
        let mut root = NO_PAGE;
        root = add(&mut p, root, b"a", &words(1, &[("red", 16), ("fox", 32)])).unwrap();
        root = add(&mut p, root, b"b", &words(1, &[("red", 48)])).unwrap();
        root = add(&mut p, root, b"c", &words(2, &[("red", 16)])).unwrap();
        assert_eq!(rows_with(&p, root, 1, "red"), [(b"a".to_vec(), 16), (b"b".to_vec(), 48)]);
        let mut state = load(&p, root).unwrap();
        assert_eq!((state.pending_rows, state.segments.len()), (3, 0));
        root = flush(&mut p, root, &mut state).unwrap();
        root = store(&mut p, root, &state).unwrap();
        assert_eq!((state.pending_rows, state.pending_bytes, state.segments.len(), state.next), (0, 0, 1, 4));
        assert_eq!(rows_with(&p, root, 1, "red"), [(b"a".to_vec(), 16), (b"b".to_vec(), 48)]);
        assert_eq!(rows_with(&p, root, 2, "red"), [(b"c".to_vec(), 16)]);
        assert_eq!(rows_with(&p, root, 1, "fox"), [(b"a".to_vec(), 32)]);
        // A removed row is no longer found, pending or merged.
        root = remove(&mut p, root, b"a").unwrap();
        root = add(&mut p, root, b"d", &words(1, &[("red", 16)])).unwrap();
        root = remove(&mut p, root, b"d").unwrap();
        assert_eq!(rows_with(&p, root, 1, "red"), [(b"b".to_vec(), 48)]);
        assert!(rows_with(&p, root, 1, "fox").is_empty());
        destroy(&mut p, root).unwrap();
        assert!(p.pages.is_empty(), "{} pages leaked", p.pages.len());
    }

    #[test]
    fn many_rows_keep_few_segments_and_lose_their_dead() {
        let mut rng = Rng::new(7);
        let mut p = MemPages::new();
        let mut root = NO_PAGE;
        let vocabulary: Vec<String> = (0..300).map(|i| format!("w{i}")).collect();
        let mut model: BTreeMap<Vec<u8>, Words> = BTreeMap::new();
        let mut most_segments = 0;
        for step in 0..6000u32 {
            let pk = format!("row{:04}", rng.below(1500)).into_bytes();
            if model.remove(&pk).is_some() {
                root = remove(&mut p, root, &pk).unwrap();
            }
            if rng.below(4) > 0 {
                let mut terms: Vec<(String, u32)> = Vec::new();
                for _ in 0..60 {
                    let term = &vocabulary[rng.below(vocabulary.len() as u64) as usize];
                    if !terms.iter().any(|(t, _)| t == term) {
                        terms.push((term.clone(), 1 + rng.below(40) as u32));
                    }
                }
                terms.sort();
                let w = Words { lead: vec![(step % 2) as u8], terms };
                root = add(&mut p, root, &pk, &w).unwrap();
                model.insert(pk, w);
            }
            p.commit();
            most_segments = most_segments.max(load(&p, root).unwrap().segments.len());
        }
        let state = load(&p, root).unwrap();
        assert!(state.segments.len() >= 2 && most_segments <= 12, "{} segments now, {most_segments} at most", state.segments.len());
        // No segment is mostly dead, and together they hold little more
        // than the rows that are left.
        assert!(state.segments.iter().all(|s| s.dead * 2 <= s.rows));
        let live: u64 = model.values().map(|w| w.terms.len() as u64).sum();
        let held: u64 = state.segments.iter().map(|s| s.entries).sum();
        assert!(held <= live * 2, "{held} entries held for {live} live");
        for lead in 0..2u8 {
            for term in ["w0", "w7", "w299"] {
                let mut expected: Vec<(Vec<u8>, u32)> = model
                    .iter()
                    .filter(|(_, w)| w.lead == [lead])
                    .filter_map(|(pk, w)| w.terms.iter().find(|(t, _)| t == term).map(|(_, s)| (pk.clone(), *s)))
                    .collect();
                expected.sort();
                assert_eq!(rows_with(&p, root, lead, term), expected, "{lead} {term}");
            }
        }
        // Everything is given back.
        let pks: Vec<Vec<u8>> = model.keys().cloned().collect();
        for pk in pks {
            root = remove(&mut p, root, &pk).unwrap();
        }
        let state = load(&p, root).unwrap();
        assert_eq!(state.segments.iter().map(|s| s.rows - s.dead).sum::<u64>(), 0);
        destroy(&mut p, root).unwrap();
        assert!(p.pages.is_empty(), "{} pages leaked", p.pages.len());
    }
}
