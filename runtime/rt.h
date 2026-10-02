/* Runtime interface.
 *
 * Included by emitted code. Implementations live in rt.c and are compiled as
 * a SEPARATE translation unit -- see docs/ir-v0.md §7.1. That separation is
 * load-bearing, not stylistic: it is what keeps the C compiler from seeing a
 * free() whose argument is a static string literal object.
 *
 * The exception is checked arithmetic, which is inline here on purpose.
 * Arithmetic is the hottest thing in the language and a call per add would be
 * indefensible; the separate-TU rule exists only to hide static objects from
 * free(), which does not apply to integers.
 */
#ifndef RT_H
#define RT_H

#include <limits.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define RC_IMMORTAL (-1)

/* A frozen object: bound to a `const`, so nothing may change it again
 * (docs/const-decision.md). The flag is the highest bit of the count below
 * the sign, rather than a field of its own, so the header stays two words:
 * the count uses the bits below it, a retain or release carries on counting
 * underneath, and only the zero test in rc_dec has to look past it.
 *
 * RC_IMMORTAL is -1, every bit set, so an immortal object reads as frozen
 * too -- which is right for a module constant's static data, and harmless
 * for the other immortals, string literals (immutable anyway) and channels
 * (never passed to a mutation path). */
#define RC_FROZEN (LONG_MAX / 2 + 1)

/* The runtime is in the middle of an operation on this collection that calls
 * back into the PROGRAM -- a `cmp` while sorting it, a `hash` or an `eq`
 * while probing it -- and is holding something the operation would
 * invalidate: a pointer to the element buffer, or a slot index. Changing the
 * collection from inside that callback is refused -- it is a bug in the
 * program, so it traps (docs/reentrancy-decision.md, reference §3.9).
 *
 * Two bits and not one so the trap can name the operation without a
 * thread-local to hold the reason, which nested operations would get wrong.
 * They sit directly below RC_FROZEN, so the count still has 60 bits and
 * every check that wants the count alone masks RC_FLAGS off.
 *
 * An immortal object is never marked: RC_IMMORTAL is every bit set, so the
 * bits cannot be cleared again afterwards, and it does not need them -- it
 * reads as frozen, so every mutation path already refuses it. */
#define RC_SORTING (LONG_MAX / 4 + 1)
#define RC_PROBING (LONG_MAX / 8 + 1)
#define RC_BUSY    (RC_SORTING | RC_PROBING)

/* Everything in the header word that is not the count. */
#define RC_FLAGS (RC_FROZEN | RC_BUSY)

typedef struct Obj Obj;

/* Called when a refcount reaches zero, BEFORE the block is freed, to release
 * whatever references the object holds. NULL means "holds none, just free" --
 * which is the common case and worth not paying a call for. Emitted code
 * generates one of these per user type that has reference-typed fields. */
typedef void (*DropFn)(Obj *);

/* Enumerate the references an object holds, for the transitive uniqueness
 * check at a thread boundary (rt_check_unique).
 *
 * A separate function from DropFn because the two differ: drop releases and
 * is allowed to free, walk only reports. NULL means the type holds no
 * references -- a string, or a struct of ints. Emitted code generates one per
 * user type that has reference-typed fields. */
typedef void (*VisitFn)(void *ctx, Obj *child);
typedef void (*WalkFn)(Obj *o, VisitFn visit, void *ctx);

/* A vtable slot. Every entry is cast to its real signature at the call site,
 * which the compiler knows statically; only the indirection is dynamic. */
typedef void (*AnyFn)(void);

/* Per-type metadata, emitted once per concrete type.
 *
 * `vtable` is a POINTER rather than an inline array so that TypeInfo has a
 * fixed size here even though the slot count is decided per program. Slot
 * numbers are assigned by the compiler: one per distinct interface method
 * name in the program, NULL where a type does not have it.
 */
/* Deep-copy one object for a `const` snapshot (rt_snapshot): allocate the
 * copy, register it with rt_copy_register BEFORE copying any child -- which
 * is what lets a cycle close back onto the copy instead of recursing
 * forever -- then fill each reference field with rt_copy_child. Emitted
 * code generates one per user type; the runtime copies its own collections
 * itself, so theirs is NULL. `ctx` is the runtime's, opaque here. */
typedef Obj *(*CopyFn)(Obj *o, void *ctx);

/* The three methods the RUNTIME itself calls on a user type, found by
 * reserved name the way `drop` above already is.
 *
 * `sort` needs an ordering and a Map keyed on a user type needs a hash and
 * an equality, and in all three cases the runtime is holding the object: it
 * has the header, so it has the TypeInfo, so a pointer stored here is
 * everything it is missing. They live in the TypeInfo rather than at fixed
 * indices at the head of the vtable (the shape docs/closures-decision.md
 * sketched) for two reasons: a field has a real prototype, so the C compiler
 * checks the signature at every call instead of a hard-coded slot number
 * having to agree between src/lower.rs and this file; and a program with no
 * interfaces at all keeps its empty vtable.
 *
 * NULL when the type declares no such method. The compiler refuses any
 * program that would need one it has not got, so a NULL reaching a call here
 * is a compiler bug -- the runtime traps rather than jumping through it. */
typedef int64_t (*CmpFn)(Obj *a, Obj *b);  /* int  T.cmp(T other)  */
typedef int64_t (*HashFn)(Obj *o);         /* int  T.hash()        */
typedef bool (*EqFn)(Obj *a, Obj *b);      /* bool T.eq(T other)   */

typedef struct TypeInfo {
    DropFn       drop;
    const AnyFn *vtable;
    WalkFn       walk;
    CopyFn       copy;
    /* The type's name as the program spells it, if the type declares a
     * destructor -- it owns a resource -- and NULL otherwise. A value that
     * owns a resource can be neither frozen nor copied, so rt_snapshot traps
     * on one, naming it; the compiler refuses the cases it can see
     * (docs/destructors-decision.md, "A resource cannot be copied"). The
     * runtime's own types never own one. */
    const char  *resource;
    /* The reserved-name methods, or NULL. See the typedefs above. */
    CmpFn        cmp;
    HashFn       hash;
    EqFn         eq;
} TypeInfo;

/* Every heap object starts with this. Two words: the count, and a pointer to
 * the type.
 *
 * Putting the type here rather than in a fat interface pointer is what keeps
 * `ref` the only reference shape in the IR -- an interface value is just an
 * `Obj *`, because the object knows what it is. Java's model. It costs one
 * extra load on dispatch against Go's fat pointer, using a word that was
 * already being spent on the drop function. */
struct Obj {
    long            rc;
    const TypeInfo *ty;
};

/* Strings carry no methods and need no drop. */
extern const TypeInfo rt_str_type;

/* A string is allocated as one block -- header, then bytes -- with `data`
 * pointing just past the struct. One malloc, one free, good locality. A
 * literal is static storage with RC_IMMORTAL and `data` pointing at a C
 * string constant, so it is never freed and needs no drop. */
typedef struct {
    Obj         hdr;
    int64_t     len;
    const char *data;
} Str;

void rc_inc(Obj *o);
void rc_dec(Obj *o);

/* The queue `rc_dec` releases a graph with, saved across a destructor's body
 * (runtime/rt.c, "a destructor's body is top-level code"). Emitted code
 * declares one beside the destructor call and passes it to the two functions
 * below; no field of it means anything outside rt.c. */
typedef struct {
    Obj  **q;
    size_t cap;
    size_t head;
    size_t tail;
    bool   releasing;
} RcDrain;

/* Around the call to a user `drop` body, and nothing else. Between them the
 * program's own code runs, and it must behave exactly as it does at the top
 * level: what it drops is released there and then, not queued until the walk
 * that is releasing this object finishes. */
void rt_drop_enter(RcDrain *save);
void rt_drop_leave(RcDrain *save);

/* A `const` binding takes a frozen snapshot of its value
 * (docs/const-decision.md). Takes the caller's +1 on `o` and returns a +1 on
 * a frozen value equal to it:
 *   - already frozen (or immortal, or a str): `o` itself;
 *   - reachable from nothing but `o` -- a literal, a fresh construction, a
 *     call's result nobody else holds: `o`, frozen in place, no copy;
 *   - otherwise: a deep copy of `o`, frozen, and `o` released. The original
 *     stays mutable and later changes to it do not reach the snapshot.
 * Frozen and immortal parts, and every str, are shared rather than copied:
 * they cannot change. */
Obj *rt_snapshot(Obj *o);

/* For emitted CopyFns only -- see CopyFn above. rt_copy_child returns a +1
 * on the child's copy (or on the child itself, where it is shared). */
void rt_copy_register(void *ctx, Obj *old, Obj *copy);
Obj *rt_copy_child(void *ctx, Obj *child);

/* Cold as well as _Noreturn: without it gcc counted the call against the
 * size of every function that stores a field, and stopped inlining small
 * methods -- a 70% slowdown on a store-heavy loop, measured, that was the
 * lost inlining and not the check. Takes the header word and picks the
 * message from it: frozen, being sorted, being probed. */
__attribute__((cold)) _Noreturn void rt_frozen_trap(long rc);

/* Every path that changes an object checks this first: the runtime's own
 * collection and bytes mutators, and the compiler before every field store
 * that is not initialising a new object. It is what catches a change the
 * compiler cannot see -- a frozen value reached through a parameter, since
 * there is no read-only parameter type -- and a change made from inside a
 * `cmp`, `hash` or `eq` the runtime is in the middle of calling (RC_BUSY).
 *
 * One test covers both, because both are bits of the same word: the guard
 * costs the reentrancy check nothing over the frozen check that was already
 * here.
 *
 * Inline, because a field store is one instruction and a call per store
 * would dominate it; the trap itself is out of line and cold. */
static inline void rt_check_mutable(Obj *o) {
    if (__builtin_expect((o->rc & RC_FLAGS) != 0, 0)) rt_frozen_trap(o->rc);
}

/* A `const` BLOCK's runtime half (docs/const-decision.md, "`const` is also a
 * block"): freeze `o` for a lexical region rather than for good. The
 * mechanism is exactly the one docs/reentrancy-decision.md already uses for
 * RC_SORTING/RC_PROBING -- mark, trap on reentrant mutation through
 * rt_check_mutable and rt_frozen_trap above, restore -- applied to RC_FROZEN
 * instead.
 *
 * rt_freeze_enter sets the flag and reports whether IT did: false when `o`
 * arrived already frozen (a module constant, or an enclosing `const` block),
 * including immortal. The compiler emits one call per named value on entry
 * to the block and passes what it returned straight to rt_freeze_leave on
 * every path out -- normal fall-through, `return`, `break`, `continue`, and
 * `?` propagation -- which clears the flag only if this call was the one
 * that set it. That is "restore, don't unconditionally clear": freezing a
 * value that was already const is then a no-op rather than a temporary hole
 * in its guarantee. Nothing calls this pair for a trap: nothing runs after
 * one, so nothing depends on the flag. */
bool rt_freeze_enter(Obj *o);
void rt_freeze_leave(Obj *o, bool took);

/* Allocate `size` bytes of object, refcount 1, with the given drop function
 * (NULL if the type holds no references). The header is initialised; the
 * caller fills in the rest. */
Obj *rt_alloc(size_t size, const TypeInfo *ty);

/* Allocate an object that is never freed. It is NOT counted by the refcount
 * invariant, because an object that can never be released is not a leak. */
Obj *rt_alloc_immortal(size_t size, const TypeInfo *ty);

int64_t rt_len(Obj *o);
Obj    *rt_concat(Obj *a, Obj *b);
bool    rt_str_eq(Obj *a, Obj *b);

void rt_print(int64_t v);
void rt_print_bool(bool v);
void rt_print_str(Obj *o);

/* The seam for `lib/io.m31` and `lib/fs.m31`: one sys-layer call each
 * (runtime/sys.h), returning its value or -errno unchanged. A primitive
 * returns only what the runtime can build without the compiler's help: a
 * scalar, a str, an element pushed onto a collection the caller passed in,
 * or bytes written in place inside a range of a `bytes` the caller passed
 * in, checked here. It never builds an `Option` or a `Result` -- their
 * layout and TypeInfo belong to the compiler -- so the library builds its
 * own error type from the errno, in source where the mapping can be read. */
int64_t rt_open(Obj *path, int64_t flags, int64_t mode);         /* fd */
int64_t rt_read(int64_t fd, Obj *buf, int64_t off, int64_t n);   /* bytes read, 0 at end */
int64_t rt_write(int64_t fd, Obj *buf, int64_t off, int64_t n);  /* bytes written */
int64_t rt_write_str(int64_t fd, Obj *s, int64_t off, int64_t n);
int64_t rt_close(int64_t fd);
int64_t rt_seek(int64_t fd, int64_t off, int64_t whence);        /* new offset */
int64_t rt_fstat(int64_t fd, Obj *out);           /* pushes size, mode, mtime_ns */
int64_t rt_stat(Obj *path, bool follow, Obj *out);/* the same, by name */
int64_t rt_mkdir(Obj *path, int64_t mode);
int64_t rt_unlink(Obj *path);
int64_t rt_rmdir(Obj *path);
int64_t rt_rename(Obj *from, Obj *to);
int64_t rt_symlink(Obj *target, Obj *path);
int64_t rt_listdir(Obj *path, Obj *buf);          /* bytes the listing needs */
/* Writes out what `print` has buffered. lib/io.m31 calls it before writing
 * to descriptor 1 or 2 itself, so the two paths to one stream land in the
 * order the program wrote them. */
void    rt_out_flush(void);

/* ---- process primitives: lib/os.m31, lib/date.m31, lib/random.m31 ------ */
/* Raw facts about the process and nothing more: the command line, one
 * environment variable, the exit status, the wall clock, the kernel's
 * randomness. Everything built on them -- Option, Result, calendars,
 * rejection sampling -- is language source. */
void    rt_args_init(int argc, char **argv);   /* called by the emitted main */
void    rt_args(Obj *out);                      /* pushes every argv[i] as bytes, argv[0] first */
int64_t rt_env(Obj *name, Obj *out);            /* 1 pushed the value as bytes, 0 unset */
void    rt_env_map(Obj *out);                   /* pushes name, value, name, value ... */
_Noreturn void rt_exit(int64_t code);           /* flushes stdout, then exit(code) */
int64_t rt_proc_start(Obj *argv);               /* pid, or -errno; argv is a List<str> */
int64_t rt_proc_wait(int64_t pid);              /* sys.h's encoded status, or -errno */
_Noreturn void rt_panic(Obj *msg);              /* rt_trap with a str message */
void    rt_clock(Obj *out);                     /* pushes seconds, then nanoseconds */
int64_t rt_entropy(int64_t n, Obj *out);        /* pushes n octets; 0, or errno */
/* ---- end process primitives ------------------------------------------- */

/* ---- net primitives: lib/net.m31 --------------------------------------- */
/* The socket seam, to the list docs/sys-layer.md §9 designed. One sys-layer
 * call each, its value or -errno passed through, and nothing decided here:
 * lib/net.m31 holds the address parser and printer, the retry loops,
 * SO_REUSEADDR's default and the errno mapping.
 *
 * An IP address crosses as (family, port, 16 bytes) rather than as a struct,
 * because a prim deals only in scalars, a str, and what it pushes onto or
 * writes into a collection it was handed. The 16 bytes are in WIRE order --
 * 127.0.0.1 is {127,0,0,1} -- and the port is a plain integer that the sys
 * layer byte-swaps, so nothing above this line ever calls htons.
 *
 * Reading, writing and closing a socket are missing on purpose: a socket is
 * a descriptor, so rt_read, rt_write and rt_close above already work on it. */
int64_t rt_socket(int64_t domain, int64_t type, int64_t protocol);   /* fd */
int64_t rt_listen(int64_t fd, int64_t backlog);
int64_t rt_shutdown(int64_t fd, int64_t how);
int64_t rt_bind(int64_t fd, int64_t family, int64_t port, Obj *addr);
int64_t rt_connect(int64_t fd, int64_t family, int64_t port, Obj *addr);
int64_t rt_bind_path(int64_t fd, Obj *path);      /* AF_UNIX, which takes a path */
int64_t rt_connect_path(int64_t fd, Obj *path);
int64_t rt_accept(int64_t fd, Obj *out, Obj *addr);  /* fd; pushes the peer */
int64_t rt_sockname(int64_t fd, int64_t peer, Obj *out, Obj *addr);
Obj    *rt_sockpath(int64_t fd, int64_t peer);    /* AF_UNIX path, "" if none */
int64_t rt_setsockopt(int64_t fd, int64_t opt, int64_t value);
int64_t rt_getsockopt(int64_t fd, int64_t opt);   /* the value, or -errno */
int64_t rt_poll(Obj *fds, Obj *events, Obj *revents, int64_t timeout_ms);
int64_t rt_resolve(Obj *host, int64_t port, int64_t family, Obj *out, Obj *addrs);
int64_t rt_ignore_sigpipe(void);  /* so a write to a dead peer is EPIPE, not death */
/* Parks the calling green thread until `fd` is ready for `events`
 * (runtime/reactor.h's RT_REACTOR_READ/WRITE), instead of blocking the
 * carrier on the syscall itself -- see rt.c's own comment on this function. */
int64_t rt_wait_io(int64_t fd, int64_t events);
/* ---- end net primitives ------------------------------------------------ */

/* ---- terminal primitives: lib/term.m31 --------------------------------- */
/* One sys-layer call each, the layer's value or -errno passed through. The
 * flag words are the layer's own constants (runtime/sys.h), so the numbers
 * lib/term.m31 writes mean the same thing on every target.
 *
 * Reading keys and writing escape sequences need nothing here: a terminal is
 * a descriptor, so rt_read, rt_write_str and rt_poll above already do it. */
int64_t rt_isatty(int64_t fd);                   /* 1 or 0, never fails */
int64_t rt_tcget(int64_t fd, Obj *out);          /* pushes iflag, oflag, cflag, lflag, vmin, vtime */
int64_t rt_tcset(int64_t fd, int64_t iflag, int64_t oflag, int64_t cflag, int64_t lflag,
                 int64_t vmin, int64_t vtime);
int64_t rt_winsize(int64_t fd, Obj *out);        /* pushes rows, then columns */

/* The safety net a destructor cannot be: the settings to put back if the
 * process ends without running one. rt_term_arm takes a snapshot of the
 * terminal as it is now; rt_term_disarm forgets it; rt_term_restore puts it
 * back and is called from the trap path and from the exit handler, never by
 * the program. See the section in rt.c for what this does and does not buy. */
int64_t rt_term_arm(int64_t fd);
int64_t rt_term_disarm(void);
void    rt_term_restore(void);
/* ---- end terminal primitives ------------------------------------------- */


/* Text primitives a library cannot write from inside the language. */
int64_t rt_str_byte_at(Obj *o, int64_t i);
bool    rt_str_parse_int(Obj *o, int64_t *out);
Obj    *rt_int_to_str(int64_t n);
Obj    *rt_bool_to_str(bool b);

/* int <-> float, both explicit in the source. */
double  rt_i2f_val(int64_t n);
int64_t rt_f2i_checked(double x);

/* A generic slot -- a collection's element, an enum's payload -- is a machine
 * word, and a double does not fit one by CONVERSION, only by bit pattern.
 * memcpy is the portable spelling of that, and both compilers turn it into a
 * register move. */
static inline int64_t rt_f2i(double x) {
    int64_t n;
    __builtin_memcpy(&n, &x, sizeof n);
    return n;
}

static inline double rt_i2f(int64_t n) {
    double x;
    __builtin_memcpy(&x, &n, sizeof x);
    return x;
}

/* ---- collections -------------------------------------------------------
 *
 * Two shapes, and the difference is layout rather than stack-versus-heap:
 *
 *   Array<T>  fixed length, elements stored INLINE right after the header.
 *             One allocation, one pointer chase, no capacity slack, and it
 *             can never reallocate. This is the fast one.
 *
 *   List<T>   growable, elements in a separate buffer that push may
 *             reallocate. Two pointer chases. This is the convenient one.
 *
 * A slot is 64 bits either way. The compiler knows the element type
 * statically, so an int rides in the slot and a reference rides as its
 * pointer -- no tagging, exactly as for channels.
 *
 * Whether the elements are references is baked into the TypeInfo at
 * construction, because only the drop function needs to know.
 */
typedef struct {
    Obj     hdr;
    int64_t len;
    int64_t data[];   /* inline: one allocation for header and elements */
} Arr;

typedef struct {
    Obj      hdr;
    int64_t  len;
    int64_t  cap;
    int64_t *data;    /* separate buffer, so it can grow */
} Lst;

/* An Array's and a List's two TypeInfos each -- elements that are values,
 * elements that are references. Exported only because a module constant is emitted as
 * static data and has to carry the same TypeInfo the runtime would have
 * given it: the runtime tells a list from an array, and a reference from a
 * value, by which of these the header points at. */
extern const TypeInfo rt_arr_val_type;
extern const TypeInfo rt_arr_ref_type;
extern const TypeInfo rt_lst_val_type;
extern const TypeInfo rt_lst_ref_type;

/* `fill` is the initial value of every element. There is no null in the
 * language, so an array cannot start with empty slots: the caller must say
 * what an unset element is. For references the fill is retained once per
 * element. */
Obj *rt_array_new(int64_t len, int64_t fill, bool elems_are_refs);
Obj *rt_list_new(bool elems_are_refs);
Obj *rt_list_repeat(int64_t n, int64_t fill, bool elems_are_refs);
Obj *rt_array_blank(int64_t len, bool elems_are_refs);
void rt_array_put(Obj *o, int64_t i, int64_t v);

/* Forward declaration: the full comment is with the checked arithmetic
 * further down, which traps the same way. Needed here, attribute and all,
 * because `a[i]` traps above that point in the file. */
__attribute__((cold)) _Noreturn void rt_trap(const char *msg);

/* `a[i]` read, write, and `.size()` -- docs/perf-board.md item 1, measured in
 * apps/git/FRICTION.md §5: `static inline`, like the checked arithmetic
 * further down and for the same reason. This header is included by the
 * emitted C, so these are in the SAME translation unit as the caller, and
 * gcc/clang can inline the bounds check and the pointer arithmetic away
 * entirely -- calling out to rt.c instead, as this used to, is what the
 * no-LTO gate (file comment, top) forbids inlining. Inlining the accessor is
 * most of item 1's win by itself; hoisting the length and the pointer THEMSELVES
 * across a loop turned out to need help beyond inlining -- gcc/clang do not
 * do it on their own even then, measured (see rt_index_get's comment,
 * below) -- so src/hoist.rs does it explicitly, in the IR, after lowering.
 *
 * The trap stays out of line and cold (rt_trap, below), so the path where
 * nothing traps -- the only one that runs in a tight loop -- is a handful of
 * straight-line instructions with no call in it.
 *
 * Array and List share a layout up to `len` (see the comment above), so one
 * pair of accessors serves both; the compiler that emits the call already
 * knows which it has, but the runtime does not need to -- it only needs to
 * find the elements. */
static inline int64_t rt_len_of(Obj *o) {
    bool is_list = o->ty == &rt_lst_val_type || o->ty == &rt_lst_ref_type;
    return is_list ? ((Lst *)o)->len : ((Arr *)o)->len;
}

static inline int64_t rt_index_get(Obj *o, int64_t i) {
    bool is_list = o->ty == &rt_lst_val_type || o->ty == &rt_lst_ref_type;
    int64_t n = is_list ? ((Lst *)o)->len : ((Arr *)o)->len;
    if (i < 0 || i >= n) rt_trap("index out of range");
    /* `restrict`: an element is never stored anywhere the header's own
     * fields live, so a write through this pointer is never the write that
     * changes `len` or `ty`. True, and worth saying, but NOT enough on its
     * own to make gcc/clang hoist `n` and this pointer across a loop that
     * also calls rt_index_set on the same object: measured present at both
     * -O2 and -O3 (which adds loop unswitching), gcc and clang, with
     * `restrict` in place, reduced to a header-plus-inline-array C
     * repro with no runtime involved at all. `int64_t` is `int64_t`, and
     * that is apparently enough doubt for the alias oracle even when the
     * regions provably do not overlap. src/hoist.rs is what actually
     * hoists these two across such a loop, in the IR, where the object's
     * SSA identity makes the invariance exact rather than inferred -- see
     * its file comment for the measurement (~13%, on top of inlining this
     * accessor in the first place). */
    int64_t *restrict data = is_list ? ((Lst *)o)->data : ((Arr *)o)->data;
    return data[i];
}

static inline void rt_index_set(Obj *o, int64_t i, int64_t v) {
    rt_check_mutable(o);
    bool is_list = o->ty == &rt_lst_val_type || o->ty == &rt_lst_ref_type;
    int64_t n = is_list ? ((Lst *)o)->len : ((Arr *)o)->len;
    if (i < 0 || i >= n) rt_trap("index out of range");
    int64_t *restrict data = is_list ? ((Lst *)o)->data : ((Arr *)o)->data;
    data[i] = v;
}

/* The other half of "hoist the metadata" (src/hoist.rs): a fixed pointer and
 * length, read ONCE outside a loop the pass proved safe, rather than
 * re-derived by rt_index_get/rt_index_set on every element. `ptr` is really
 * an `int64_t *`, carried as `int64_t` for the same reason a collection's own
 * slots are (docs/ir-v0.md): the IR has three scalar shapes and a raw
 * pointer is not one of them, and punning it through the shape it already
 * has for a machine word costs nothing and adds no fourth one.
 *
 * `rt_index_set_at` still takes `o`: the frozen check is NOT part of what
 * this pass hoists (a `const` block can freeze the object partway through
 * the loop, docs/const-decision.md's check-hoisting barrier), so it stays a
 * per-store check exactly as it is in rt_index_set. */
static inline int64_t rt_seq_data_addr(Obj *o) {
    bool is_list = o->ty == &rt_lst_val_type || o->ty == &rt_lst_ref_type;
    int64_t *data = is_list ? ((Lst *)o)->data : ((Arr *)o)->data;
    return (int64_t)(intptr_t)data;
}

static inline int64_t rt_index_get_at(int64_t ptr, int64_t len, int64_t i) {
    if (i < 0 || i >= len) rt_trap("index out of range");
    return ((int64_t *)(intptr_t)ptr)[i];
}

static inline void rt_index_set_at(Obj *o, int64_t ptr, int64_t len, int64_t i, int64_t v) {
    rt_check_mutable(o);
    if (i < 0 || i >= len) rt_trap("index out of range");
    ((int64_t *)(intptr_t)ptr)[i] = v;
}

void    rt_list_push(Obj *o, int64_t v);
Obj    *rt_seq_clone(Obj *o);         /* shallow copy of an array or list */
Obj    *rt_seq_slice(Obj *o, int64_t from, int64_t to);  /* the same, of a range */
Obj    *rt_str_clone(Obj *o);         /* a str copy with its own refcount */
Obj    *rt_str_substr(Obj *o, int64_t from, int64_t to);
int64_t rt_str_find(Obj *o, Obj *needle);
bool    rt_str_starts_with(Obj *o, Obj *p);
bool    rt_str_ends_with(Obj *o, Obj *p);
Obj    *rt_str_trim(Obj *o);
Obj    *rt_str_case(Obj *o, bool upper);
Obj    *rt_str_repeat(Obj *o, int64_t n);
Obj    *rt_str_split(Obj *o, Obj *sep);
Obj    *rt_str_join(Obj *parts, Obj *sep);

/* ---- bytes -------------------------------------------------------------
 *
 * A mutable, growable run of octets: the buffer a read fills and a codec
 * works in. Shaped like a List -- a separate buffer that push may grow --
 * but each element is ONE byte, not a 64-bit slot, so a 4 KiB buffer is
 * 4 KiB and hands straight to read(2) or memcpy.
 *
 * A byte crosses into the language as an int from 0 to 255. Storing one
 * outside that range traps rather than truncating: 256 silently becoming 0
 * is the kind of wrong answer a codec never recovers from.
 *
 * It holds no references, so its TypeInfo has a drop (the buffer) and no
 * walk: at a thread boundary it is a leaf, unique when its count is 1. */
typedef struct {
    Obj      hdr;
    int64_t  len;
    int64_t  cap;
    uint8_t *data;    /* separate buffer, so it can grow; never NULL */
} Bytes;

/* Exported for a module constant's bytes, as rt_arr_val_type is. */
extern const TypeInfo rt_bytes_type;

Obj    *rt_bytes_new(int64_t cap);              /* empty, room for `cap` */
Obj    *rt_bytes_fill(int64_t n, int64_t v);    /* `[v; n]` */

/* Inline for the same reason as rt_index_get/rt_index_set above -- SHA-1's
 * message schedule is exactly this accessor, 480 times per 64 octets
 * hashed (apps/git/FRICTION.md §5). */
static inline int64_t rt_bytes_len(Obj *o) {
    return ((Bytes *)o)->len;
}

static inline int64_t rt_bytes_get(Obj *o, int64_t i) {
    Bytes *b = (Bytes *)o;
    int64_t n = b->len;
    if (i < 0 || i >= n) rt_trap("index out of range");
    /* `restrict`, for the reason given on rt_index_get above: `uint8_t` gets
     * the "may alias anything" exception in the standard, which is the
     * opposite of what hoisting needs -- without this, a write through
     * ANOTHER bytes' data could look, to gcc, like it might be the write
     * that changes THIS bytes' `len`. */
    uint8_t *restrict data = b->data;
    return data[i];
}

/* The index is checked before the value, the order a reader sees them in
 * `b[i] = v`. */
static inline void rt_bytes_set(Obj *o, int64_t i, int64_t v) {
    rt_check_mutable(o);
    Bytes *b = (Bytes *)o;
    int64_t n = b->len;
    if (i < 0 || i >= n) rt_trap("index out of range");
    if (v < 0 || v > 255) rt_trap("byte value out of range 0..255");
    uint8_t *restrict data = b->data;
    data[i] = (uint8_t)v;
}

/* The `bytes` half of src/hoist.rs's "hoist the metadata" -- see
 * rt_seq_data_addr/rt_index_get_at/rt_index_set_at above, same idea, one
 * octet at a time instead of one word. */
static inline int64_t rt_bytes_data_addr(Obj *o) {
    return (int64_t)(intptr_t)((Bytes *)o)->data;
}

static inline int64_t rt_bytes_get_at(int64_t ptr, int64_t len, int64_t i) {
    if (i < 0 || i >= len) rt_trap("index out of range");
    return ((uint8_t *)(intptr_t)ptr)[i];
}

static inline void rt_bytes_set_at(Obj *o, int64_t ptr, int64_t len, int64_t i, int64_t v) {
    rt_check_mutable(o);
    if (i < 0 || i >= len) rt_trap("index out of range");
    if (v < 0 || v > 255) rt_trap("byte value out of range 0..255");
    ((uint8_t *)(intptr_t)ptr)[i] = (uint8_t)v;
}

void    rt_bytes_push(Obj *o, int64_t v);
int64_t rt_bytes_pop(Obj *o);
void    rt_bytes_clear(Obj *o);
void    rt_bytes_truncate(Obj *o, int64_t n);    /* keep the first `n` */
void    rt_bytes_drop_front(Obj *o, int64_t n);  /* remove the first `n` */
void    rt_bytes_extend(Obj *o, Obj *more);
bool    rt_bytes_eq(Obj *a, Obj *b);
Obj    *rt_bytes_clone(Obj *o);
Obj    *rt_bytes_substr(Obj *o, int64_t from, int64_t to);
int64_t rt_bytes_find(Obj *o, Obj *needle);
bool    rt_bytes_starts_with(Obj *o, Obj *p);
bool    rt_bytes_ends_with(Obj *o, Obj *p);
Obj    *rt_bytes_trim(Obj *o);
Obj    *rt_bytes_case(Obj *o, bool upper);
Obj    *rt_bytes_repeat(Obj *o, int64_t n);
Obj    *rt_bytes_split(Obj *o, Obj *sep);
Obj    *rt_bytes_join(Obj *parts, Obj *sep);
Obj    *rt_bytes_hex(Obj *o);                    /* a str */
bool    rt_bytes_utf8(Obj *o, Obj **out);        /* a str, if valid UTF-8 */
Obj    *rt_str_to_bytes(Obj *s);

/* A hash map. Keys are `int` or `str`; the compiler restricts it, because
 * hashing a user type would need a Hashable interface that does not exist
 * yet. Open addressing with linear probing, which keeps the whole table in
 * one allocation and needs no per-entry node.
 *
 * `get` on a missing key TRAPS, the same as an out-of-range index: there is
 * no null to return, so the honest options are trap or force every read
 * through a check. `has` is there for the check. */
enum { SLOT_EMPTY = 0, SLOT_FULL = 1, SLOT_DEAD = 2 };

typedef struct {
    int64_t k;
    int64_t v;
    uint8_t state;
} MapSlot;

/* What a key is, which decides how it is hashed and compared. Three cases
 * rather than the `key_is_str` flag this used to be, because a user type is
 * a third kind and two bools would have had a fourth, meaningless state. */
typedef enum {
    MK_INT = 0, /* an int, a bool, a float's bits: hashed as a word    */
    MK_STR = 1, /* a str: hashed over its bytes, compared by value     */
    MK_OBJ = 2, /* a user type: its own `hash` and `eq` (rt.h TypeInfo)*/
} MapKey;

/* Public for the same reason as rt_arr_val_type: a module constant's map is
 * a static Map whose table the compiler has already hashed. Everything else
 * reaches a map through the functions below. */
typedef struct {
    Obj      hdr;
    MapSlot *slots;
    int64_t  cap;
    int64_t  len;      /* live entries */
    int64_t  used;     /* live + tombstones, for the load factor */
    uint8_t  key;      /* a MapKey */
    bool     key_is_ref;
    bool     val_is_ref;
} Map;

extern const TypeInfo rt_map_type;

Obj    *rt_map_new(int64_t key, bool key_is_ref, bool val_is_ref);
Obj    *rt_map_clone(Obj *o);         /* shallow, like rt_seq_clone */
void    rt_map_set(Obj *o, int64_t k, int64_t v);
int64_t rt_map_get(Obj *o, int64_t k);
bool    rt_map_has(Obj *o, int64_t k);
void    rt_map_remove(Obj *o, int64_t k);
int64_t rt_map_len(Obj *o);
Obj    *rt_map_keys(Obj *o);
Obj    *rt_map_values(Obj *o);
void    rt_map_clear(Obj *o);
int64_t rt_list_pop(Obj *o);
void    rt_list_insert(Obj *o, int64_t i, int64_t v);
int64_t rt_list_remove_at(Obj *o, int64_t i);
void    rt_list_clear(Obj *o, bool elems_are_refs);
void    rt_seq_reverse(Obj *o);
void    rt_sort_int(Obj *o);
void    rt_sort_str(Obj *o);
void    rt_sort_float(Obj *o);
/* Order the elements by their own type's `cmp` (TypeInfo above). */
void    rt_sort_obj(Obj *o);
/* How a sequence's elements compare. A slot is one machine word whatever it
 * holds, so the caller has to say what is in it. */
enum { SEQ_WORD = 0, SEQ_STR = 1, SEQ_FLOAT = 2 };
bool    rt_seq_contains(Obj *o, int64_t v, int kind);
int64_t rt_seq_index_of(Obj *o, int64_t v, int kind);

/* ---- concurrency -------------------------------------------------------
 *
 * Green threads (docs/concurrency-decision.md, Phases 1-3 wired to `spawn`):
 * every `spawn` is a green thread on a process-wide scheduler, never an OS
 * thread -- see rt_spawn below and runtime/scheduler.h. `Chan`'s send/recv
 * park the calling green thread instead of blocking the carrier OS thread
 * (rt.c's rt_chan_send/rt_chan_recv) -- the channel surface itself did not
 * change, exactly as this section used to promise it would not.
 *
 * A slot is 64 bits. The compiler knows the element type statically, so an
 * int rides in the slot directly and a reference rides as its pointer --
 * there is no tagging and no dynamic type test.
 *
 * Values are MOVED across a channel: the sender gives up its reference and
 * the receiver acquires it, with no retain or release in between. That is
 * what keeps rc_inc/rc_dec non-atomic. */
typedef struct Chan Chan;

Chan *rt_chan_new(int64_t capacity);
void  rt_chan_send(Chan *c, int64_t slot);
int64_t rt_chan_recv(Chan *c);
void  rt_chan_close(Chan *c);
void  rt_chan_drop(Chan *c);

/* Spawn a green thread running `entry(arg)`, on the process-wide scheduler
 * (lazily created on first use -- rt_global_scheduler() in rt.c). `spawn`
 * always means this now; there is no OS-thread spawn left (see
 * docs/concurrency-decision.md's "The decision being made here" for why a
 * conditional spawn was rejected). */
void rt_spawn(void (*entry)(void *), void *arg);

/* The whole of `main` -- source order at the top level, emitted as `$main`
 * -- also runs as a green thread (id 0 on the scheduler), not on the raw OS
 * thread, which is what lets a `Chan`/`net` call made before any `spawn`
 * park against something. Called exactly once, from generated `main()`:
 * creates the scheduler, spawns `entry` as its first green thread, waits
 * for it and everything it (transitively) spawns to finish -- including
 * spawns made after `entry` itself has returned -- then shuts the scheduler
 * down. See rt.c for why "spawned count caught up with completed count" is
 * an exact, race-free quiescence signal here and not an approximation. */
void rt_run_program(void (*entry)(void));

/* Aborts with "trap: <msg>" on stderr and exit status 134 (SIGABRT).
 * Out-of-line and _Noreturn so the checks below stay cheap. `cold` too, not
 * just _Noreturn -- docs/const-decision.md, "Cost, measured": gcc counted an
 * un-attributed trap call against the size of every function that reaches it
 * and stopped inlining the caller, a 70% slowdown that `__attribute__((cold))`
 * on rt_frozen_trap (above) fixed outright. `a[i]`'s bounds check (rt.h,
 * rt_index_get/rt_index_set/rt_bytes_get/rt_bytes_set) reaches this one on
 * every element access, so it gets the same attribute for the same reason. */
__attribute__((cold)) _Noreturn void rt_trap(const char *msg);

/* Traps unless `o` is the only reference to its object.
 *
 * A value crossing a thread boundary must be UNIQUE, or two threads end up
 * mutating one non-atomic refcount -- which is the race the whole
 * moved-not-shared design exists to prevent. The compiler refuses the cases
 * it can see; this catches the ones it cannot, such as a local that was
 * aliased earlier. Immortal objects pass: nothing ever counts them. */
void rt_check_unique(Obj *o);

/* ---- green threads, Phase 1: the compiler-emitted stack probe ----------
 *
 * docs/concurrency-decision.md, "Stacks: fixed, but not limited" and
 * "Phases" (Phase 1). Fixed-size stacks with no guard page -- the compiler
 * emits a check instead, so a million green threads cost ~1000 VMAs instead
 * of ~2 million. The rest of Phase 1 (the x86-64 context switch, the slab
 * allocator, the per-thread state table) is internal plumbing with no
 * reason to appear in a header included by emitted code, so it lives in
 * runtime/greenthread.h instead; this probe is the one piece every emitted
 * function must see.
 *
 * `rt_stack_limit` holds the lowest valid address of the stack belonging to
 * whichever green thread is currently running on THIS carrier OS thread --
 * thread-local because one OS thread runs one green thread's code at a
 * time. src/emit_c.rs (emit_func) opens every emitted function with a call
 * to `rt_stack_check()`, below.
 *
 * THAT CALL MUST STAY A REAL, OUT-OF-LINE CALL -- not the inlined
 * `{ int local; if (&local < rt_stack_limit) ... }` text this probe used to
 * be, CHICKEN's own idiom (still cited by the design doc for the local-
 * stands-in-for-rsp trick itself). A real, reproducible bug, found and fixed
 * the hard way: a single function that is live across a park/resume cycle --
 * any `prim` call can trigger one, since the compiler has no notion that the
 * OS thread actually running this code can change mid-function -- gets
 * exactly one probe at its own entry under this scheme, but a direct,
 * unrelated SOURCE call to another small function (`Conn.close`, say) gets
 * inlined under `-O2`, carrying ITS OWN copy of the same inlined probe text
 * along with it, deeper in the caller's body. `clang -O2` (not `gcc -O2`,
 * which happens not to in this exact shape) then computes `&rt_stack_limit`
 * -- the THREAD-LOCAL ADDRESS, via the `%fs`-relative thread pointer, not
 * merely its value -- ONCE, early in the caller, and caches it in a spilled
 * register across the real, opaque calls in between (`connect`/`write`/
 * `read_until`, any of which may have parked this green thread and resumed
 * it on a DIFFERENT carrier). The second, inlined probe then reads through
 * that stale, wrong-carrier address -- a value that carrier's own,
 * completely unrelated activity may since have overwritten to anything --
 * producing a spurious "stack overflow" trap a handful of real frames into a
 * fresh green thread, reproducible well over half the time with 2+ carriers
 * doing genuine concurrent socket I/O, confirmed via a hardware watchpoint on
 * the live TLS slot showing every actual WRITE to it was correct throughout.
 * This is ordinary, valid optimisation from the compiler's point of view --
 * nothing in C's memory model says a TLS variable's resolved address can
 * change mid-function, and nothing told it this runtime violates that.
 * `rt_stack_check` being a genuine, `noinline`, separate-TU call (the
 * runtime is already its own translation unit, with `-flto` off -- see the
 * project README's "Two rules that look like details and are not") means
 * the caller never sees a TLS access to cache in the first place: every call
 * to it forces a fresh `%fs`-relative lookup inside, with nothing for the
 * optimiser to hoist across. Do not re-inline the probe text at the call
 * site; that reintroduces exactly this bug.
 *
 * Every program now runs entirely on carriers (rt_run_program wraps `main`
 * itself as green thread 0, and every `spawn` is a green thread too -- see
 * the "concurrency" section above), so this is set for the whole of a
 * compiled program's life, not left at its default of 0 the way it was
 * before `spawn` was wired to the scheduler. A thread that is somehow not a
 * carrier (a hand-written C test harness that calls runtime functions
 * directly, never through rt_run_program) still sees the safe default: an
 * address is never 0, so the comparison is always false and the probe costs
 * one compare and one untaken branch per call. */
extern _Thread_local uintptr_t rt_stack_limit;

/* The real design (docs/concurrency-decision.md, Gambit's technique cited
 * there) POISONS rt_stack_limit to a value above every real address, so the
 * very same probe that catches overflow also doubles as a voluntary
 * preemption point: the next function call traps into here on purpose, not
 * because the stack is actually exhausted. RT_STACK_LIMIT_POISON is that
 * sentinel.
 *
 * Phase 1 never sets rt_stack_limit to this value -- there is no scheduler
 * yet for a "preempted" thread to yield TO, so nothing in this phase has a
 * reason to poison anything. The constant and the branch in
 * rt_stack_probe_slow exist now so Phase 2 can start poisoning without the
 * probe's own call site in emit_c.rs ever needing to change again. */
#define RT_STACK_LIMIT_POISON (~(uintptr_t)0)

/* The probe itself -- src/emit_c.rs calls this at the top of every emitted
 * function. `__attribute__((noinline))` is load-bearing, not defensive
 * styling: see rt_stack_limit's own comment above for exactly which bug
 * re-inlining this reintroduces. `-flto` is already off project-wide, so
 * cross-TU inlining could not happen anyway; the attribute documents the
 * requirement explicitly and survives a future build-flag change that this
 * comment might not get read before. */
__attribute__((noinline)) void rt_stack_check(void);

/* Called when the probe fires: either rt_stack_limit was poisoned (see
 * above -- not reachable in Phase 1) or the thread genuinely ran off the
 * end of its stack. Phase 1 only has to handle the second case, and it does
 * what every other runtime check in this file does: rt_trap, cleanly, with
 * a clear message. Defined in rt.c. */
void rt_stack_probe_slow(void);

/* ---- blocking FFI, Phase 3 (docs/concurrency-decision.md, "Blocking FFI")
 *
 * `O_NONBLOCK` is a no-op on a regular file and `getaddrinfo` is synchronous
 * by specification, so any foreign (`prim`) call might block the OS thread
 * underneath it for an arbitrary time. src/emit_c.rs wraps every such call
 * site in rt_enter_blocking()/rt_exit_blocking() -- unconditionally, the
 * same "always correct, always emitted" discipline the stack probe above
 * uses, and for the same reason: this header, and rt.c's implementation of
 * these two functions, are linked into EVERY compiled program, whether or
 * not it uses the Phase 2/3 scheduler at all, so the common case (no
 * scheduler) has to be free.
 *
 * The mechanism is the same shape as rt_stack_limit just above: a
 * thread-local the compiler-emitted call site touches with no argument to
 * pass, because src/emit_c.rs has no way to know, at a `prim` call site,
 * which carrier (if any) is running on this OS thread. `rt_blocking_rec` is
 * NULL on every OS thread that never registered one -- which, absent a
 * scheduler, is every thread there is -- and on such a thread both
 * functions below are a null check and nothing else.
 *
 * A carrier (runtime/scheduler.c) registers one rt_blocking_rec_t per
 * carrier OS thread, once, before it ever dispatches a green thread
 * (rt_blocking_register), and a monitor thread (runtime/monitor.c) polls
 * every registered carrier's record: a nonzero `blocking_since_ns` older
 * than its timeout means that carrier is stuck in a foreign call, and its
 * local buffer of queued-but-not-yet-run green threads is hinted away to a
 * backup carrier so they are not starved by the one carrier's blocking
 * syscall. See runtime/monitor.h for the full mechanism. */
typedef struct rt_blocking_rec {
    /* 0 = not currently inside a blocking FFI call. Otherwise a
     * CLOCK_MONOTONIC nanosecond timestamp of when the current call
     * entered -- never wall-clock time, which can jump backwards and would
     * make "how long has this been blocking" unanswerable. */
    _Atomic uint64_t blocking_since_ns;
} rt_blocking_rec_t;

extern _Thread_local rt_blocking_rec_t *rt_blocking_rec;

/* Called once by a carrier, on its own OS thread, before it dispatches any
 * green thread -- sets rt_blocking_rec for THIS thread only, exactly like
 * rt_fiber_switch sets rt_stack_limit. `rec`'s storage is owned by the
 * caller (the scheduler) and must outlive every call this carrier makes to
 * rt_enter_blocking/rt_exit_blocking, i.e. for the carrier's whole life. */
void rt_blocking_register(rt_blocking_rec_t *rec);

/* Emitted immediately before/after every `prim` call site (src/emit_c.rs).
 * A plain, uncontended atomic store on the fast path when nobody is
 * watching (rt_blocking_rec == NULL: no-op; non-NULL: one relaxed-adjacent
 * store), so the cost of instrumenting every FFI call is negligible for a
 * program that never uses the scheduler. */
void rt_enter_blocking(void);
void rt_exit_blocking(void);

/* Checked arithmetic. int is 64-bit and overflow TRAPS -- docs/ir-v0.md §3.
 * The operators never wrap; wrapping is a separately named method
 * (`a.wrapping_add(b)`, below), so a wrap is always visible in the source.
 *
 * __builtin_*_overflow is gcc 5+ and clang 3.8+. Verified 2026-09-18:
 * identical values and identical trap behaviour under gcc and clang at -O0
 * and -O2, exit 134, no warnings. */
static inline int64_t rt_iadd(int64_t a, int64_t b) {
    int64_t r;
    if (__builtin_add_overflow(a, b, &r)) rt_trap("integer overflow in +");
    return r;
}

static inline int64_t rt_isub(int64_t a, int64_t b) {
    int64_t r;
    if (__builtin_sub_overflow(a, b, &r)) rt_trap("integer overflow in -");
    return r;
}

static inline int64_t rt_imul(int64_t a, int64_t b) {
    int64_t r;
    if (__builtin_mul_overflow(a, b, &r)) rt_trap("integer overflow in *");
    return r;
}

/* Division has two trapping cases, and the second one is the reason this is
 * not just `a / b`: INT64_MIN / -1 is undefined behaviour in C, and on x86 it
 * faults with SIGFPE rather than producing a value. Trapping deliberately
 * turns both into the same diagnosable failure. */
static inline int64_t rt_idiv(int64_t a, int64_t b) {
    if (b == 0) rt_trap("division by zero");
    if (a == INT64_MIN && b == -1) rt_trap("integer overflow in /");
    return a / b;
}

static inline int64_t rt_irem(int64_t a, int64_t b) {
    if (b == 0) rt_trap("remainder by zero");
    if (a == INT64_MIN && b == -1) rt_trap("integer overflow in %");
    return a % b;
}

/* Bit operations -- reference §6.1. They are operations on the 64-bit
 * pattern, not on the number, so none of them traps on "overflow"; only a
 * shift count outside 0..63 traps, because C leaves that undefined and the
 * hardware disagrees about it (x86 masks the count to 6 bits, so `1 << 64`
 * would quietly be 1). That bounds check branches on `n`, never on `a` --
 * see rt_ishr below for why that distinction is load-bearing.
 *
 * Nothing here relies on signed behaviour C leaves open. Shifting a negative
 * value left is undefined, so the shift is done on uint64_t. Converting a
 * uint64_t above INT64_MAX back to int64_t is implementation-defined, so it
 * goes back by bit pattern, the way rt_i2f does. */
static inline int64_t rt_u2i(uint64_t u) {
    int64_t n;
    __builtin_memcpy(&n, &u, sizeof n);
    return n;
}

static inline int64_t rt_iand(int64_t a, int64_t b) { return a & b; }
static inline int64_t rt_ior(int64_t a, int64_t b)  { return a | b; }
static inline int64_t rt_ixor(int64_t a, int64_t b) { return a ^ b; }

static inline int64_t rt_ishl(int64_t a, int64_t n) {
    if (n < 0 || n > 63) rt_trap("shift count out of range in <<");
    return rt_u2i((uint64_t)a << n);
}

/* Shifting a negative value right is implementation-defined in C, so the
 * sign extension has to be spelled out -- and it has to be spelled out
 * without branching on the sign of `a`. `a < 0 ? ~(~a >> n) : a >> n` (the
 * previous version of this function) is exactly such a branch: its taken
 * direction is correlated with the value being shifted, which is a real
 * timing side channel the moment this function is ever used on secret data
 * (found in review while building this project's SSH crypto -- a language
 * whose stdlib includes cryptographic primitives cannot have this in its
 * runtime).
 *
 * The fix does the shift entirely in the unsigned domain, then ORs in the
 * sign-extension bits computed arithmetically instead of selected by branch:
 *   - `ua >> n` is a logical shift -- well-defined, no sign extension.
 *   - `sign_mask` is all-ones if `a` was negative, all-zero otherwise,
 *     computed as an unsigned subtraction (wraps by definition, no UB) on
 *     the sign bit alone -- no comparison, no branch.
 *   - the top `n` bits of that mask are what a logical shift is missing;
 *     `(sign_mask << (63 - n)) << 1` computes exactly those bits without
 *     ever shifting by 64, which would itself be UB at n = 0 if written as
 *     the more obvious `sign_mask << (64 - n)`. Splitting one full-width
 *     shift into two shifts of at most 63 sidesteps that entirely.
 * The only branch left is the bounds check on `n` above, and `n` -- a shift
 * count -- is structural (fixed by the algorithm: "rotate by 6", "shift by
 * 26") in every real use, never itself secret data. */
static inline int64_t rt_ishr(int64_t a, int64_t n) {
    if (n < 0 || n > 63) rt_trap("shift count out of range in >>");
    uint64_t ua;
    __builtin_memcpy(&ua, &a, sizeof ua);
    uint64_t logical   = ua >> n;
    uint64_t sign_mask = (uint64_t)0 - (ua >> 63);
    uint64_t fill      = (sign_mask << (63 - n)) << 1;
    return rt_u2i(logical | fill);
}

/* Wrapping arithmetic, for hashes and PRNGs, which are defined modulo 2^64.
 * Unsigned arithmetic in C wraps by definition, so these are the checked
 * helpers above with the check taken out and the sign taken off. */
static inline int64_t rt_wrapping_add(int64_t a, int64_t b) {
    return rt_u2i((uint64_t)a + (uint64_t)b);
}

static inline int64_t rt_wrapping_sub(int64_t a, int64_t b) {
    return rt_u2i((uint64_t)a - (uint64_t)b);
}

static inline int64_t rt_wrapping_mul(int64_t a, int64_t b) {
    return rt_u2i((uint64_t)a * (uint64_t)b);
}

#endif /* RT_H */
