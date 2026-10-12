#[test]
fn the_width_tables_are_the_ones_lib_unicode_carries() {
    // src/width.rs has no tables of its own: it parses WIDTH_RANGES and
    // GCB_RANGES out of the embedded text of lib/text/unicode.m31, so the compiler
    // and the standard library cannot drift apart on what a column is. The
    // counts are the ones the generator recorded in that file's comments; a
    // regeneration for a new Unicode version changes both the numbers here
    // and the prose there, together. A rename or a reshape of either table
    // yields zero, which is what this catches.
    let (width, gcb) = crate::width::table_sizes();
    assert_eq!(width, 494, "WIDTH_RANGES ranges");
    assert_eq!(gcb, 683, "GCB_RANGES ranges");
}

#[test]
fn display_width_measures_clusters_not_code_points() {
    let w = crate::width::display_width;
    assert_eq!(w("abc"), 3);
    // East Asian Wide.
    assert_eq!(w("漢字"), 4);
    // An emoji is wide; a skin-tone modifier joins its cluster and adds none.
    assert_eq!(w("🙂"), 2);
    assert_eq!(w("👍🏽"), 2);
    // A combining mark takes no columns of its own.
    assert_eq!(w("e\u{301}"), 1);
    assert_eq!(w("\u{e9}"), 1);
    // A ZWJ sequence is one cluster and one glyph, measured by its base.
    assert_eq!(w("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"), 2);
    // A flag is a pair of regional indicators, neutral apart, wide together.
    assert_eq!(w("\u{1F1EC}\u{1F1E7}"), 2);
    // U+FE0F asks for the emoji form, which is two columns.
    assert_eq!(w("1\u{FE0F}\u{20E3}"), 2);
    // An arrow is neutral: one column, not two.
    assert_eq!(w("\u{2192}"), 1);
}
