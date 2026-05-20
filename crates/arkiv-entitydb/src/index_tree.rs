//! Adaptive Radix Tree (ART) — ordered set of byte-string keys.
//!
//! Used as the Tier-2 index: one tree per attribute key, stored in the
//! account `code` field at `index_address(attr_key)`.
//!
//! ### Node types
//!
//! | Type    | Children | Child lookup   |
//! |---------|----------|----------------|
//! | Node4   | 1–4      | linear scan    |
//! | Node16  | 5–16     | linear scan    |
//! | Node48  | 17–48    | 256-byte index |
//! | Node256 | 49–256   | direct array   |
//!
//! Each inner node carries a **path-compressed prefix** (bytes shared by
//! all keys in the subtree) and a `has_end` flag that marks the key
//! formed by the path from root to this node's position as being itself
//! present in the set (needed when one key is a prefix of another).
//!
//! Leaves store only the **key suffix** — bytes not yet consumed by
//! ancestor path bytes — so long shared prefixes are stored once.
//!
//! ### Serialisation format
//!
//! ```text
//! Tree    := COUNT:u32-BE  Node
//! Node    := 0x00                                           (absent)
//!          | 0x01  LEN:u16-BE  suffix:LEN                  (Leaf)
//!          | 0x02  PFX  HAS:u8  N:u8  (KEY:u8 Node)×N     (Node4)
//!          | 0x03  PFX  HAS:u8  N:u8  (KEY:u8 Node)×N     (Node16)
//!          | 0x04  PFX  HAS:u8  N:u8  IDX:u8[256] Node×N  (Node48)
//!          | 0x05  PFX  HAS:u8  Node×256                   (Node256)
//! PFX     := PFX_LEN:u8  PFX_LEN bytes
//! ```
//!
//! Node4/16 children written in ascending key-byte order. Node48 in
//! slot-index order. Node256 in byte order 0..=255. Encoding is
//! fully deterministic for any given key set.

use eyre::{Result, bail};

// ─── Tag bytes ──────────────────────────────────────────────────────────────

const TAG_EMPTY: u8 = 0x00;
const TAG_LEAF:  u8 = 0x01;
const TAG_N4:    u8 = 0x02;
const TAG_N16:   u8 = 0x03;
const TAG_N48:   u8 = 0x04;
const TAG_N256:  u8 = 0x05;
const N48_NIL:   u8 = 0xFF;

// ─── Node ───────────────────────────────────────────────────────────────────

#[derive(Default)]
enum Node {
    #[default]
    Empty,
    /// Key suffix from the current depth downward (full key = path_bytes + suffix).
    Leaf(Vec<u8>),
    N4(Box<N4>),
    N16(Box<N16>),
    N48(Box<N48>),
    N256(Box<N256>),
}

// ─── Inner node structs ─────────────────────────────────────────────────────

struct N4 {
    prefix:  Vec<u8>,
    has_end: bool,
    n:       u8,
    keys:    [u8; 4],
    ch:      [Node; 4],
}

struct N16 {
    prefix:  Vec<u8>,
    has_end: bool,
    n:       u8,
    keys:    [u8; 16],
    ch:      [Node; 16],
}

struct N48 {
    prefix:  Vec<u8>,
    has_end: bool,
    n:       u8,
    idx:     Box<[u8; 256]>, // byte → slot, N48_NIL if absent
    ch:      [Node; 48],
}

struct N256 {
    prefix:  Vec<u8>,
    has_end: bool,
    n:       u16,
    ch:      Box<[Node; 256]>,
}

// ─── Constructors ───────────────────────────────────────────────────────────

impl N4 {
    fn empty(prefix: Vec<u8>, has_end: bool) -> Self {
        Self { prefix, has_end, n: 0, keys: [0; 4], ch: std::array::from_fn(|_| Node::Empty) }
    }
}
impl N16 {
    fn empty(prefix: Vec<u8>, has_end: bool) -> Self {
        Self { prefix, has_end, n: 0, keys: [0; 16], ch: std::array::from_fn(|_| Node::Empty) }
    }
}
impl N48 {
    fn empty(prefix: Vec<u8>, has_end: bool) -> Self {
        Self {
            prefix, has_end, n: 0,
            idx: Box::new([N48_NIL; 256]),
            ch: std::array::from_fn(|_| Node::Empty),
        }
    }
}
impl N256 {
    fn empty(prefix: Vec<u8>, has_end: bool) -> Self {
        Self { prefix, has_end, n: 0, ch: Box::new(std::array::from_fn(|_| Node::Empty)) }
    }
}

/// Move a boxed inner-node variant out of `slot`, leaving `Node::Empty`.
macro_rules! take_box {
    ($slot:expr, $var:ident) => {
        if let Node::$var(b) = std::mem::replace($slot, Node::Empty) { b } else { unreachable!() }
    };
}

// ─── Node-type upgrades / downgrades ────────────────────────────────────────

fn n4_to_n16(n4: N4) -> N16 {
    let mut n16 = N16::empty(n4.prefix, n4.has_end);
    n16.n = n4.n;
    for i in 0..n4.n as usize { n16.keys[i] = n4.keys[i]; }
    for (i, ch) in n4.ch.into_iter().enumerate() { n16.ch[i] = ch; }
    n16
}

fn n16_to_n48(n16: N16) -> N48 {
    let mut n48 = N48::empty(n16.prefix, n16.has_end);
    n48.n = n16.n;
    for (slot, (key, ch)) in n16.keys[..n16.n as usize]
        .iter().copied()
        .zip(n16.ch.into_iter())
        .enumerate()
    {
        n48.idx[key as usize] = slot as u8;
        n48.ch[slot] = ch;
    }
    n48
}

fn n48_to_n256(n48: N48) -> N256 {
    let mut n256 = N256::empty(n48.prefix, n48.has_end);
    n256.n = n48.n as u16;
    let mut slot_byte = [N48_NIL; 48];
    for (byte, &slot) in n48.idx.iter().enumerate() {
        if slot != N48_NIL { slot_byte[slot as usize] = byte as u8; }
    }
    for (slot, ch) in n48.ch.into_iter().enumerate() {
        let byte = slot_byte[slot];
        if byte != N48_NIL { n256.ch[byte as usize] = ch; }
    }
    n256
}

fn n16_to_n4(n16: N16) -> N4 {
    let mut n4 = N4::empty(n16.prefix, n16.has_end);
    n4.n = n16.n;
    for i in 0..n16.n as usize { n4.keys[i] = n16.keys[i]; }
    for (i, ch) in n16.ch.into_iter().take(n16.n as usize).enumerate() { n4.ch[i] = ch; }
    n4
}

fn n48_to_n16(n48: N48) -> N16 {
    let mut pairs: Vec<(u8, usize)> = (0usize..256)
        .filter(|&b| n48.idx[b] != N48_NIL)
        .map(|b| (b as u8, n48.idx[b] as usize))
        .collect();
    pairs.sort_unstable_by_key(|&(b, _)| b);

    let mut ch_opts: Vec<Option<Node>> = n48.ch.into_iter().map(Some).collect();
    let mut n16 = N16::empty(n48.prefix, n48.has_end);
    n16.n = n48.n;
    for (pos, (byte, slot)) in pairs.iter().enumerate() {
        n16.keys[pos] = *byte;
        n16.ch[pos] = ch_opts[*slot].take().unwrap_or(Node::Empty);
    }
    n16
}

fn n256_to_n48(n256: N256) -> N48 {
    let mut n48 = N48::empty(n256.prefix, n256.has_end);
    n48.n = n256.n as u8;
    let mut slot = 0usize;
    for (byte, ch) in n256.ch.into_iter().enumerate() {
        if !matches!(ch, Node::Empty) {
            n48.idx[byte] = slot as u8;
            n48.ch[slot] = ch;
            slot += 1;
        }
    }
    n48
}

// ─── IndexTree ──────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct IndexTree {
    root: Node,
    len:  usize,
}

impl IndexTree {
    pub fn new() -> Self { Self::default() }

    pub fn is_empty(&self) -> bool { self.len == 0 }
    pub fn len(&self) -> usize { self.len }

    pub fn insert(&mut self, key: Vec<u8>) -> bool {
        if insert_node(&mut self.root, &key, 0) { self.len += 1; true } else { false }
    }

    pub fn remove(&mut self, key: &[u8]) -> bool {
        if remove_node(&mut self.root, key, 0) { self.len -= 1; true } else { false }
    }

    pub fn iter_gt<'a>(&'a self, lo: &'a [u8]) -> impl Iterator<Item = Vec<u8>> + 'a {
        self.filtered(move |k: &[u8]| k > lo)
    }
    pub fn iter_gte<'a>(&'a self, lo: &'a [u8]) -> impl Iterator<Item = Vec<u8>> + 'a {
        self.filtered(move |k: &[u8]| k >= lo)
    }
    pub fn iter_lt<'a>(&'a self, hi: &'a [u8]) -> impl Iterator<Item = Vec<u8>> + 'a {
        self.filtered(move |k: &[u8]| k < hi)
    }
    pub fn iter_lte<'a>(&'a self, hi: &'a [u8]) -> impl Iterator<Item = Vec<u8>> + 'a {
        self.filtered(move |k: &[u8]| k <= hi)
    }
    pub fn iter_prefix<'a>(&'a self, prefix: &'a [u8]) -> impl Iterator<Item = Vec<u8>> + 'a {
        self.filtered(move |k: &[u8]| k.starts_with(prefix))
    }

    fn filtered<'a, F>(&'a self, pred: F) -> impl Iterator<Item = Vec<u8>> + 'a
    where F: Fn(&[u8]) -> bool + 'a {
        let mut keys = Vec::new();
        collect_all(&self.root, &mut Vec::new(), &mut keys);
        keys.into_iter().filter(move |k| pred(k))
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(self.len as u32).to_be_bytes());
        ser_node(&self.root, &mut buf);
        buf
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < 4 { bail!("IndexTree: too short ({} bytes)", data.len()); }
        let len = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
        let (root, end) = de_node(data, 4)?;
        if end != data.len() { bail!("IndexTree: {} trailing bytes", data.len() - end); }
        Ok(Self { root, len })
    }
}

// ─── DFS collection ─────────────────────────────────────────────────────────

fn collect_all(node: &Node, path: &mut Vec<u8>, out: &mut Vec<Vec<u8>>) {
    match node {
        Node::Empty => {}
        Node::Leaf(sfx) => { let mut k = path.clone(); k.extend_from_slice(sfx); out.push(k); }
        Node::N4(n) => {
            let base = path.len();
            path.extend_from_slice(&n.prefix);
            if n.has_end { out.push(path.clone()); }
            for i in 0..n.n as usize { path.push(n.keys[i]); collect_all(&n.ch[i], path, out); path.pop(); }
            path.truncate(base);
        }
        Node::N16(n) => {
            let base = path.len();
            path.extend_from_slice(&n.prefix);
            if n.has_end { out.push(path.clone()); }
            for i in 0..n.n as usize { path.push(n.keys[i]); collect_all(&n.ch[i], path, out); path.pop(); }
            path.truncate(base);
        }
        Node::N48(n) => {
            let base = path.len();
            path.extend_from_slice(&n.prefix);
            if n.has_end { out.push(path.clone()); }
            for b in 0u16..=255 {
                let slot = n.idx[b as usize];
                if slot == N48_NIL { continue; }
                path.push(b as u8); collect_all(&n.ch[slot as usize], path, out); path.pop();
            }
            path.truncate(base);
        }
        Node::N256(n) => {
            let base = path.len();
            path.extend_from_slice(&n.prefix);
            if n.has_end { out.push(path.clone()); }
            for b in 0u16..=255 {
                path.push(b as u8); collect_all(&n.ch[b as usize], path, out); path.pop();
            }
            path.truncate(base);
        }
    }
}

// ─── Insert ─────────────────────────────────────────────────────────────────

fn insert_node(slot: &mut Node, key: &[u8], depth: usize) -> bool {
    // ── Empty ────────────────────────────────────────────────────────
    if matches!(slot, Node::Empty) {
        *slot = Node::Leaf(key[depth..].to_vec());
        return true;
    }

    // ── Leaf: split if keys differ ───────────────────────────────────
    if let Node::Leaf(existing_sfx) = slot {
        let new_sfx = &key[depth..];
        if existing_sfx.as_slice() == new_sfx { return false; }

        let common = common_prefix(existing_sfx, new_sfx);
        let mut n4  = N4::empty(existing_sfx[..common].to_vec(), false);

        if common < existing_sfx.len() {
            let b    = existing_sfx[common];
            let sfx2 = existing_sfx[common + 1..].to_vec();
            n4.add_ch(b, Node::Leaf(sfx2));
        } else {
            n4.has_end = true;
        }
        if common < new_sfx.len() {
            let b = new_sfx[common];
            n4.add_ch(b, Node::Leaf(new_sfx[common + 1..].to_vec()));
        } else {
            if n4.has_end { return false; }
            n4.has_end = true;
        }
        *slot = Node::N4(Box::new(n4));
        return true;
    }

    // ── Inner node ───────────────────────────────────────────────────
    //
    // We use short-lived borrows that end before the next mutable access,
    // so Rust's borrow checker stays happy.
    //
    // Step 1: read prefix info (borrow released at end of block).
    let (pfx_len, matched) = {
        let pfx = node_prefix(slot);
        let m = common_prefix(pfx, &key[depth..]);
        (pfx.len(), m)
    };

    // Step 2: prefix mismatch → split.
    if matched < pfx_len {
        // Collect what we need before mutating (all borrows from slot are
        // temporary and end before the line that mutates slot).
        let (branch_b, split_pfx, rest_pfx) = {
            let pfx = node_prefix(slot);
            (pfx[matched], pfx[..matched].to_vec(), pfx[matched + 1..].to_vec())
        };
        *node_prefix_mut(slot) = rest_pfx;

        // Move the whole existing node out.
        let existing = std::mem::replace(slot, Node::Empty);
        let mut n4 = N4::empty(split_pfx, false);
        n4.add_ch(branch_b, existing);

        let rem = &key[depth..];
        if matched < rem.len() {
            n4.add_ch(rem[matched], Node::Leaf(rem[matched + 1..].to_vec()));
        } else {
            n4.has_end = true;
        }
        *slot = Node::N4(Box::new(n4));
        return true;
    }

    // Step 3: prefix matched fully.
    let depth = depth + pfx_len;

    if depth == key.len() {
        // Key ends exactly at this inner node.
        if node_has_end(slot) { return false; }
        *node_has_end_mut(slot) = true;
        return true;
    }

    let b = key[depth];

    // Step 4: recurse into existing child (if present).
    //
    // `node_child_mut` borrows `slot` through its return value.  We return
    // immediately in this branch, so no second borrow of `slot` occurs.
    if node_has_child(slot, b) {
        let child = node_child_mut(slot, b).unwrap();
        return insert_node(child, key, depth + 1);
    }

    // Step 5: grow FIRST (ensures capacity), then add the new leaf.
    // Growing before adding guarantees add_ch is never called on a full
    // node, regardless of whether the current node was created by a
    // shrink that left it at maximum capacity.
    node_maybe_grow(slot);
    node_add_child(slot, b, Node::Leaf(key[depth + 1..].to_vec()));
    true
}

// ─── Remove ─────────────────────────────────────────────────────────────────

fn remove_node(slot: &mut Node, key: &[u8], depth: usize) -> bool {
    if matches!(slot, Node::Empty) { return false; }

    if let Node::Leaf(sfx) = slot {
        if sfx.as_slice() == &key[depth..] { *slot = Node::Empty; return true; }
        return false;
    }

    let (pfx_len, matched) = {
        let pfx = node_prefix(slot);
        let m = common_prefix(pfx, &key[depth..]);
        (pfx.len(), m)
    };
    if matched < pfx_len { return false; }

    let depth = depth + pfx_len;

    if depth == key.len() {
        if !node_has_end(slot) { return false; }
        *node_has_end_mut(slot) = false;
        node_maybe_shrink(slot);
        return true;
    }

    let b = key[depth];
    if !node_has_child(slot, b) { return false; }

    // Recurse — separate borrow block.
    let removed = {
        let child = node_child_mut(slot, b).unwrap();
        remove_node(child, key, depth + 1)
    };

    if removed {
        // If child is now Empty, remove it from this node.
        if node_child_is_empty(slot, b) {
            node_del_child(slot, b);
            node_maybe_shrink(slot);
        }
    }
    removed
}

// ─── Node-level dispatch helpers ────────────────────────────────────────────
//
// These take a `&Node` or `&mut Node` (not a pattern match variable), so there
// is no double-borrow: each call is a fresh, short borrow.

fn node_prefix(node: &Node) -> &[u8] {
    match node {
        Node::N4(n)   => &n.prefix,
        Node::N16(n)  => &n.prefix,
        Node::N48(n)  => &n.prefix,
        Node::N256(n) => &n.prefix,
        _ => &[],
    }
}

fn node_prefix_mut(node: &mut Node) -> &mut Vec<u8> {
    match node {
        Node::N4(n)   => &mut n.prefix,
        Node::N16(n)  => &mut n.prefix,
        Node::N48(n)  => &mut n.prefix,
        Node::N256(n) => &mut n.prefix,
        _ => panic!("node_prefix_mut on non-inner node"),
    }
}

fn node_has_end(node: &Node) -> bool {
    match node {
        Node::N4(n)   => n.has_end,
        Node::N16(n)  => n.has_end,
        Node::N48(n)  => n.has_end,
        Node::N256(n) => n.has_end,
        _ => false,
    }
}

fn node_has_end_mut(node: &mut Node) -> &mut bool {
    match node {
        Node::N4(n)   => &mut n.has_end,
        Node::N16(n)  => &mut n.has_end,
        Node::N48(n)  => &mut n.has_end,
        Node::N256(n) => &mut n.has_end,
        _ => panic!("node_has_end_mut on non-inner node"),
    }
}

fn node_has_child(node: &Node, b: u8) -> bool {
    match node {
        Node::N4(n)   => (0..n.n as usize).any(|i| n.keys[i] == b),
        Node::N16(n)  => (0..n.n as usize).any(|i| n.keys[i] == b),
        Node::N48(n)  => n.idx[b as usize] != N48_NIL,
        Node::N256(n) => !matches!(n.ch[b as usize], Node::Empty),
        _ => false,
    }
}

fn node_child_mut(node: &mut Node, b: u8) -> Option<&mut Node> {
    match node {
        Node::N4(n)   => (0..n.n as usize).find(|&i| n.keys[i] == b).map(|i| &mut n.ch[i]),
        Node::N16(n)  => (0..n.n as usize).find(|&i| n.keys[i] == b).map(|i| &mut n.ch[i]),
        Node::N48(n)  => {
            let slot = n.idx[b as usize];
            if slot == N48_NIL { None } else { Some(&mut n.ch[slot as usize]) }
        }
        Node::N256(n) => match &mut n.ch[b as usize] { Node::Empty => None, ch => Some(ch) },
        _ => None,
    }
}

fn node_child_is_empty(node: &Node, b: u8) -> bool {
    match node {
        Node::N4(n)   => (0..n.n as usize).find(|&i| n.keys[i] == b).map_or(true,  |i| matches!(n.ch[i], Node::Empty)),
        Node::N16(n)  => (0..n.n as usize).find(|&i| n.keys[i] == b).map_or(true,  |i| matches!(n.ch[i], Node::Empty)),
        Node::N48(n)  => {
            let slot = n.idx[b as usize];
            slot == N48_NIL || matches!(n.ch[slot as usize], Node::Empty)
        }
        Node::N256(n) => matches!(n.ch[b as usize], Node::Empty),
        _ => true,
    }
}

fn node_add_child(node: &mut Node, b: u8, child: Node) {
    match node {
        Node::N4(n)   => n.add_ch(b, child),
        Node::N16(n)  => n.add_ch(b, child),
        Node::N48(n)  => n.add_ch(b, child),
        Node::N256(n) => n.add_ch(b, child),
        _ => {}
    }
}

fn node_del_child(node: &mut Node, b: u8) {
    match node {
        Node::N4(n)   => n.del_ch(b),
        Node::N16(n)  => n.del_ch(b),
        Node::N48(n)  => n.del_ch(b),
        Node::N256(n) => n.del_ch(b),
        _ => {}
    }
}

fn node_n_children(node: &Node) -> usize {
    match node {
        Node::N4(n)   => n.n as usize,
        Node::N16(n)  => n.n as usize,
        Node::N48(n)  => n.n as usize,
        Node::N256(n) => n.n as usize,
        _ => 0,
    }
}

fn node_maybe_grow(slot: &mut Node) {
    match slot {
        Node::N4(n) if n.n == 4 => {
            let old = take_box!(slot, N4);
            *slot = Node::N16(Box::new(n4_to_n16(*old)));
        }
        Node::N16(n) if n.n == 16 => {
            let old = take_box!(slot, N16);
            *slot = Node::N48(Box::new(n16_to_n48(*old)));
        }
        Node::N48(n) if n.n == 48 => {
            let old = take_box!(slot, N48);
            *slot = Node::N256(Box::new(n48_to_n256(*old)));
        }
        _ => {}
    }
}

fn node_maybe_shrink(slot: &mut Node) {
    let (nc, he) = (node_n_children(slot), node_has_end(slot));

    if nc == 0 && !he {
        *slot = Node::Empty;
        return;
    }

    match slot {
        // N4 with one child and no has_end: merge prefix + branch byte into child.
        Node::N4(n) if n.n == 1 && !n.has_end => {
            let b     = n.keys[0];
            let pfx   = n.prefix.clone();
            let child = std::mem::replace(&mut n.ch[0], Node::Empty);
            prepend_into(slot, &pfx, b, child);
        }
        // N16 shrinks to N4.
        Node::N16(n) if n.n <= 4 => {
            let old = take_box!(slot, N16);
            *slot = Node::N4(Box::new(n16_to_n4(*old)));
        }
        // N48 shrinks to N16.
        Node::N48(n) if n.n <= 16 => {
            let old = take_box!(slot, N48);
            *slot = Node::N16(Box::new(n48_to_n16(*old)));
        }
        // N256 shrinks to N48.
        Node::N256(n) if n.n <= 48 => {
            let old = take_box!(slot, N256);
            *slot = Node::N48(Box::new(n256_to_n48(*old)));
        }
        _ => {}
    }
}

/// Merge `parent_pfx + branch_b` into the child's prefix, then put the
/// child back into `slot`.  Called when N4 shrinks to a single child.
fn prepend_into(slot: &mut Node, parent_pfx: &[u8], branch_b: u8, child: Node) {
    fn prepend(p: &[u8], b: u8, pfx: &mut Vec<u8>) {
        let mut new = p.to_vec(); new.push(b); new.extend_from_slice(pfx); *pfx = new;
    }
    match child {
        Node::Leaf(mut sfx) => {
            let mut merged = parent_pfx.to_vec(); merged.push(branch_b); merged.extend_from_slice(&sfx); sfx = merged;
            *slot = Node::Leaf(sfx);
        }
        Node::N4(mut n)   => { prepend(parent_pfx, branch_b, &mut n.prefix);   *slot = Node::N4(n); }
        Node::N16(mut n)  => { prepend(parent_pfx, branch_b, &mut n.prefix);   *slot = Node::N16(n); }
        Node::N48(mut n)  => { prepend(parent_pfx, branch_b, &mut n.prefix);   *slot = Node::N48(n); }
        Node::N256(mut n) => { prepend(parent_pfx, branch_b, &mut n.prefix);   *slot = Node::N256(n); }
        Node::Empty       => { *slot = Node::Empty; }
    }
}

// ─── Per-type child operations ───────────────────────────────────────────────

impl N4 {
    fn add_ch(&mut self, b: u8, child: Node) {
        let pos = (0..self.n as usize).take_while(|&i| self.keys[i] < b).count();
        let n = self.n as usize;
        for i in (pos..n).rev() { self.keys[i + 1] = self.keys[i]; self.ch.swap(i, i + 1); }
        self.keys[pos] = b; self.ch[pos] = child; self.n += 1;
    }
    fn del_ch(&mut self, b: u8) {
        let Some(pos) = (0..self.n as usize).find(|&i| self.keys[i] == b) else { return };
        let n = self.n as usize;
        for i in pos..n - 1 { self.keys[i] = self.keys[i + 1]; self.ch.swap(i, i + 1); }
        self.ch[n - 1] = Node::Empty; self.n -= 1;
    }
}

impl N16 {
    fn add_ch(&mut self, b: u8, child: Node) {
        let pos = (0..self.n as usize).take_while(|&i| self.keys[i] < b).count();
        let n = self.n as usize;
        for i in (pos..n).rev() { self.keys[i + 1] = self.keys[i]; self.ch.swap(i, i + 1); }
        self.keys[pos] = b; self.ch[pos] = child; self.n += 1;
    }
    fn del_ch(&mut self, b: u8) {
        let Some(pos) = (0..self.n as usize).find(|&i| self.keys[i] == b) else { return };
        let n = self.n as usize;
        for i in pos..n - 1 { self.keys[i] = self.keys[i + 1]; self.ch.swap(i, i + 1); }
        self.ch[n - 1] = Node::Empty; self.n -= 1;
    }
}

impl N48 {
    fn add_ch(&mut self, b: u8, child: Node) {
        let slot = (0..48).find(|&i| matches!(self.ch[i], Node::Empty)).expect("N48 full");
        self.idx[b as usize] = slot as u8; self.ch[slot] = child; self.n += 1;
    }
    fn del_ch(&mut self, b: u8) {
        let s = self.idx[b as usize];
        if s == N48_NIL { return; }
        self.ch[s as usize] = Node::Empty; self.idx[b as usize] = N48_NIL; self.n -= 1;
    }
}

impl N256 {
    fn add_ch(&mut self, b: u8, child: Node) { self.ch[b as usize] = child; self.n += 1; }
    fn del_ch(&mut self, b: u8) {
        if !matches!(self.ch[b as usize], Node::Empty) { self.ch[b as usize] = Node::Empty; self.n -= 1; }
    }
}

// ─── Utility ────────────────────────────────────────────────────────────────

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

// ─── Serialisation ──────────────────────────────────────────────────────────

fn ser_node(node: &Node, buf: &mut Vec<u8>) {
    match node {
        Node::Empty => { buf.push(TAG_EMPTY); }
        Node::Leaf(sfx) => {
            buf.push(TAG_LEAF);
            buf.extend_from_slice(&(sfx.len() as u16).to_be_bytes());
            buf.extend_from_slice(sfx);
        }
        Node::N4(n) => {
            buf.push(TAG_N4); ser_pfx(&n.prefix, n.has_end, buf);
            buf.push(n.n);
            for i in 0..n.n as usize { buf.push(n.keys[i]); ser_node(&n.ch[i], buf); }
        }
        Node::N16(n) => {
            buf.push(TAG_N16); ser_pfx(&n.prefix, n.has_end, buf);
            buf.push(n.n);
            for i in 0..n.n as usize { buf.push(n.keys[i]); ser_node(&n.ch[i], buf); }
        }
        Node::N48(n) => {
            buf.push(TAG_N48); ser_pfx(&n.prefix, n.has_end, buf);
            buf.push(n.n);
            buf.extend_from_slice(n.idx.as_ref());
            for i in 0..n.n as usize { ser_node(&n.ch[i], buf); }
        }
        Node::N256(n) => {
            buf.push(TAG_N256); ser_pfx(&n.prefix, n.has_end, buf);
            for i in 0..256usize { ser_node(&n.ch[i], buf); }
        }
    }
}

fn ser_pfx(prefix: &[u8], has_end: bool, buf: &mut Vec<u8>) {
    assert!(prefix.len() <= 255);
    buf.push(prefix.len() as u8);
    buf.extend_from_slice(prefix);
    buf.push(has_end as u8);
}

// ─── Deserialisation ────────────────────────────────────────────────────────

fn de_node(data: &[u8], pos: usize) -> Result<(Node, usize)> {
    let &tag = data.get(pos).ok_or_else(|| eyre::eyre!("IndexTree: truncated (tag)"))?;
    match tag {
        TAG_EMPTY => Ok((Node::Empty, pos + 1)),
        TAG_LEAF => {
            let len = u16_at(data, pos + 1, "Leaf len")? as usize;
            let end = pos + 3 + len;
            if data.len() < end { bail!("IndexTree: truncated Leaf data"); }
            Ok((Node::Leaf(data[pos + 3..end].to_vec()), end))
        }
        TAG_N4 | TAG_N16 => {
            let (prefix, has_end, mut p) = de_pfx(data, pos + 1)?;
            let n = u8_at(data, p, "n")? as usize; p += 1;
            let limit = if tag == TAG_N4 { 4 } else { 16 };
            if n > limit { bail!("IndexTree: tag {tag:#x} n={n} > {limit}"); }
            let (mut keys, mut children) = (Vec::with_capacity(n), Vec::with_capacity(n));
            for _ in 0..n {
                keys.push(u8_at(data, p, "key")?); p += 1;
                let (ch, next) = de_node(data, p)?; p = next;
                children.push(ch);
            }
            let node = if tag == TAG_N4 {
                let mut nd = N4::empty(prefix, has_end); nd.n = n as u8;
                for i in 0..n { nd.keys[i] = keys[i]; nd.ch[i] = children.remove(0); }
                Node::N4(Box::new(nd))
            } else {
                let mut nd = N16::empty(prefix, has_end); nd.n = n as u8;
                for i in 0..n { nd.keys[i] = keys[i]; nd.ch[i] = children.remove(0); }
                Node::N16(Box::new(nd))
            };
            Ok((node, p))
        }
        TAG_N48 => {
            let (prefix, has_end, mut p) = de_pfx(data, pos + 1)?;
            let n = u8_at(data, p, "N48 n")? as usize; p += 1;
            if data.len() < p + 256 { bail!("IndexTree: truncated N48 index"); }
            let mut idx = Box::new([N48_NIL; 256]);
            idx.copy_from_slice(&data[p..p + 256]); p += 256;
            let mut ch: [Node; 48] = std::array::from_fn(|_| Node::Empty);
            for i in 0..n { let (child, next) = de_node(data, p)?; p = next; ch[i] = child; }
            Ok((Node::N48(Box::new(N48 { prefix, has_end, n: n as u8, idx, ch })), p))
        }
        TAG_N256 => {
            let (prefix, has_end, mut p) = de_pfx(data, pos + 1)?;
            let mut ch: Box<[Node; 256]> = Box::new(std::array::from_fn(|_| Node::Empty));
            let mut n = 0u16;
            for i in 0..256usize {
                let (child, next) = de_node(data, p)?; p = next;
                if !matches!(child, Node::Empty) { n += 1; }
                ch[i] = child;
            }
            Ok((Node::N256(Box::new(N256 { prefix, has_end, n, ch })), p))
        }
        other => bail!("IndexTree: unknown tag {other:#x}"),
    }
}

fn de_pfx(data: &[u8], pos: usize) -> Result<(Vec<u8>, bool, usize)> {
    let pfx_len = u8_at(data, pos, "pfx_len")? as usize;
    let end = pos + 1 + pfx_len;
    if data.len() < end + 1 { bail!("IndexTree: truncated prefix"); }
    let prefix  = data[pos + 1..end].to_vec();
    let has_end = data[end] != 0;
    Ok((prefix, has_end, end + 1))
}

fn u8_at(data: &[u8], pos: usize, ctx: &str) -> Result<u8> {
    data.get(pos).copied().ok_or_else(|| eyre::eyre!("IndexTree: truncated ({ctx})"))
}

fn u16_at(data: &[u8], pos: usize, ctx: &str) -> Result<u16> {
    data.get(pos..pos + 2)
        .map(|s| u16::from_be_bytes(s.try_into().unwrap()))
        .ok_or_else(|| eyre::eyre!("IndexTree: truncated ({ctx})"))
}
