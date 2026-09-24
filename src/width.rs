//! Display width, in terminal columns, for the caret under an echoed source
//! line (`diag.rs`).
//!
//! # Why the compiler needs this at all
//!
//! A column counts CODE POINTS -- that is what an editor jumps to when it is
//! handed `line:col`, so the number in a diagnostic cannot become anything
//! else. But the echoed source line is printed raw, and a terminal draws it
//! in CELLS: `漢` takes two, a combining mark takes none. Padding the caret
//! with one space per code point therefore puts it too far left on a line
//! holding CJK or an emoji, and too far right on one holding combining
//! marks. The number stays code points; only the caret's padding is width.
//!
//! # Where the rule and the data come from
//!
//! `lib/unicode.src` already decided the rule and carries the tables, and
//! there must not be a second, drifting copy of either. So there is not one:
//! this module **parses the tables out of the embedded text of
//! `lib/unicode.src`** -- literally the same bytes the standard library
//! compiles, since `stdlib::source` carries the module inside the binary --
//! and implements the same algorithm over them:
//!
//! * cluster the text by UAX #29 (`unicode.cluster_starts`);
//! * measure each cluster by its base, U+FE0F, or a regional-indicator pair
//!   (`unicode.cluster_width`);
//! * look a code point up in `WIDTH_RANGES`, default 1 (`unicode.char_width`).
//!
//! A regeneration of the tables for a new Unicode version therefore lands in
//! both at once, and `src/tests.rs` asserts the parse still finds them with
//! the range counts the generator recorded. If a future edit renames or
//! reshapes a table, the parse yields nothing and every character falls back
//! to width 1 -- the behaviour before this module existed, so a diagnostic is
//! never lost, only mispadded; the test is what stops that going unnoticed.

use std::sync::OnceLock;

/// Grapheme_Cluster_Break values, in the order `GCB_RANGES` was generated
/// with. Kept in step with the same list in `lib/unicode.src`.
const GCB_OTHER: i64 = 0;
const GCB_CR: i64 = 1;
const GCB_LF: i64 = 2;
const GCB_CONTROL: i64 = 3;
const GCB_EXTEND: i64 = 4;
const GCB_ZWJ: i64 = 5;
const GCB_REGIONAL_INDICATOR: i64 = 6;
const GCB_PREPEND: i64 = 7;
const GCB_SPACINGMARK: i64 = 8;
const GCB_L: i64 = 9;
const GCB_V: i64 = 10;
const GCB_T: i64 = 11;
const GCB_LV: i64 = 12;
const GCB_LVT: i64 = 13;
/// Indic_Conjunct_Break, in bits 5 and 6 of a packed class.
const INCB_LINKER: i64 = 1;
const INCB_CONSONANT: i64 = 2;
const INCB_EXTEND: i64 = 3;

/// U+AC00 and the shape of the Hangul block: 19 leads x 21 vowels x 28
/// trailings. Arithmetic rather than 798 table lines, as in `unicode.src`.
const HANGUL_BASE: i64 = 44032;
const HANGUL_TRAILING: i64 = 28;
const HANGUL_COUNT: i64 = 11172;

/// U+FE0F VARIATION SELECTOR-16, which asks for the emoji form.
const EMOJI_PRESENTATION: i64 = 65039;
const REGIONAL_INDICATOR_FIRST: i64 = 127462;
const REGIONAL_INDICATOR_LAST: i64 = 127487;

/// The two tables, parsed once from the embedded `lib/unicode.src`.
struct Tables {
    /// (lo, hi, width) triples for every code point that is not one column.
    width: Vec<i64>,
    /// (lo, hi, packed class) triples; anything absent is `GCB_OTHER`.
    gcb: Vec<i64>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let src = crate::stdlib::source("unicode").unwrap_or("");
        Tables {
            width: parse_table(src, "WIDTH_RANGES"),
            gcb: parse_table(src, "GCB_RANGES"),
        }
    })
}

/// The integers of `const Array<int> <name> = [ .. ];` in `src`.
///
/// Deliberately literal-minded: the tables are generated output, one shape,
/// and anything else is not a table this can read. An unreadable one yields
/// an empty vector, which reads as "every code point is one column wide".
fn parse_table(src: &str, name: &str) -> Vec<i64> {
    let needle = format!("const Array<int> {name} = [");
    let Some(start) = src.find(&needle) else {
        return Vec::new();
    };
    let rest = &src[start + needle.len()..];
    let Some(end) = rest.find(']') else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for tok in rest[..end].split(',') {
        match tok.trim().parse::<i64>() {
            Ok(n) => out.push(n),
            Err(_) => return Vec::new(),
        }
    }
    if out.len() % 3 != 0 {
        return Vec::new();
    }
    out
}

/// How many (lo, hi, value) ranges each table holds. For the test that keeps
/// this module and `lib/unicode.src` in step.
#[cfg(test)]
pub fn table_sizes() -> (usize, usize) {
    let t = tables();
    (t.width.len() / 3, t.gcb.len() / 3)
}

/// Binary search a (lo, hi, value) table; `dflt` when `c` is in no range.
fn lookup(table: &[i64], c: i64, dflt: i64) -> i64 {
    let mut lo: isize = 0;
    let mut hi: isize = table.len() as isize / 3 - 1;
    while lo <= hi {
        let mid = (lo + hi) / 2;
        let at = (mid * 3) as usize;
        if c < table[at] {
            hi = mid - 1;
        } else if c > table[at + 1] {
            lo = mid + 1;
        } else {
            return table[at + 2];
        }
    }
    dflt
}

/// The packed class of `c`: GCB in the low four bits, Extended_Pictographic
/// in bit 4, Indic_Conjunct_Break in bits 5 and 6.
fn class_of(c: i64) -> i64 {
    if (HANGUL_BASE..HANGUL_BASE + HANGUL_COUNT).contains(&c) {
        return if (c - HANGUL_BASE) % HANGUL_TRAILING == 0 {
            GCB_LV
        } else {
            GCB_LVT
        };
    }
    lookup(&tables().gcb, c, GCB_OTHER)
}

/// The width of one code point: 0, 1 or 2. One is the default and is not in
/// the table.
fn char_width(c: i64) -> i64 {
    lookup(&tables().width, c, 1)
}

/// Is there a cluster boundary between `prev` and `cur`? UAX #29 §3.1.1 in
/// its own order -- the first rule that matches decides. A transcription of
/// `unicode.breaks`; the two must say the same thing.
fn breaks(prev: i64, cur: i64, ri: i64, pic: i64, cons: i64) -> bool {
    let p = prev & 15;
    let c = cur & 15;
    // GB3, GB4, GB5.
    if p == GCB_CR && c == GCB_LF {
        return false;
    }
    if p == GCB_CR || p == GCB_LF || p == GCB_CONTROL {
        return true;
    }
    if c == GCB_CR || c == GCB_LF || c == GCB_CONTROL {
        return true;
    }
    // GB6, GB7, GB8: a Hangul syllable spelled out in jamo.
    if p == GCB_L && (c == GCB_L || c == GCB_V || c == GCB_LV || c == GCB_LVT) {
        return false;
    }
    if (p == GCB_LV || p == GCB_V) && (c == GCB_V || c == GCB_T) {
        return false;
    }
    if (p == GCB_LVT || p == GCB_T) && c == GCB_T {
        return false;
    }
    // GB9, GB9a, GB9b.
    if c == GCB_EXTEND || c == GCB_ZWJ || c == GCB_SPACINGMARK {
        return false;
    }
    if p == GCB_PREPEND {
        return false;
    }
    // GB9c: an Indic conjunct.
    if cons == 2 && (cur >> 5) & 3 == INCB_CONSONANT {
        return false;
    }
    // GB11: emoji ZWJ emoji.
    if pic == 2 && cur & 16 != 0 {
        return false;
    }
    // GB12, GB13: regional indicators pair up into flags.
    if p == GCB_REGIONAL_INDICATOR && c == GCB_REGIONAL_INDICATOR && ri % 2 == 1 {
        return false;
    }
    true
}

/// The width of `s` in terminal columns -- `unicode.width`, in Rust.
///
/// Per grapheme cluster, not per code point: `👍🏽` is two wide code points
/// and one two-column glyph.
pub fn display_width(s: &str) -> usize {
    let cps: Vec<i64> = s.chars().map(|c| c as i64).collect();
    let mut total = 0usize;
    let mut cluster: Vec<i64> = Vec::new();
    let mut prev = 0i64;
    let (mut ri, mut pic, mut cons) = (0i64, 0i64, 0i64);
    for (i, &cp) in cps.iter().enumerate() {
        let cur = class_of(cp);
        if i > 0 && breaks(prev, cur, ri, pic, cons) {
            total += cluster_width(&cluster);
            cluster.clear();
        }
        cluster.push(cp);
        let g = cur & 15;
        ri = if g == GCB_REGIONAL_INDICATOR {
            ri + 1
        } else {
            0
        };
        pic = if cur & 16 != 0 || (pic == 1 && g == GCB_EXTEND) {
            1
        } else if pic == 1 && g == GCB_ZWJ {
            2
        } else {
            0
        };
        let b = (cur >> 5) & 3;
        cons = if b == INCB_CONSONANT {
            1
        } else if cons > 0 && b == INCB_LINKER {
            2
        } else if cons > 0 && b == INCB_EXTEND {
            cons
        } else {
            0
        };
        prev = cur;
    }
    total + cluster_width(&cluster)
}

/// The width of one cluster -- `unicode.cluster_width`.
fn cluster_width(cluster: &[i64]) -> usize {
    let Some(&base) = cluster.first() else {
        return 0;
    };
    let mut ri = 0;
    for &c in cluster {
        if c == EMOJI_PRESENTATION {
            return 2;
        }
        if (REGIONAL_INDICATOR_FIRST..=REGIONAL_INDICATOR_LAST).contains(&c) {
            ri += 1;
        }
    }
    if ri == 2 {
        return 2;
    }
    char_width(base).max(0) as usize
}
