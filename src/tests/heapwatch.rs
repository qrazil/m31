use super::*;

// ---- heap watch: allocation sites (docs/heap-watch-design.md) ----------

fn c_of(src: &str) -> String {
    compile_str(src, "c").expect("expected this program to compile")
}

#[test]
fn each_allocation_site_of_a_user_type_gets_its_own_type_record() {
    let c = c_of("type P { int x; }\nP a = P(1);\nP b = P(2);\nP d = P(3); P e = P(4);");
    // Three lines, so three sites; the two allocations on line 4 share one.
    for n in 1..=3 {
        assert!(
            c.contains(&format!("_s{n} __attribute__((unused)) = ")),
            "no record for site {n}:\n{c}"
        );
    }
    assert!(
        !c.contains("_s4 "),
        "one record per site, not per allocation:\n{c}"
    );
    // A site's record is the type's own, plus its slot and its line.
    let rec = c
        .lines()
        .find(|l| l.contains("_s1 __attribute__"))
        .expect("site 1");
    assert!(
        rec.contains(", 1, ") && rec.ends_with("/*hwsite*/"),
        "{rec}"
    );
    assert!(rec.contains("\"P\""), "the type's name: {rec}");
    // The allocations name their site's record, never the type's own.
    assert!(!c.contains("rt_alloc(sizeof(T0), &ti_T0)"), "{c}");
}

#[test]
fn the_line_table_and_the_start_call_are_emitted() {
    let c = c_of("type P { int x; }\n\nP a = P(1);");
    assert!(c.contains("{ \"main.m31\", 3 }, /*hwsite*/"), "{c}");
    assert!(
        c.contains("rt_hw_program(hw_locs, 2, hw_slots, 1); /*hwsite*/"),
        "{c}"
    );
    let start = c.find("rt_hw_program(").unwrap();
    let run = c.find("rt_run_program(").unwrap();
    assert!(start < run, "heap watch starts before the program:\n{c}");
}

#[test]
fn a_runtime_call_that_may_allocate_names_its_line_first() {
    let c = c_of("str a = \"x\";\n\nstr b = a + \"y\";\nprint(b.size());");
    let at = c.find("rt_hw_at(").expect("a line before concat");
    let call = c.find("rt_concat(").expect("concat");
    assert!(at < call, "{c}");
    // Printing and reading a length make nothing, and say nothing.
    let print_at = c.find("rt_print(").unwrap();
    let before = &c[..print_at];
    let last = before.rfind("    ").unwrap();
    assert!(!before[last..].contains("rt_hw_at"), "{c}");
}

#[test]
fn the_ir_marks_the_line_of_an_allocation() {
    let out = ir("type P { int x; }\nP a = P(1);");
    let site = out.find("site 1").expect("a site marker");
    let alloc = out.find("alloc type").expect("the allocation");
    assert!(site < alloc, "{out}");
}

#[test]
fn a_destructor_sets_the_count_and_nothing_beside_it() {
    let c = c_of("type G { int n; }\nvoid G.drop() { print(n); }\nG g = G(1);");
    assert!(c.contains("rt_rc_set_count(o, 1);"), "{c}");
    assert!(c.contains("if (rt_rc_count(o) != 1) rt_trap("), "{c}");
    assert!(c.contains("rt_rc_set_count(o, 0);"), "{c}");
    assert!(
        !c.contains("o->rc ="),
        "no raw store to the count word:\n{c}"
    );
}

#[test]
fn a_reference_field_is_in_the_type_records_field_table() {
    let c = c_of(
        "type S { str name; int n; }\nenum E { A(str); B; }\n\
         S s = S(\"a\", 1);\nE e = E.A(\"x\");\nprint(s.n);",
    );
    let t = |name: &str| -> String {
        c.lines()
            .find(|l| {
                l.starts_with("static const TypeInfo ti_T")
                    && l.contains(&format!("\"{name}\", hwf_T"))
            })
            .unwrap_or_else(|| panic!("no record for {name}:\n{c}"))
            .to_string()
    };
    let s_rec = t("S");
    let i = s_rec["static const TypeInfo ti_T".len()..]
        .split(' ')
        .next()
        .unwrap();
    assert!(
        c.contains(&format!("hwf_T{i}[] __attribute__((unused)) = {{ {{ \"name\", offsetof(T{i}, f_name), -1 }} }};")),
        "only the reference field, at its offset:\n{c}"
    );
    let e_rec = t("E");
    let j = e_rec["static const TypeInfo ti_T".len()..]
        .split(' ')
        .next()
        .unwrap();
    assert!(
        c.contains(&format!("{{ \"A.0\", offsetof(T{j}, u.v0.p0), 0 }}")),
        "an enum's payload slot names its variant and tag:\n{c}"
    );
}

// ---- the ledger reader ----------------------------------------------

/// A ledger with one site and two shards, laid out as runtime/heapwatch.c
/// lays one out, with the given wheel cells (shard, level, k, count) and
/// lifetime buckets on shard 0.
fn ledger(
    minute: u32,
    shard_minutes: [u32; 2],
    cells: &[(usize, usize, u64, i32)],
    life: &[(usize, u32)],
) -> Vec<u8> {
    const PAGE: usize = 4096;
    let off_slots = 2 * PAGE;
    let off_traced = off_slots + 2 * 32;
    let off_strings = off_traced + 8;
    let off_epochs = off_strings + 64;
    let off_shards = 4 * PAGE;
    let stride = PAGE;
    let mut b = vec![0u8; off_shards + 2 * stride];
    let put32 =
        |b: &mut Vec<u8>, at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    let put64 =
        |b: &mut Vec<u8>, at: usize, v: u64| b[at..at + 8].copy_from_slice(&v.to_le_bytes());
    let h = PAGE;
    b[h..h + 8].copy_from_slice(b"M31HWLDG");
    put32(&mut b, h + 8, 1);
    put32(&mut b, h + 40, minute);
    put32(&mut b, h + 44, 60000);
    put32(&mut b, h + 48, 2); // shards
    put32(&mut b, h + 52, 2); // max slots
    put32(&mut b, h + 56, 2); // slots in use
    put32(&mut b, h + 60, 320);
    put64(&mut b, h + 64, off_slots as u64);
    put64(&mut b, h + 72, off_traced as u64);
    put64(&mut b, h + 80, off_strings as u64);
    put64(&mut b, h + 88, off_epochs as u64);
    put64(&mut b, h + 96, off_shards as u64);
    put64(&mut b, h + 104, stride as u64);
    put32(&mut b, h + 120, 10); // floor
    put32(&mut b, h + 124, 10); // factor
    b[off_strings + 1..off_strings + 6].copy_from_slice(b"Conn\0");
    put32(&mut b, off_slots + 32, 1);
    for (i, m) in shard_minutes.iter().enumerate() {
        put32(&mut b, off_shards + i * stride, *m);
        put32(&mut b, off_shards + i * stride + 4, 1);
    }
    for &(shard, lv, k, v) in cells {
        let at = off_shards + shard * stride + 64 + 320 + (lv * 8 + (k as usize & 7)) * 4;
        let old = i32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        put32(&mut b, at, (old + v) as u32);
    }
    for &(bucket, n) in life {
        put32(&mut b, off_shards + 64 + 320 + 224 + 4 * bucket, n);
    }
    b
}

#[test]
fn an_object_freed_on_another_shard_cancels_out() {
    use crate::tools::heapwatch::Ledger;
    // Born in minute 3 on shard 0, which still has it at level 0; freed on
    // shard 1, whose clock is at minute 20 and so holds minute 3 in the
    // level-1 block for minutes 0..8. Added up, nothing is alive.
    let b = ledger(20, [5, 20], &[(0, 0, 3, 1), (1, 1, 0, -1)], &[(0, 1)]);
    let l = Ledger::parse(&b).expect("a ledger");
    let sum = l.sum(1);
    assert_eq!(sum.live, 0);
    assert_eq!(sum.oldest, None);
}

#[test]
fn a_survivor_far_older_than_its_sites_lifetime_is_counted_as_such() {
    use crate::tools::heapwatch::Ledger;
    // 99 objects lived under a minute; 2 born in minute 1 are still alive at
    // minute 40: older than max(10, 10 x 1 minute).
    let b = ledger(40, [40, 0], &[(0, 1, 0, 2)], &[(0, 99)]);
    let l = Ledger::parse(&b).expect("a ledger");
    let sum = l.sum(1);
    assert_eq!(sum.live, 2);
    assert_eq!(sum.limit, 10);
    assert_eq!(sum.oldest, Some(33), "the block 0..8 ends at minute 7");
    assert_eq!(sum.older_than, 2);
    assert_eq!(l.slot(1).type_name, "Conn");
}

// ---- the store hook, sends and receives (trace and epoch close) ---------

#[test]
fn a_reference_field_store_tells_heap_watch_first() {
    let c = c_of("type S { str name; int n; }\nS s = S(\"a\", 1);\ns.name = \"b\";\ns.n = 2;");
    let hook = c.find("rt_hw_store(").expect("a store hook");
    let store = c[hook..].find("->f_name = ").expect("the store after it");
    assert!(
        c[hook..hook + store].contains("offsetof(T"),
        "the hook names the field by its offset:\n{c}"
    );
    // An int field is not a reference: no hook.
    assert!(
        !c.contains("f_n));"),
        "only reference fields are hooked:\n{c}"
    );
}

#[test]
fn an_element_store_of_a_reference_tells_heap_watch_first() {
    let c = c_of("List<str> xs = [\"a\"];\nxs[0] = \"b\";\nList<int> ns = [1];\nns[0] = 2;");
    assert_eq!(
        c.matches("rt_hw_store_elem(").count(),
        1,
        "the str element store only:\n{c}"
    );
    let hook = c.find("rt_hw_store_elem(").unwrap();
    let set = c.find("rt_index_set(").unwrap();
    assert!(hook < set, "{c}");
}

#[test]
fn a_send_leaves_the_books_after_the_uniqueness_check() {
    let out = ir(
        "type B { int n; }\nChan<B> c = Chan<B>(1);\nsend(c, B(1));\nB b = recv(c);\nprint(b.n);",
    );
    let check = out.find("call rt_check_unique(").expect("the check");
    let leave = out.find("call rt_hw_send(").expect("the send hook");
    let send = out.find("call rt_chan_send(").expect("the send");
    assert!(check < leave && leave < send, "{out}");
    let recv = out.find("call rt_chan_recv(").expect("the recv");
    let arrive = out.find("call rt_hw_receive(").expect("the receive hook");
    assert!(recv < arrive, "{out}");
}

#[test]
fn a_spawned_thread_is_an_epoch_that_takes_its_arguments() {
    let c =
        c_of("type B { int n; }\nvoid work(B b, int k) { print(b.n + k); }\nspawn work(B(1), 2);");
    let tramp = c.find("static void trampoline_").expect("a trampoline");
    let body = &c[tramp..];
    let begin = body
        .find("rt_hw_task_begin(\"work\");")
        .expect("named after the function");
    let take = body
        .find("rt_hw_receive(a->a0);")
        .expect("the reference argument");
    let call = body.find("fn_work(a->a0, a->a1);").expect("the call");
    assert!(begin < take && take < call, "{body}");
    assert!(
        !body[..call].contains("rt_hw_receive(a->a1)"),
        "an int is no object: {body}"
    );
}
