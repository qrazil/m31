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
