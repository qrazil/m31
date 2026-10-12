use super::*;

// ---- collections, for-in, clone ---------------------------------------

#[test]
fn an_array_allocates_once_and_a_list_has_a_buffer() {
    let a = ir("Array<int> a = [0; 4]; print(a[0]);");
    assert!(a.contains("rt_array_new"), "{a}");
    let l = ir("List<int> l = []; l.push(1); print(l[0]);");
    assert!(l.contains("rt_list_new"), "{l}");
}

#[test]
fn indexing_a_reference_element_is_borrowed() {
    // Reading an element does not retain: the collection holds the +1, the
    // same rule as reading a field.
    let out = ir("Array<str> a = [\"\"; 2]; print(a[0]);");
    let main = out.split("func $main").nth(1).unwrap();
    // One retain for the fill argument, and no extra for the read.
    assert_eq!(
        main.matches("rc_inc").count(),
        0,
        "the read must add no refcount traffic:\n{main}"
    );
}

#[test]
fn assigning_an_element_releases_the_old_one() {
    let out = ir("Array<str> a = [\"x\"; 2]; a[0] = \"y\"; print(a[0]);");
    let main = out.split("func $main").nth(1).unwrap();
    assert!(
        main.contains("rt_index_get") && main.contains("rt_index_set"),
        "old value must be read before the store:\n{main}"
    );
    assert!(
        main.contains("rc_dec"),
        "the old element must be released:\n{main}"
    );
}

#[test]
fn for_in_increments_before_the_body() {
    // This is what makes `continue` advance the loop. An increment at the
    // bottom of the body would be skipped by it and the loop would hang.
    let out = ir("List<int> xs = []; xs.push(1); for (int x in xs) { print(x); }");
    let body = out
        .split("block2:")
        .nth(1)
        .expect("expected a loop body block");
    let add = body.find("iadd").expect("the index must be incremented");
    let get = body.find("rt_index_get").expect("the element must be read");
    assert!(
        add < get,
        "the increment must come before the element read:\n{body}"
    );
}

#[test]
fn for_in_reads_the_length_once() {
    let out = ir("List<int> xs = []; for (int x in xs) { print(x); }");
    assert_eq!(
        out.matches("rt_len_of").count(),
        1,
        "the length is read once, before the loop:\n{out}"
    );
}

#[test]
fn clone_copies_a_struct_field_by_field() {
    let out = ir("type P { int x; str s; }\nP a = P(1, \"t\"); P b = clone(a); print(b.x);");
    let main = out.split("func $main").nth(1).unwrap();
    assert!(main.contains("alloc"), "clone must allocate:\n{main}");
    assert!(
        main.matches("store").count() >= 4,
        "both fields must be copied into the new object:\n{main}"
    );
}

#[test]
fn collection_misuse_is_rejected() {
    assert!(err("Array<int> a = Array<int>(4); print(a[0]);").contains("as a literal"));
    assert!(err("Array<int> a = [0; 2]; a.push(1);").contains("needs a List"));
    assert!(err("int x = 1; print(x[0]);").contains("cannot be indexed"));
    assert!(err("List<int> xs = []; xs.push(\"no\");").contains("expected int"));
    assert!(err("int x = 1; print(clone(x));").contains("nothing to clone"));
    assert!(err("List<int> xs = []; for (str s in xs) { print(s); }")
        .contains("expected str, found int"));
}

// ---- bytes -------------------------------------------------------------

#[test]
fn bytes_is_a_type_keyword() {
    assert_eq!(
        toks("bytes b"),
        vec![Tok::KwBytes, Tok::Ident("b".into()), Tok::Eof]
    );
}

#[test]
fn a_byte_is_a_value_and_moves_no_refcount() {
    // Reading, writing and iterating a bytes touch no count: a byte is an
    // int, not a reference, so the only retain is the loop's hold on the
    // collection itself.
    let out = ir("bytes b = [1, 2]; b[0] = b[1]; for (int x in b) { print(x); }");
    let main = out.split("func $main").nth(1).unwrap();
    assert!(
        main.contains("rt_bytes_get") && main.contains("rt_bytes_set"),
        "{main}"
    );
    assert_eq!(
        main.matches("rc_inc").count(),
        1,
        "only the loop's hold on the collection:\n{main}"
    );
}

#[test]
fn a_bytes_literal_builds_through_push() {
    // Sized once for what is written, then filled: each push checks its
    // value, so a computed element traps exactly as `b.push(v)` would.
    let out = ir("int k = 3; bytes b = [1, k]; print(b.size());");
    assert!(out.contains("rt_bytes_new"), "{out}");
    assert_eq!(out.matches("rt_bytes_push").count(), 2, "{out}");
}
