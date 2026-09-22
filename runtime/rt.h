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

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define RC_IMMORTAL (-1)

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
typedef struct TypeInfo {
    DropFn       drop;
    const AnyFn *vtable;
    WalkFn       walk;
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
void rt_print_float(double x);

/* The seam for `lib/io.src`. A primitive returns only what the runtime can
 * build without the compiler's help: a scalar, a str, or an element pushed
 * onto a collection the caller passed in. It never builds an `Option` or a
 * `Result` -- their layout and TypeInfo belong to the compiler -- so a
 * failure comes back as a raw errno and the library turns it into its own
 * error type, in source where the mapping can be read. */
int64_t rt_file_read(Obj *path, Obj *out);          /* 0, or errno */
int64_t rt_file_write(Obj *path, Obj *data);        /* 0, or errno */
int64_t rt_file_append(Obj *path, Obj *data);       /* 0, or errno */
int64_t rt_stdin_line(Obj *out);                    /* 1 pushed a line, 0 at end */
void    rt_stderr_write(Obj *s);
void rt_format_float(char *buf, size_t cap, double x);

/* Text primitives a library cannot write from inside the language. */
int64_t rt_str_byte_at(Obj *o, int64_t i);
bool    rt_str_parse_int(Obj *o, int64_t *out);
bool    rt_str_parse_float(Obj *o, double *out);
Obj    *rt_int_to_str(int64_t n);
Obj    *rt_bool_to_str(bool b);
Obj    *rt_float_to_str(double x);

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

/* `fill` is the initial value of every element. There is no null in the
 * language, so an array cannot start with empty slots: the caller must say
 * what an unset element is. For references the fill is retained once per
 * element. */
Obj *rt_array_new(int64_t len, int64_t fill, bool elems_are_refs);
Obj *rt_list_new(bool elems_are_refs);
Obj *rt_list_repeat(int64_t n, int64_t fill, bool elems_are_refs);
Obj *rt_array_blank(int64_t len, bool elems_are_refs);
void rt_array_put(Obj *o, int64_t i, int64_t v);

int64_t rt_len_of(Obj *o);            /* works for both */
int64_t rt_index_get(Obj *o, int64_t i);
void    rt_index_set(Obj *o, int64_t i, int64_t v);
void    rt_list_push(Obj *o, int64_t v);
Obj    *rt_seq_clone(Obj *o);         /* shallow copy of an array or list */
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

Obj    *rt_bytes_new(int64_t cap);              /* empty, room for `cap` */
Obj    *rt_bytes_fill(int64_t n, int64_t v);    /* `[v; n]` */
int64_t rt_bytes_len(Obj *o);
int64_t rt_bytes_get(Obj *o, int64_t i);
void    rt_bytes_set(Obj *o, int64_t i, int64_t v);
void    rt_bytes_push(Obj *o, int64_t v);
int64_t rt_bytes_pop(Obj *o);
void    rt_bytes_clear(Obj *o);
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
Obj    *rt_map_new(bool key_is_str, bool key_is_ref, bool val_is_ref);
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
/* How a sequence's elements compare. A slot is one machine word whatever it
 * holds, so the caller has to say what is in it. */
enum { SEQ_WORD = 0, SEQ_STR = 1, SEQ_FLOAT = 2 };
bool    rt_seq_contains(Obj *o, int64_t v, int kind);
int64_t rt_seq_index_of(Obj *o, int64_t v, int kind);

/* ---- concurrency -------------------------------------------------------
 *
 * OS threads and blocking channels, which is stage 2 of
 * docs/concurrency-decision.md: get the channel semantics right against a
 * simple scheduler before building a real one. Green threads replace the
 * thread half later; the channel surface does not change.
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

/* Spawn a thread running `entry(arg)`. The thread is detached: there is no
 * join yet, and `rt_wait_all` below is what the program end waits on. */
void rt_spawn(void *(*entry)(void *), void *arg);

/* Block until every spawned thread has finished. Emitted at the end of the
 * program, so a spawn cannot outlive main and silently lose its output. */
void rt_wait_all(void);

/* Aborts with "trap: <msg>" on stderr and exit status 134 (SIGABRT).
 * Out-of-line and _Noreturn so the checks below stay cheap: the compiler
 * treats the trap edge as cold and keeps the hot path straight. */
_Noreturn void rt_trap(const char *msg);

/* Traps unless `o` is the only reference to its object.
 *
 * A value crossing a thread boundary must be UNIQUE, or two threads end up
 * mutating one non-atomic refcount -- which is the race the whole
 * moved-not-shared design exists to prevent. The compiler refuses the cases
 * it can see; this catches the ones it cannot, such as a local that was
 * aliased earlier. Immortal objects pass: nothing ever counts them. */
void rt_check_unique(Obj *o);

/* Checked arithmetic. int is 64-bit and overflow TRAPS -- docs/ir-v0.md §3.
 * There is no wrapping variant; one way to do each thing.
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

#endif /* RT_H */
