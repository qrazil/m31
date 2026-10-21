/* Heap watch: one ledger for every object (docs/heap-watch-design.md).
 *
 * #included by rt.c, for the reason the sys layer and greenthread.c are: the
 * runtime stays one translation unit, so every build line keeps naming rt.c
 * alone. It uses the C library directly for the four things the sys layer
 * has no call for -- mmap, ftruncate, a directory listing and the process
 * id -- the way greenthread.c already does for its stack slabs.
 *
 * The rules this file keeps, from the design:
 *
 *   - No heap walk, ever. Nothing here visits objects to find leaks: every
 *     answer comes from counters that allocation and release keep up to
 *     date as they happen.
 *   - Nothing is freed by heap watch. It reports; the program is unchanged.
 *   - O(1) per operation. An allocation and a release each update a fixed
 *     number of counters in the calling thread's own shard.
 *
 * The ledger is a file, mapped MAP_SHARED, so a counter update is a plain
 * store into the page cache and the kernel writes it back on its own: the
 * ledger survives the process dying in any way, `kill -9` included, with no
 * exit hook involved. `m31c -heapwatch` reads it from a live or a dead
 * process (src/tools/heapwatch.rs, which documents the same layout).
 *
 * Off with M31_HEAPWATCH=0: then nothing is counted and no file is made;
 * an object's header still has its fields, all of them zero but the epoch,
 * which reads "counted against none" (rt.h). */

#include <dirent.h>
#include <fcntl.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

/* ---- the file layout ---------------------------------------------------
 *
 * Every offset and size here is also in src/tools/heapwatch.rs; a change to
 * one is a change to both, and HW_VERSION with it.
 *
 *   0       control page   requests from the tool (trace on and off)
 *   4096    header         HwHeader
 *   8192    slots          HwSlot[max_slots]: what each slot counts
 *   ...     traced         uint8_t[max_slots]: trace on for this slot
 *   ...     strings        type names and file names, NUL-terminated
 *   ...     epochs         HwEpochRow[HW_EPOCH_ROWS]: epochs that leaked
 *   ...     shards         n_shards x (HwShardHead, HwCount[max_slots])
 *
 * A SLOT is one allocation site of one type: the ledger's unit of account.
 * Slot 0 is never used; an object whose type record has site 0 was made
 * before heap watch started, or after the slots ran out, and is not
 * counted. */
#define HW_VERSION 1
#define HW_PAGE 4096
#define HW_LEVELS 7
#define HW_CELLS 8
#define HW_LIFE 16
#define HW_EPOCH_ROWS 64
#define HW_STRINGS (1u << 20)
#define HW_DYNAMIC_SLOTS 4096u
#define HW_MAX_SHARDS 256u
#define HW_KEEP_LEDGERS 8

/* One slot's counters in one shard: 320 bytes.
 *
 * `wheel` counts the slot's live objects by the minute they were born, as a
 * timing wheel: level L has HW_CELLS cells of 8^L minutes each, so level 0
 * holds the last 8 minutes one by one and level 6 reaches past the 20-bit
 * birth field. When the shard's clock moves on, the cell about to be reused
 * at a level is folded into the level above (hw_fold), so a cell's meaning
 * depends only on the shard's own `minute` (HwShardHead). An object born in
 * minute m and released on another shard is subtracted there, at whatever
 * level that shard holds m; the reader adds the shards up block by block
 * (hw_sum_slot), so the +1 and the -1 meet in the coarser of the two
 * blocks. Counts are signed for that reason: one shard alone may be
 * negative.
 *
 * `life` is the slot's lifetime histogram: an object that dies at age `a`
 * minutes adds 1 to bucket 0 when a is 0, else to bucket floor(log2 a) + 1,
 * the last bucket taking everything older. It is what the slot's
 * NORMAL lifetime is learned from. */
typedef struct {
    int32_t  wheel[HW_LEVELS][HW_CELLS];
    uint32_t life[HW_LIFE];
    int32_t  forever;   /* live objects marked heapwatch.forever */
    uint32_t pad[7];
} HwCount;

typedef struct {
    uint32_t minute;    /* the minute this shard's wheels are folded to */
    uint32_t owned;     /* 1 once a thread has it */
    uint32_t pad[14];
} HwShardHead;

/* What a slot counts. Strings are offsets into the string area. */
typedef struct {
    uint32_t type_off;
    uint32_t file_off;
    uint32_t line;
    uint32_t flags;     /* HW_SLOT_* */
    uint32_t limit;     /* minutes, when HW_SLOT_LIMIT */
    uint32_t level;     /* suspect: the last doubling written to the log, +1 */
    uint32_t pad[2];
} HwSlot;

#define HW_SLOT_FOREVER 1u  /* M31_AGE said this site or type lives forever */
#define HW_SLOT_LIMIT   2u  /* M31_AGE gave this site or type an age limit */

/* An epoch name and how often it closed with leaks. */
typedef struct {
    char     name[48];
    uint64_t times;
    uint32_t minute;    /* when it last did */
    uint32_t pad;
    char     last[64];  /* the first leaked site of that close */
} HwEpochRow;

typedef struct {
    char     magic[8];      /* "M31HWLDG" */
    uint32_t version;
    uint32_t page;
    uint64_t pid;
    int64_t  start_unix;    /* wall clock at start, seconds */
    int64_t  tick_unix;     /* wall clock at the last tick, seconds */
    uint32_t minute;        /* the program's clock: minutes since start */
    uint32_t minute_ms;     /* how long a minute is (60000, or a test's) */
    uint32_t n_shards;
    uint32_t max_slots;
    uint32_t n_slots;       /* slots handed out, slot 0 included */
    uint32_t count_size;    /* sizeof(HwCount) */
    uint64_t off_slots;
    uint64_t off_traced;
    uint64_t off_strings;
    uint64_t off_epochs;
    uint64_t off_shards;
    uint64_t shard_stride;
    uint32_t strings_cap;
    uint32_t strings_used;
    uint32_t floor_min;     /* M31_HEAPWATCH_MIN */
    uint32_t factor;        /* M31_HEAPWATCH_FACTOR */
    uint32_t running;       /* 1 until a clean exit */
    uint32_t lost_slots;    /* sites that found the slots full */
    uint32_t trace_depth;   /* --depth for trace (phase 2) */
    uint32_t edges;         /* trace: edges recorded now */
    uint32_t edges_dropped; /* trace: edges dropped at the cap */
    uint32_t pad;
    char     program[128];
} HwHeader;

/* ---- process state ---------------------------------------------------- */

static bool             hw_on;          /* counting at all */
static unsigned char   *hw_base;        /* the mapping */
static size_t           hw_size;
static HwHeader        *hw_head;
static HwSlot          *hw_slots;
static uint8_t         *hw_traced;
static char            *hw_strings;
static HwEpochRow      *hw_epochs;
static unsigned char   *hw_shards;
static uint32_t         hw_minute_now;  /* read with relaxed atomics */
static uint64_t         hw_minute_ns;
static uint64_t         hw_start_ns;
static uint32_t         hw_next_shard;
static pthread_mutex_t  hw_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t  hw_shared_lock = PTHREAD_MUTEX_INITIALIZER;
static char             hw_path[1100];  /* the ledger; "" when anonymous */
static size_t           hw_path_stem;   /* length of hw_path without ".ledger" */
static bool             hw_reported;    /* anything written to .suspects */
static bool             hw_keep;        /* M31_HEAPWATCH_KEEP */
static int              hw_log_fd = -1;

/* The program's source lines and its static slots (rt_hw_program). */
static const RtHwLoc         *hw_locs;
static uint32_t               hw_nlocs;
/* The site records made at run time for the runtime's own types: one per
 * (line, kind), published once and never freed -- static data in all but
 * name. */
#define HW_KINDS 8
static const TypeInfo *_Atomic *hw_loc_types;

/* The line the program is at, for an object the RUNTIME makes on its behalf
 * (a str from concat, a List from a literal). Set by emitted code just
 * before a runtime call that may allocate (rt_hw_at). Thread-local because
 * the line belongs to whichever green thread is running on this carrier,
 * and it is set immediately before the call that reads it, with no park in
 * between. */
static _Thread_local uint32_t hw_loc;

/* This OS thread's shard, NULL until its first allocation. */
static _Thread_local HwShardHead *hw_shard;
static _Thread_local bool         hw_shard_shared;

/* Trace (phase 2) and epoch close (phase 3), below the ledger. */
static void hw_trace_slot(uint32_t s, uint8_t on);
static void hw_trace_suspect(uint32_t s, uint32_t now, uint32_t limit);
static void hw_control_init(void);
static void hw_control(void);
static long hw_epoch_of_new(const TypeInfo *ty);
static void hw_epoch_free(Obj *o);
static void hw_epoch_store(Obj *h, Obj *v);
static uint32_t hw_edges_max;
static uint8_t *hw_edge_sites;
static bool hw_trap_on_leak;
static void hw_trace_free(Obj *o);

static inline uint32_t hw_now(void) {
    return __atomic_load_n(&hw_minute_now, __ATOMIC_RELAXED);
}

static inline HwCount *hw_counts(HwShardHead *sh) {
    return (HwCount *)(void *)(sh + 1);
}

static inline int32_t hw_ld(const int32_t *p) {
    return __atomic_load_n(p, __ATOMIC_RELAXED);
}

static inline uint32_t hw_ldu(const uint32_t *p) {
    return __atomic_load_n(p, __ATOMIC_RELAXED);
}

/* Counters are only ever written by the shard's owner, so an update is a
 * plain load and store -- relaxed atomics, which compile to exactly that,
 * so the tick reading them from another thread is not a data race. The one
 * shared shard (more threads than shards) takes hw_shared_lock instead. */
static inline void hw_add(int32_t *p, int32_t d) {
    __atomic_store_n(p, __atomic_load_n(p, __ATOMIC_RELAXED) + d, __ATOMIC_RELAXED);
}

static inline void hw_addu(uint32_t *p, uint32_t d) {
    __atomic_store_n(p, __atomic_load_n(p, __ATOMIC_RELAXED) + d, __ATOMIC_RELAXED);
}

static const char *hw_str(uint32_t off) {
    return hw_strings + off;
}

/* ---- the timing wheel --------------------------------------------------- */

/* Fold the shard's wheels from its own minute to `now`: at each level, each
 * cell that a later minute is about to reuse hands its count up a level.
 * Only the shard's owner calls this, so it needs no lock. At most
 * HW_LEVELS x HW_CELLS moves per slot, once a minute at most per shard --
 * O(slots) work, never O(objects). */
static void hw_fold(HwShardHead *sh, uint32_t now) {
    uint32_t was = sh->minute;
    if (now <= was) return;
    uint32_t n_slots = __atomic_load_n(&hw_head->n_slots, __ATOMIC_ACQUIRE);
    HwCount *cs = hw_counts(sh);
    for (int lv = 0; lv < HW_LEVELS - 1; lv++) {
        uint32_t old_k = was >> (3 * lv), new_k = now >> (3 * lv);
        if (old_k == new_k) break;
        /* Cell `idx` holds the one k <= old_k that is idx mod 8 (and no
         * older than old_k - 7). If the new window no longer reaches it,
         * its count moves up a level, to the cell for k >> 3. */
        for (uint32_t idx = 0; idx < HW_CELLS; idx++) {
            uint32_t back = (old_k - idx) & (HW_CELLS - 1);
            if (back > old_k) continue; /* that minute never existed */
            uint32_t k = old_k - back;
            if (new_k - k < HW_CELLS) continue;
            uint32_t up = (k >> 3) & (HW_CELLS - 1);
            for (uint32_t s = 1; s < n_slots; s++) {
                int32_t v = hw_ld(&cs[s].wheel[lv][idx]);
                if (v == 0) continue;
                hw_add(&cs[s].wheel[lv + 1][up], v);
                __atomic_store_n(&cs[s].wheel[lv][idx], 0, __ATOMIC_RELAXED);
            }
        }
    }
    __atomic_store_n(&sh->minute, now, __ATOMIC_RELEASE);
}

/* The cell that holds an object born in minute `m`, on a shard folded to
 * `sm` (m <= sm): the lowest level whose window still reaches back to it. */
static int32_t *hw_cell(HwCount *c, uint32_t sm, uint32_t m) {
    for (int lv = 0; lv < HW_LEVELS; lv++) {
        uint32_t k = m >> (3 * lv), K = sm >> (3 * lv);
        if (K - k < HW_CELLS || lv == HW_LEVELS - 1) {
            return &c->wheel[lv][k & (HW_CELLS - 1)];
        }
    }
    return &c->wheel[HW_LEVELS - 1][0];
}

static unsigned hw_life_bucket(uint32_t age) {
    if (age == 0) return 0;
    unsigned b = 32u - (unsigned)__builtin_clz(age);
    return b < HW_LIFE ? b : HW_LIFE - 1;
}

/* This thread's shard, claimed on first use. Past the last shard every
 * further thread shares the last one, under a lock. */
static HwShardHead *hw_my_shard(void) {
    HwShardHead *sh = hw_shard;
    if (sh != NULL) return sh;
    uint32_t i = __atomic_fetch_add(&hw_next_shard, 1, __ATOMIC_RELAXED);
    if (i >= hw_head->n_shards) {
        i = hw_head->n_shards - 1;
        hw_shard_shared = true;
    }
    sh = (HwShardHead *)(void *)(hw_shards + (size_t)i * hw_head->shard_stride);
    __atomic_store_n(&sh->owned, 1, __ATOMIC_RELAXED);
    hw_shard = sh;
    return sh;
}

/* ---- strings and slots -------------------------------------------------- */

/* Copy a string into the ledger's string area, once per distinct string.
 * Called with hw_lock held. */
static uint32_t hw_intern(const char *s) {
    if (s == NULL) s = "?";
    uint32_t used = hw_head->strings_used;
    /* Interned by search: a program has a few hundred distinct names, and
     * this runs once per slot, never per object. */
    for (uint32_t at = 1; at < used;) {
        const char *have = hw_strings + at;
        if (strcmp(have, s) == 0) return at;
        at += (uint32_t)strlen(have) + 1;
    }
    size_t n = strlen(s) + 1;
    if (used + n > hw_head->strings_cap) return 0;
    memcpy(hw_strings + used, s, n);
    __atomic_store_n(&hw_head->strings_used, used + (uint32_t)n, __ATOMIC_RELEASE);
    return used;
}

/* ---- configuration: M31_AGE and M31_HEAPWATCH_CONF ----------------------
 *
 * An entry says how long objects of a type, or of one site of a type, are
 * meant to live:
 *
 *     Type:forever              every site of Type lives as long as it likes
 *     Type:10m                  every site of Type: suspect past 10 minutes
 *     Type@file.m31:40:forever  that site only (a duration works there too)
 *
 * Durations are a number and a unit, s m h or d; seconds round up to the
 * minute, which is the ledger's resolution. M31_AGE holds entries separated
 * by commas; the file named by M31_HEAPWATCH_CONF holds one per line, `#`
 * starts a comment, and a line `trace SPEC` turns trace on at start like
 * M31_HEAPWATCH_TRACE. A malformed entry is reported once on stderr and
 * ignored: the program runs either way. */
typedef struct {
    char     type[96];
    char     file[160];  /* "" for every site */
    uint32_t line;
    uint32_t limit;      /* minutes; 0 with forever */
    bool     forever;
} HwRule;

#define HW_MAX_RULES 64
static HwRule   hw_rules[HW_MAX_RULES];
static uint32_t hw_nrules;

/* A trace request kept for slots made later (phase 2 uses it). */
typedef struct {
    char     type[96];
    char     file[160];
    uint32_t line;
} HwSpec;

#define HW_MAX_TRACE 64
static HwSpec   hw_trace_specs[HW_MAX_TRACE];
static uint32_t hw_ntrace;

static void hw_warn(const char *what, const char *text) {
    char buf[400];
    int n = snprintf(buf, sizeof buf, "heapwatch: ignoring %s '%s'\n", what, text);
    if (n > 0) write_all(2, buf, (size_t)(n < (int)sizeof buf ? n : (int)sizeof buf - 1));
}

/* "Type" or "Type@file:line" into a spec; false when malformed. */
static bool hw_parse_spec(const char *s, size_t n, HwSpec *out) {
    memset(out, 0, sizeof *out);
    const char *at = memchr(s, '@', n);
    size_t tn = at != NULL ? (size_t)(at - s) : n;
    if (tn == 0 || tn >= sizeof out->type) return false;
    memcpy(out->type, s, tn);
    if (at == NULL) return true;
    const char *rest = at + 1;
    size_t rn = n - tn - 1;
    const char *colon = NULL;
    for (size_t i = rn; i > 0; i--) {
        if (rest[i - 1] == ':') {
            colon = rest + i - 1;
            break;
        }
    }
    if (colon == NULL) return false;
    size_t fn = (size_t)(colon - rest);
    if (fn == 0 || fn >= sizeof out->file) return false;
    memcpy(out->file, rest, fn);
    uint32_t line = 0;
    const char *p = colon + 1;
    if (p == rest + rn) return false;
    for (; p < rest + rn; p++) {
        if (*p < '0' || *p > '9' || line > 100000000u) return false;
        line = line * 10 + (uint32_t)(*p - '0');
    }
    out->line = line;
    return true;
}

/* One M31_AGE entry: SPEC:forever or SPEC:<duration>. */
static void hw_parse_rule(const char *s, size_t n) {
    while (n > 0 && (*s == ' ' || *s == '\t')) s++, n--;
    while (n > 0 && (s[n - 1] == ' ' || s[n - 1] == '\t' || s[n - 1] == '\r')) n--;
    if (n == 0) return;
    char text[300];
    size_t tn = n < sizeof text - 1 ? n : sizeof text - 1;
    memcpy(text, s, tn);
    text[tn] = 0;
    const char *colon = NULL;
    for (size_t i = n; i > 0; i--) {
        if (s[i - 1] == ':') {
            colon = s + i - 1;
            break;
        }
    }
    if (colon == NULL || hw_nrules == HW_MAX_RULES) {
        hw_warn("age entry", text);
        return;
    }
    HwSpec spec;
    if (!hw_parse_spec(s, (size_t)(colon - s), &spec)) {
        hw_warn("age entry", text);
        return;
    }
    const char *v = colon + 1;
    size_t vn = n - (size_t)(v - s);
    HwRule r;
    memset(&r, 0, sizeof r);
    memcpy(r.type, spec.type, sizeof r.type);
    memcpy(r.file, spec.file, sizeof r.file);
    r.line = spec.line;
    if (vn == 7 && memcmp(v, "forever", 7) == 0) {
        r.forever = true;
    } else {
        uint64_t num = 0;
        size_t i = 0;
        for (; i < vn && v[i] >= '0' && v[i] <= '9'; i++) {
            num = num * 10 + (uint64_t)(v[i] - '0');
            if (num > 100000000ull) break;
        }
        if (i == 0 || i + 1 != vn) {
            hw_warn("age entry", text);
            return;
        }
        uint64_t minutes;
        switch (v[i]) {
        case 's': minutes = (num + 59) / 60; break;
        case 'm': minutes = num; break;
        case 'h': minutes = num * 60; break;
        case 'd': minutes = num * 1440; break;
        default: hw_warn("age entry", text); return;
        }
        r.limit = minutes == 0 ? 1 : (uint32_t)minutes;
    }
    hw_rules[hw_nrules++] = r;
}

static void hw_add_trace_spec(const char *s, size_t n) {
    while (n > 0 && (*s == ' ' || *s == '\t')) s++, n--;
    while (n > 0 && (s[n - 1] == ' ' || s[n - 1] == '\t' || s[n - 1] == '\r')) n--;
    if (n == 0) return;
    HwSpec spec;
    if (hw_ntrace == HW_MAX_TRACE || !hw_parse_spec(s, n, &spec)) {
        char text[300];
        size_t tn = n < sizeof text - 1 ? n : sizeof text - 1;
        memcpy(text, s, tn);
        text[tn] = 0;
        hw_warn("trace entry", text);
        return;
    }
    hw_trace_specs[hw_ntrace++] = spec;
}

/* Comma-separated entries, each handed to `one`. */
static void hw_each_entry(const char *list, void (*one)(const char *, size_t)) {
    if (list == NULL) return;
    const char *p = list;
    while (*p != 0) {
        const char *e = strchr(p, ',');
        size_t n = e != NULL ? (size_t)(e - p) : strlen(p);
        one(p, n);
        if (e == NULL) break;
        p = e + 1;
    }
}

static void hw_read_conf(const char *path) {
    int64_t fd = sys_open(path, SYS_O_RDONLY, 0);
    if (fd < 0) {
        hw_warn("config file", path);
        return;
    }
    char buf[16384];
    int64_t got = 0, r;
    while (got < (int64_t)sizeof buf - 1 && (r = sys_read(fd, buf + got, (int64_t)sizeof buf - 1 - got)) > 0) {
        got += r;
    }
    sys_close(fd);
    buf[got] = 0;
    char *line = buf;
    while (line != NULL && *line != 0) {
        char *nl = strchr(line, '\n');
        if (nl != NULL) *nl = 0;
        char *hash = strchr(line, '#');
        if (hash != NULL) *hash = 0;
        char *t = line;
        while (*t == ' ' || *t == '\t') t++;
        if (strncmp(t, "trace ", 6) == 0) {
            hw_each_entry(t + 6, hw_add_trace_spec);
        } else {
            hw_each_entry(t, hw_parse_rule);
        }
        line = nl != NULL ? nl + 1 : NULL;
    }
}

/* Does a written type name match the type a slot counts? The program's own
 * spelling, `Conn` or `net.Conn`, and for an imported type the bare name
 * too. */
static bool hw_type_matches(const char *want, const char *have) {
    if (strcmp(want, have) == 0) return true;
    const char *dot = strrchr(have, '.');
    return dot != NULL && strchr(want, '.') == NULL && strcmp(want, dot + 1) == 0;
}

/* `file` as written in an entry against the slot's own: equal, or the
 * slot's path ends in "/" + it. */
static bool hw_file_matches(const char *want, const char *have) {
    if (strcmp(want, have) == 0) return true;
    size_t wn = strlen(want), hn = strlen(have);
    return hn > wn && have[hn - wn - 1] == '/' && strcmp(have + hn - wn, want) == 0;
}

static bool hw_spec_matches(const char *type, const char *file, uint32_t line,
                            const char *stype, const char *sfile, uint32_t sline) {
    if (!hw_type_matches(stype, type)) return false;
    if (sfile[0] == 0) return true;
    return sline == line && hw_file_matches(sfile, file);
}

/* Apply the rules and trace specs to a new slot. Called with hw_lock held,
 * before the slot is published. The most specific rule wins: a site rule
 * over a type rule. */
static void hw_apply_rules(uint32_t s, const char *type, const char *file, uint32_t line) {
    HwSlot *sl = &hw_slots[s];
    bool site_rule = false;
    for (uint32_t i = 0; i < hw_nrules; i++) {
        const HwRule *r = &hw_rules[i];
        if (!hw_spec_matches(type, file, line, r->type, r->file, r->line)) continue;
        bool is_site = r->file[0] != 0;
        if (site_rule && !is_site) continue;
        site_rule = site_rule || is_site;
        sl->flags &= ~(HW_SLOT_FOREVER | HW_SLOT_LIMIT);
        if (r->forever) {
            sl->flags |= HW_SLOT_FOREVER;
        } else {
            sl->flags |= HW_SLOT_LIMIT;
            sl->limit = r->limit;
        }
    }
    for (uint32_t i = 0; i < hw_ntrace; i++) {
        const HwSpec *t = &hw_trace_specs[i];
        if (hw_spec_matches(type, file, line, t->type, t->file, t->line)) {
            hw_trace_slot(s, 1);
        }
    }
}

/* A new slot for `ty` at `loc`; 0 when the slots have run out. Called with
 * hw_lock held. */
static uint32_t hw_new_slot(const char *type, uint32_t loc) {
    uint32_t s = hw_head->n_slots;
    if (s >= hw_head->max_slots) {
        hw_head->lost_slots++;
        return 0;
    }
    const char *file = loc < hw_nlocs ? hw_locs[loc].file : "?";
    uint32_t line = loc < hw_nlocs ? hw_locs[loc].line : 0;
    HwSlot *sl = &hw_slots[s];
    sl->type_off = hw_intern(type);
    sl->file_off = hw_intern(file);
    sl->line = line;
    hw_apply_rules(s, type, file, line);
    __atomic_store_n(&hw_head->n_slots, s + 1, __ATOMIC_RELEASE);
    return s;
}

/* Which of the runtime's own types a canonical record is, as an index into
 * a line's table of site records. */
static unsigned hw_kind(const TypeInfo *canon) {
    if (canon == &rt_str_type) return 0;
    if (canon == &rt_arr_val_type) return 1;
    if (canon == &rt_arr_ref_type) return 2;
    if (canon == &rt_lst_val_type) return 3;
    if (canon == &rt_lst_ref_type) return 4;
    if (canon == &rt_bytes_type) return 5;
    if (canon == &rt_map_type) return 6;
    return 7;
}

/* The site record for an object of canonical type `canon` made at the line
 * the program is at. Made on first use and published with a release store,
 * so the common case is one acquire load. A user type reaching here (a
 * canonical record handed to rt_alloc, which the compiler does not do) gets
 * no record of its own and is left uncounted. */
static _Thread_local const TypeInfo *hw_last_canon;
static _Thread_local uint32_t        hw_last_loc;
static _Thread_local const TypeInfo *hw_last_site;

static const TypeInfo *hw_site_type_slow(const TypeInfo *canon);

/* The common case first: the same type at the same line as this thread's
 * last allocation of a runtime type, which is what a loop does. */
static inline const TypeInfo *hw_site_type(const TypeInfo *canon) {
    if (canon == hw_last_canon && hw_loc == hw_last_loc) return hw_last_site;
    const TypeInfo *t = hw_site_type_slow(canon);
    hw_last_canon = canon;
    hw_last_loc = hw_loc;
    hw_last_site = t;
    return t;
}

static const TypeInfo *hw_site_type_slow(const TypeInfo *canon) {
    unsigned kind = hw_kind(canon);
    if (kind == 7) return canon;
    uint32_t loc = hw_loc < hw_nlocs ? hw_loc : 0;
    const TypeInfo *_Atomic *cell = &hw_loc_types[(size_t)loc * HW_KINDS + kind];
    const TypeInfo *t = atomic_load_explicit(cell, memory_order_acquire);
    if (t != NULL) return t;
    pthread_mutex_lock(&hw_lock);
    t = atomic_load_explicit(cell, memory_order_relaxed);
    if (t == NULL) {
        uint32_t s = hw_new_slot(canon->name, loc);
        if (s == 0) {
            t = canon;
        } else {
            TypeInfo *n = malloc(sizeof *n);
            if (n == NULL) rt_trap("out of memory");
            *n = *canon;
            n->canon = canon;
            n->site = s;
            n->loc = loc;
            t = n;
        }
        atomic_store_explicit(cell, t, memory_order_release);
    }
    pthread_mutex_unlock(&hw_lock);
    return t;
}

/* ---- allocation and release ------------------------------------------- */

/* The header bits for a new object of `*typ`, which this may swap for the
 * record of the site the program is at, counting it. */
static long hw_alloc(const TypeInfo **typ) {
    const TypeInfo *ty = *typ;
    if (!hw_on) return 1 | ((long)RC_EPOCH_NONE << RC_EPOCH_SHIFT);
    if (ty->site == 0) {
        ty = hw_site_type(ty);
        *typ = ty;
    }
    uint32_t m = hw_now();
    long epoch = hw_epoch_of_new(ty);
    if (ty->site != 0) {
        HwShardHead *sh = hw_my_shard();
        if (hw_shard_shared) pthread_mutex_lock(&hw_shared_lock);
        if (sh->minute != m) hw_fold(sh, m);
        hw_add(&hw_counts(sh)[ty->site].wheel[0][m & (HW_CELLS - 1)], 1);
        if (hw_shard_shared) pthread_mutex_unlock(&hw_shared_lock);
    }
    return 1 | ((long)m << RC_BIRTH_SHIFT) | (epoch << RC_EPOCH_SHIFT);
}

/* An object is about to be freed: take it off its birth minute and add its
 * age to the slot's lifetime histogram. */
static void hw_free(Obj *o) {
    if (!hw_on) return;
    const TypeInfo *ty = o->ty;
    hw_epoch_free(o);
    if (__atomic_load_n(&rt_hw_tracing, __ATOMIC_RELAXED)) hw_trace_free(o);
    if (ty->site == 0) return;
    uint32_t m = (uint32_t)((o->rc & RC_BIRTH) >> RC_BIRTH_SHIFT);
    uint32_t now = hw_now();
    HwShardHead *sh = hw_my_shard();
    if (hw_shard_shared) pthread_mutex_lock(&hw_shared_lock);
    if (sh->minute != now) hw_fold(sh, now);
    HwCount *c = &hw_counts(sh)[ty->site];
    if (m == RC_BIRTH_FOREVER) {
        hw_add(&c->forever, -1);
    } else {
        if (m > now) m = now;
        hw_add(hw_cell(c, now, m), -1);
        hw_addu(&c->life[hw_life_bucket(now - m)], 1);
    }
    if (hw_shard_shared) pthread_mutex_unlock(&hw_shared_lock);
}

void rt_hw_at(uint32_t loc) {
    hw_loc = loc;
}

/* heapwatch.forever(x): move `o` from its birth minute to the slot's
 * forever count, and mark it so its release takes it from there. Exempt
 * from age, and from nothing else. */
void rt_hw_forever(Obj *o) {
    if (o == NULL || o->rc == RC_IMMORTAL || !hw_on) return;
    uint32_t m = (uint32_t)((o->rc & RC_BIRTH) >> RC_BIRTH_SHIFT);
    if (m == RC_BIRTH_FOREVER) return;
    if (o->ty->site != 0) {
        uint32_t now = hw_now();
        HwShardHead *sh = hw_my_shard();
        if (hw_shard_shared) pthread_mutex_lock(&hw_shared_lock);
        if (sh->minute != now) hw_fold(sh, now);
        HwCount *c = &hw_counts(sh)[o->ty->site];
        if (m > now) m = now;
        hw_add(hw_cell(c, now, m), -1);
        hw_add(&c->forever, 1);
        if (hw_shard_shared) pthread_mutex_unlock(&hw_shared_lock);
    }
    o->rc |= RC_BIRTH;
}

/* ---- the suspect log -------------------------------------------------- */

static void hw_log(const char *line) {
    if (hw_path[0] == 0) return;
    if (hw_log_fd < 0) {
        char p[1200];
        snprintf(p, sizeof p, "%.*s.suspects", (int)hw_path_stem, hw_path);
        hw_log_fd = open(p, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0644);
        if (hw_log_fd < 0) return;
    }
    hw_reported = true;
    write_all(hw_log_fd, line, strlen(line));
}

/* "40m", "2h", "3d", "<1m": an age in minutes, the way a report prints it. */
static void hw_age_text(char *buf, size_t cap, uint64_t minutes) {
    if (minutes == 0) {
        snprintf(buf, cap, "<1m");
    } else if (minutes < 120) {
        snprintf(buf, cap, "%um", (unsigned)minutes);
    } else if (minutes < 2880) {
        snprintf(buf, cap, "%uh", (unsigned)(minutes / 60));
    } else {
        snprintf(buf, cap, "%ud", (unsigned)(minutes / 1440));
    }
}

static void hw_stamp(char *buf, size_t cap) {
    time_t t = (time_t)(sys_clock_ns(SYS_CLOCK_REALTIME) / 1000000000);
    struct tm tm;
    gmtime_r(&t, &tm);
    strftime(buf, cap, "%Y-%m-%dT%H:%M:%SZ", &tm);
}

/* ---- adding the shards up ----------------------------------------------
 *
 * One slot's live objects by birth, over every shard: each nonzero cell is
 * an aligned block of minutes [k * 8^L, (k+1) * 8^L), and blocks from
 * different shards are either nested or apart. A block inside a coarser one
 * that some shard holds is merged into it, so a +1 on one shard and the -1
 * for the same object on another always end up in the same block. What is
 * left are disjoint blocks with exact counts. src/tools/heapwatch.rs does
 * the same sum for the report. */
typedef struct {
    uint8_t  level;
    uint32_t k;
    int64_t  count;
} HwBlock;

typedef struct {
    int64_t  live;         /* objects alive */
    int64_t  forever;      /* of them, marked heapwatch.forever */
    uint64_t freed;        /* lifetime histogram total */
    uint32_t p99;          /* minutes: upper bound of the p99 bucket */
    bool     has_old;
    uint32_t oldest_age;   /* minutes, at least: the youngest edge of the
                            * oldest block that still has objects */
    int64_t  older_than;   /* objects certainly older than `limit` */
    uint32_t limit;        /* the age past which the slot is suspect */
} HwSum;

#define HW_MAX_BLOCKS 4096
static HwBlock hw_blocks[HW_MAX_BLOCKS]; /* the tick's scratch: one thread */

static int hw_find_block(int n, unsigned lv, uint32_t k) {
    for (int i = 0; i < n; i++) {
        if (hw_blocks[i].level == lv && hw_blocks[i].k == k) return i;
    }
    return -1;
}

static void hw_sum_slot(uint32_t s, uint32_t now, HwSum *out) {
    memset(out, 0, sizeof *out);
    uint32_t life[HW_LIFE] = {0};
    int n = 0;
    for (uint32_t i = 0; i < hw_head->n_shards; i++) {
        HwShardHead *sh = (HwShardHead *)(void *)(hw_shards + (size_t)i * hw_head->shard_stride);
        if (!__atomic_load_n(&sh->owned, __ATOMIC_RELAXED)) continue;
        uint32_t sm = __atomic_load_n(&sh->minute, __ATOMIC_ACQUIRE);
        HwCount *c = &hw_counts(sh)[s];
        out->forever += hw_ld(&c->forever);
        for (int b = 0; b < HW_LIFE; b++) life[b] += hw_ldu(&c->life[b]);
        for (unsigned lv = 0; lv < HW_LEVELS; lv++) {
            uint32_t K = sm >> (3 * lv);
            for (uint32_t idx = 0; idx < HW_CELLS; idx++) {
                int32_t v = hw_ld(&c->wheel[lv][idx]);
                if (v == 0) continue;
                uint32_t k = K - ((K - idx) & (HW_CELLS - 1));
                int at = hw_find_block(n, lv, k);
                if (at < 0) {
                    if (n == HW_MAX_BLOCKS) continue;
                    at = n++;
                    hw_blocks[at].level = (uint8_t)lv;
                    hw_blocks[at].k = k;
                    hw_blocks[at].count = 0;
                }
                hw_blocks[at].count += v;
            }
        }
    }
    /* Merge each block into the nearest coarser block that contains it. */
    for (unsigned lv = 0; lv + 1 < HW_LEVELS; lv++) {
        for (int i = 0; i < n; i++) {
            if (hw_blocks[i].level != lv || hw_blocks[i].count == 0) continue;
            for (unsigned up = lv + 1; up < HW_LEVELS; up++) {
                int j = hw_find_block(n, up, hw_blocks[i].k >> (3 * (up - lv)));
                if (j >= 0) {
                    hw_blocks[j].count += hw_blocks[i].count;
                    hw_blocks[i].count = 0;
                    break;
                }
            }
        }
    }
    for (int b = 0; b < HW_LIFE; b++) out->freed += life[b];
    if (out->freed > 0) {
        uint64_t want = (out->freed * 99 + 99) / 100, seen = 0;
        for (int b = 0; b < HW_LIFE; b++) {
            seen += life[b];
            if (seen >= want) {
                out->p99 = b == 0 ? 1 : 1u << b;
                break;
            }
        }
    }
    const HwSlot *sl = &hw_slots[s];
    if (sl->flags & HW_SLOT_LIMIT) {
        out->limit = sl->limit;
    } else {
        uint64_t l = (uint64_t)out->p99 * hw_head->factor;
        out->limit = l > hw_head->floor_min ? (uint32_t)(l < 0xffffffffu ? l : 0xffffffffu)
                                            : hw_head->floor_min;
    }
    out->live = out->forever;
    for (int i = 0; i < n; i++) {
        int64_t v = hw_blocks[i].count;
        if (v <= 0) continue;
        out->live += v;
        uint64_t span = 1ull << (3 * hw_blocks[i].level);
        uint64_t last = ((uint64_t)hw_blocks[i].k + 1) * span - 1;
        uint32_t young = last >= now ? 0 : (uint32_t)(now - last);
        if (!out->has_old || young > out->oldest_age) {
            out->oldest_age = young;
            out->has_old = true;
        }
        if (young > out->limit) out->older_than += v;
    }
}

/* Once a minute: which slots are suspects now? A slot is one when its
 * oldest survivor is older than max(floor, factor x its own p99 lifetime),
 * or than the limit M31_AGE gave it. A slot with no lifetime yet -- nothing
 * of it was ever freed -- is "never freed", not a suspect, unless M31_AGE
 * gave it a limit. A line goes to the log when a slot becomes a suspect and
 * each time its oldest survivor doubles in age again. */
static void hw_scan(uint32_t now) {
    uint32_t n_slots = __atomic_load_n(&hw_head->n_slots, __ATOMIC_ACQUIRE);
    for (uint32_t s = 1; s < n_slots; s++) {
        HwSlot *sl = &hw_slots[s];
        if (sl->flags & HW_SLOT_FOREVER) continue;
        HwSum sum;
        hw_sum_slot(s, now, &sum);
        if (sum.live - sum.forever <= 0 || !sum.has_old) continue;
        if (sum.freed == 0 && !(sl->flags & HW_SLOT_LIMIT)) continue;
        if (sum.oldest_age <= sum.limit) continue;
        uint32_t level = 1;
        for (uint64_t t = (uint64_t)sum.limit * 2; t > 0 && sum.oldest_age > t && level < 40; t *= 2) level++;
        if (level <= sl->level) continue;
        sl->level = level;
        char line[700], when[32], age[16], lim[16], p99[16];
        hw_stamp(when, sizeof when);
        hw_age_text(age, sizeof age, sum.oldest_age);
        hw_age_text(lim, sizeof lim, sum.limit);
        if (sum.freed > 0) {
            hw_age_text(p99, sizeof p99, sum.p99 <= 1 ? 0 : sum.p99);
        } else {
            snprintf(p99, sizeof p99, "none");
        }
        snprintf(line, sizeof line,
                 "%s SUSPECT %s %s:%u %lld alive > %s (limit %s, p99 lifetime %s)\n",
                 when, hw_str(sl->type_off), hw_str(sl->file_off), sl->line,
                 (long long)sum.older_than, age, lim, p99);
        hw_log(line);
        hw_trace_suspect(s, now, sum.limit);
    }
}

/* ---- the tick ----------------------------------------------------------
 *
 * Called by the scheduler's monitor thread on every poll (runtime/
 * scheduler.c, monitor_main), which every compiled program runs: it moves
 * the program's minute on, applies the control page once a second, and
 * scans for suspects once a minute. One load and a compare when there is
 * nothing to do. */
static uint64_t hw_last_second_ns;

void rt_hw_tick(void) {
    if (!hw_on) return;
    uint64_t now_ns = (uint64_t)sys_clock_ns(SYS_CLOCK_MONOTONIC);
    if (now_ns - hw_last_second_ns < 1000000000ull && hw_minute_ns >= 1000000000ull) {
        /* Between seconds nothing but the minute can change, and with a
         * real minute that cannot either. */
        return;
    }
    uint64_t m64 = (now_ns - hw_start_ns) / hw_minute_ns;
    uint32_t m = m64 >= RC_BIRTH_MAX - 1 ? RC_BIRTH_MAX - 1 : (uint32_t)m64;
    if (now_ns - hw_last_second_ns >= 1000000000ull) {
        hw_last_second_ns = now_ns;
        __atomic_store_n(&hw_head->tick_unix, sys_clock_ns(SYS_CLOCK_REALTIME) / 1000000000,
                         __ATOMIC_RELAXED);
        hw_control();
    }
    if (m != hw_now()) {
        __atomic_store_n(&hw_minute_now, m, __ATOMIC_RELAXED);
        __atomic_store_n(&hw_head->minute, m, __ATOMIC_RELAXED);
        hw_scan(m);
    }
}

/* ---- start and exit ----------------------------------------------------- */

static uint32_t hw_env_u32(const char *name, uint32_t dflt, uint32_t lo, uint32_t hi) {
    const char *v = getenv(name);
    if (v == NULL || *v == 0) return dflt;
    uint64_t n = 0;
    for (const char *p = v; *p; p++) {
        if (*p < '0' || *p > '9' || n > 0xffffffffull) {
            hw_warn(name, v);
            return dflt;
        }
        n = n * 10 + (uint64_t)(*p - '0');
    }
    if (n < lo || n > hi) {
        hw_warn(name, v);
        return dflt;
    }
    return (uint32_t)n;
}

/* Remove ledgers of this program left by processes that are gone, keeping
 * the HW_KEEP_LEDGERS newest: a crash or a redeploy leaves its ledger on
 * disk for the next look, and this is what keeps them from piling up. */
static void hw_prune(const char *dir, const char *prog) {
    DIR *d = opendir(dir);
    if (d == NULL) return;
    typedef struct { char name[300]; int64_t mtime; long pid; } Old;
    Old olds[64];
    int n = 0;
    size_t pn = strlen(prog);
    struct dirent *e;
    while ((e = readdir(d)) != NULL) {
        const char *nm = e->d_name;
        size_t len = strlen(nm);
        if (len <= pn + 8 || strncmp(nm, prog, pn) != 0 || nm[pn] != '.') continue;
        if (strcmp(nm + len - 7, ".ledger") != 0) continue;
        long pid = 0;
        const char *p = nm + pn + 1;
        for (; *p >= '0' && *p <= '9'; p++) pid = pid * 10 + (*p - '0');
        if (p != nm + len - 7 || pid <= 0) continue;
        char full[1400];
        snprintf(full, sizeof full, "%s/%s", dir, nm);
        struct stat st;
        if (stat(full, &st) != 0) continue;
        Old o;
        snprintf(o.name, sizeof o.name, "%s", nm);
        o.mtime = (int64_t)st.st_mtime;
        o.pid = pid;
        if (n < 64) {
            olds[n++] = o;
        } else {
            /* Keep the 64 newest seen; replace the oldest. */
            int oldest = 0;
            for (int i = 1; i < n; i++) if (olds[i].mtime < olds[oldest].mtime) oldest = i;
            if (o.mtime > olds[oldest].mtime) olds[oldest] = o;
        }
    }
    closedir(d);
    /* Newest first. */
    for (int i = 1; i < n; i++) {
        Old o = olds[i];
        int j = i;
        while (j > 0 && olds[j - 1].mtime < o.mtime) {
            olds[j] = olds[j - 1];
            j--;
        }
        olds[j] = o;
    }
    for (int i = HW_KEEP_LEDGERS; i < n; i++) {
        if (olds[i].pid == (long)getpid()) continue;
        if (kill((pid_t)olds[i].pid, 0) == 0 || errno != ESRCH) continue;
        char full[1400];
        size_t stem = strlen(olds[i].name) - 7;
        static const char *const exts[] = { ".ledger", ".suspects", ".trace" };
        for (int x = 0; x < 3; x++) {
            snprintf(full, sizeof full, "%s/%.*s%s", dir, (int)stem, olds[i].name, exts[x]);
            unlink(full);
        }
    }
}

static void hw_exit(void);

/* Start heap watch: read the environment, make and map the ledger, and
 * register the program's slots. Called once, from the emitted main, before
 * the program's first statement. */
void rt_hw_program(const RtHwLoc *locs, uint32_t nlocs, const TypeInfo *const *slots,
                   uint32_t nslots) {
    const char *off = getenv("M31_HEAPWATCH");
    if (off != NULL && strcmp(off, "0") == 0) return;
    if (hw_on) return;
    hw_locs = locs;
    hw_nlocs = nlocs;
    hw_keep = getenv("M31_HEAPWATCH_KEEP") != NULL && strcmp(getenv("M31_HEAPWATCH_KEEP"), "0") != 0;

    uint32_t ms = hw_env_u32("M31_HEAPWATCH_MINUTE_MS", 60000, 1, 86400000);
    hw_minute_ns = (uint64_t)ms * 1000000ull;
    hw_start_ns = (uint64_t)sys_clock_ns(SYS_CLOCK_MONOTONIC);
    hw_last_second_ns = hw_start_ns;

    long ncpu = sysconf(_SC_NPROCESSORS_ONLN);
    uint32_t carriers = hw_env_u32("LANG_NUM_CARRIERS", ncpu > 0 ? (uint32_t)ncpu : 1, 1, 4096);
    uint32_t n_shards = carriers + 8;
    if (n_shards > HW_MAX_SHARDS) n_shards = HW_MAX_SHARDS;
    uint32_t max_slots = nslots + 1 + HW_DYNAMIC_SLOTS;

    size_t page = HW_PAGE;
    size_t off_slots = 2 * page;
    size_t off_traced = off_slots + (size_t)max_slots * sizeof(HwSlot);
    size_t off_strings = off_traced + (((size_t)max_slots + 7) & ~(size_t)7);
    size_t off_epochs = off_strings + HW_STRINGS;
    size_t off_shards = (off_epochs + HW_EPOCH_ROWS * sizeof(HwEpochRow) + page - 1) & ~(page - 1);
    size_t stride = (sizeof(HwShardHead) + (size_t)max_slots * sizeof(HwCount) + page - 1) & ~(page - 1);
    size_t size = off_shards + (size_t)n_shards * stride;

    /* The program's name, for the file and the report. */
    const char *prog = rt_argv_saved != NULL && rt_argc_saved > 0 ? rt_argv_saved[0] : "m31";
    const char *slash = strrchr(prog, '/');
    if (slash != NULL) prog = slash + 1;
    char name[128];
    snprintf(name, sizeof name, "%s", prog);
    for (char *c = name; *c; c++) {
        if (*c == '/' || *c == ' ') *c = '_';
    }

    char dir[900];
    const char *want = getenv("M31_HEAPWATCH_DIR");
    if (want != NULL && *want != 0) {
        snprintf(dir, sizeof dir, "%s", want);
    } else {
        const char *tmp = getenv("TMPDIR");
        if (tmp == NULL || *tmp == 0) tmp = "/tmp";
        size_t tl = strlen(tmp);
        while (tl > 1 && tmp[tl - 1] == '/') tl--;
        snprintf(dir, sizeof dir, "%.*s/m31-heapwatch-%u", (int)tl, tmp, (unsigned)getuid());
    }
    mkdir(dir, 0700);
    hw_prune(dir, name);

    int fd = -1;
    snprintf(hw_path, sizeof hw_path, "%.880s/%.120s.%ld.ledger", dir, name, (long)getpid());
    hw_path_stem = strlen(hw_path) - 7;
    fd = open(hw_path, O_RDWR | O_CREAT | O_TRUNC | O_CLOEXEC, 0644);
    if (fd >= 0 && ftruncate(fd, (off_t)size) != 0) {
        close(fd);
        unlink(hw_path);
        fd = -1;
    }
    void *base;
    if (fd >= 0) {
        base = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        close(fd);
        if (base == MAP_FAILED) unlink(hw_path);
    } else {
        base = MAP_FAILED;
    }
    if (base == MAP_FAILED) {
        /* No file: count in memory anyway, so epoch close and the program's
         * own behaviour are the same; there is just nothing to read later. */
        hw_path[0] = 0;
        base = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANON, -1, 0);
        if (base == MAP_FAILED) return;
    }
    hw_base = base;
    hw_size = size;
    hw_head = (HwHeader *)(void *)(hw_base + page);
    hw_slots = (HwSlot *)(void *)(hw_base + off_slots);
    hw_traced = hw_base + off_traced;
    hw_strings = (char *)(hw_base + off_strings);
    hw_epochs = (HwEpochRow *)(void *)(hw_base + off_epochs);
    hw_shards = hw_base + off_shards;

    hw_loc_types = calloc((size_t)(nlocs > 0 ? nlocs : 1) * HW_KINDS, sizeof *hw_loc_types);
    if (hw_loc_types == NULL) rt_trap("out of memory");

    HwHeader *h = hw_head;
    h->version = HW_VERSION;
    h->page = (uint32_t)page;
    h->pid = (uint64_t)getpid();
    h->start_unix = sys_clock_ns(SYS_CLOCK_REALTIME) / 1000000000;
    h->tick_unix = h->start_unix;
    h->minute_ms = ms;
    h->n_shards = n_shards;
    h->max_slots = max_slots;
    h->n_slots = 1;
    h->count_size = (uint32_t)sizeof(HwCount);
    h->off_slots = off_slots;
    h->off_traced = off_traced;
    h->off_strings = off_strings;
    h->off_epochs = off_epochs;
    h->off_shards = off_shards;
    h->shard_stride = stride;
    h->strings_cap = HW_STRINGS;
    h->strings_used = 1;
    h->floor_min = hw_env_u32("M31_HEAPWATCH_MIN", 10, 1, 100000000);
    h->factor = hw_env_u32("M31_HEAPWATCH_FACTOR", 10, 1, 100000);
    h->running = 1;
    h->trace_depth = hw_env_u32("M31_HEAPWATCH_DEPTH", 3, 1, 64);
    hw_edges_max = hw_env_u32("M31_HEAPWATCH_EDGES", 1u << 20, 16, 1u << 28);
    hw_edge_sites = calloc(max_slots, 1);
    snprintf(h->program, sizeof h->program, "%s", name);
    hw_control_init();

    /* A test build traps on an epoch that closes with leaks, so a test
     * fails on any leaked cycle: the corpus's build, -DRC_DEBUG, unless
     * M31_HEAPWATCH_TRAP=0; any build with M31_HEAPWATCH_TRAP=1. */
    const char *trap = getenv("M31_HEAPWATCH_TRAP");
#ifdef RC_DEBUG
    hw_trap_on_leak = trap == NULL || strcmp(trap, "0") != 0;
#else
    hw_trap_on_leak = trap != NULL && strcmp(trap, "1") == 0;
#endif

    hw_each_entry(getenv("M31_AGE"), hw_parse_rule);
    const char *conf = getenv("M31_HEAPWATCH_CONF");
    if (conf != NULL && *conf != 0) hw_read_conf(conf);
    hw_each_entry(getenv("M31_HEAPWATCH_TRACE"), hw_add_trace_spec);

    pthread_mutex_lock(&hw_lock);
    for (uint32_t i = 0; i < nslots; i++) {
        const TypeInfo *t = slots[i];
        /* The compiler numbers its site records 1, 2, ... in this order. */
        if (t->site != i + 1) rt_trap("internal: heap watch slot table out of order");
        uint32_t s = hw_new_slot(t->name, t->loc);
        if (s != t->site) rt_trap("internal: heap watch slot table out of order");
    }
    pthread_mutex_unlock(&hw_lock);
    /* The magic last, so a reader never sees a half-made header. */
    __atomic_store_n(&h->magic[0], 'M', __ATOMIC_RELAXED);
    memcpy(h->magic + 1, "31HWLDG", 7);
    hw_on = true;
    atexit(hw_exit);
}

/* A clean exit with nothing reported leaves nothing behind: the files are
 * for a program that crashed, was killed, or has something to say. Not run
 * on a trap or a signal, which is exactly when they must stay. */
static void hw_exit(void) {
    if (!hw_on || hw_head == NULL) return;
    __atomic_store_n(&hw_head->running, 0, __ATOMIC_RELAXED);
    if (hw_path[0] == 0 || hw_reported || hw_keep) return;
    char p[1200];
    unlink(hw_path);
    snprintf(p, sizeof p, "%.*s.suspects", (int)hw_path_stem, hw_path);
    unlink(p);
    snprintf(p, sizeof p, "%.*s.trace", (int)hw_path_stem, hw_path);
    unlink(p);
}

/* ---- trace: who holds it (phase 2) --------------------------------------
 *
 * docs/heap-watch-design.md, "Trace: who holds it". For the slots trace is
 * on for, every reference store records an EDGE -- value <- holder.slot --
 * and every overwrite, removal and release takes it out again. Nothing is
 * walked: the table only ever holds what stores told it, and a report reads
 * the table.
 *
 * An edge is keyed by (holder, where, value) for a collection's element,
 * key or value -- a List can hold the same object twice, so those count --
 * and by (holder, where) alone for a field, which holds one value at a time:
 * a store into a field replaces whatever edge the field had, so the old
 * value's edge goes without anybody having to know what it was.
 *
 * Every operation here takes hw_trace_lock: the table is shared by every
 * thread, and none of it runs unless trace is on for something. With trace
 * off (rt_hw_tracing == 0) a store pays one load and an untaken branch. */

/* HW_WHERE_ELEM, _KEY and _VALUE are defined at the top of rt.c, which
 * stores through them before this file. */
#define HW_IS_FIELD(w) ((w) < 0xFFFFFFF0u)

typedef struct {
    Obj     *h;          /* the holder */
    Obj     *v;          /* the value it holds; NULL on a free entry */
    uint32_t where;      /* a field's offset, or HW_WHERE_* */
    uint32_t count;      /* a collection can hold one value many times */
    int32_t  hnext, hprev, vnext, vprev; /* the holder's and the value's lists */
} HwEdge;

/* Open addressing, keyed by an object's address, linear probing with
 * backward-shift deletion so there are no tombstones. Maps an object to
 * the head of its list of edges (as a holder, and as a value), or an edge
 * key to an edge. */
typedef struct {
    uintptr_t *keys;  /* 0 is empty */
    uintptr_t *keys2; /* second key word, for the edge index */
    int32_t   *vals;
    uint32_t   cap;   /* a power of two, or 0 */
    uint32_t   len;
} HwMap;

static pthread_mutex_t hw_trace_lock = PTHREAD_MUTEX_INITIALIZER;
uint8_t                rt_hw_tracing;      /* read inline by emitted stores */
static uint32_t        hw_traced_slots;    /* how many slots trace is on for */
static HwEdge         *hw_edges;
static uint32_t        hw_edges_cap, hw_edges_used, hw_edge_free = UINT32_MAX;
static uint32_t        hw_edges_max = 1u << 20;
static HwMap           hw_by_key, hw_by_holder, hw_by_value;
static int             hw_trace_fd = -1;
/* Per slot: whether an edge ever named an object of it, as holder or as
 * value. Set under hw_trace_lock and never cleared; read without it, so a
 * store or a release of an object no edge can name -- most of them, even
 * with trace on -- does not take the lock at all. */
static uint8_t        *hw_edge_sites;

static inline bool hw_may_have_edges(const Obj *o) {
    return hw_edge_sites != NULL && o->ty->site != 0 &&
           __atomic_load_n(&hw_edge_sites[o->ty->site], __ATOMIC_RELAXED) != 0;
}

static uint64_t hw_mix(uintptr_t a, uintptr_t b) {
    uint64_t x = (uint64_t)a * 0x9E3779B97F4A7C15ull ^ (uint64_t)b * 0xC2B2AE3D27D4EB4Full;
    return x ^ (x >> 29);
}

static void hw_map_grow(HwMap *m) {
    uint32_t cap = m->cap == 0 ? 256 : m->cap * 2;
    uintptr_t *k = calloc(cap, sizeof *k), *k2 = calloc(cap, sizeof *k2);
    int32_t *v = calloc(cap, sizeof *v);
    if (k == NULL || k2 == NULL || v == NULL) rt_trap("out of memory");
    for (uint32_t i = 0; i < m->cap; i++) {
        if (m->keys[i] == 0) continue;
        uint32_t j = (uint32_t)hw_mix(m->keys[i], m->keys2[i]) & (cap - 1);
        while (k[j] != 0) j = (j + 1) & (cap - 1);
        k[j] = m->keys[i];
        k2[j] = m->keys2[i];
        v[j] = m->vals[i];
    }
    free(m->keys);
    free(m->keys2);
    free(m->vals);
    m->keys = k;
    m->keys2 = k2;
    m->vals = v;
    m->cap = cap;
}

/* The slot of (a, b), or of the empty place it would go in. */
static uint32_t hw_map_slot(const HwMap *m, uintptr_t a, uintptr_t b) {
    uint32_t i = (uint32_t)hw_mix(a, b) & (m->cap - 1);
    while (m->keys[i] != 0 && (m->keys[i] != a || m->keys2[i] != b)) i = (i + 1) & (m->cap - 1);
    return i;
}

static int32_t hw_map_get(const HwMap *m, uintptr_t a, uintptr_t b) {
    if (m->cap == 0) return -1;
    uint32_t i = hw_map_slot(m, a, b);
    return m->keys[i] == 0 ? -1 : m->vals[i];
}

static void hw_map_put(HwMap *m, uintptr_t a, uintptr_t b, int32_t v) {
    if (m->cap == 0 || (m->len + 1) * 4 >= m->cap * 3) hw_map_grow(m);
    uint32_t i = hw_map_slot(m, a, b);
    if (m->keys[i] == 0) {
        m->keys[i] = a;
        m->keys2[i] = b;
        m->len++;
    }
    m->vals[i] = v;
}

static void hw_map_del(HwMap *m, uintptr_t a, uintptr_t b) {
    if (m->cap == 0) return;
    uint32_t i = hw_map_slot(m, a, b);
    if (m->keys[i] == 0) return;
    m->len--;
    /* Backward shift: pull later entries of the run into the hole. */
    uint32_t j = i;
    for (;;) {
        m->keys[i] = 0;
        for (;;) {
            j = (j + 1) & (m->cap - 1);
            if (m->keys[j] == 0) return;
            uint32_t home = (uint32_t)hw_mix(m->keys[j], m->keys2[j]) & (m->cap - 1);
            /* Stay if home lies cyclically in (i, j]. */
            bool stays = i <= j ? (i < home && home <= j) : (i < home || home <= j);
            if (!stays) break;
        }
        m->keys[i] = m->keys[j];
        m->keys2[i] = m->keys2[j];
        m->vals[i] = m->vals[j];
        i = j;
    }
}

static uintptr_t hw_key_value(Obj *v, uint32_t where) {
    return HW_IS_FIELD(where) ? 0 : (uintptr_t)v;
}

/* The edge key's second word: where, and for a collection the value too. */
static uintptr_t hw_key2(Obj *v, uint32_t where) {
    return (uintptr_t)where ^ (hw_key_value(v, where) << 1);
}

static void hw_edge_unlink(int32_t e) {
    HwEdge *x = &hw_edges[e];
    if (x->hprev >= 0) {
        hw_edges[x->hprev].hnext = x->hnext;
    } else if (x->hnext >= 0) {
        hw_map_put(&hw_by_holder, (uintptr_t)x->h, 0, x->hnext);
    } else {
        hw_map_del(&hw_by_holder, (uintptr_t)x->h, 0);
    }
    if (x->hnext >= 0) hw_edges[x->hnext].hprev = x->hprev;
    if (x->vprev >= 0) {
        hw_edges[x->vprev].vnext = x->vnext;
    } else if (x->vnext >= 0) {
        hw_map_put(&hw_by_value, (uintptr_t)x->v, 0, x->vnext);
    } else {
        hw_map_del(&hw_by_value, (uintptr_t)x->v, 0);
    }
    if (x->vnext >= 0) hw_edges[x->vnext].vprev = x->vprev;
}

static void hw_edge_remove(int32_t e) {
    HwEdge *x = &hw_edges[e];
    hw_map_del(&hw_by_key, (uintptr_t)x->h, hw_key2(x->v, x->where));
    hw_edge_unlink(e);
    x->v = NULL;
    x->hnext = (int32_t)hw_edge_free;
    hw_edge_free = (uint32_t)e;
    hw_head->edges--;
}

static void hw_edge_add(Obj *h, Obj *v, uint32_t where) {
    int32_t e = hw_map_get(&hw_by_key, (uintptr_t)h, hw_key2(v, where));
    if (e >= 0) {
        hw_edges[e].count++;
        return;
    }
    if (hw_edge_free != UINT32_MAX) {
        e = (int32_t)hw_edge_free;
        hw_edge_free = (uint32_t)hw_edges[e].hnext;
    } else {
        if (hw_edges_used == hw_edges_cap) {
            if (hw_edges_cap >= hw_edges_max) {
                hw_head->edges_dropped++;
                return;
            }
            uint32_t cap = hw_edges_cap == 0 ? 1024 : hw_edges_cap * 2;
            if (cap > hw_edges_max) cap = hw_edges_max;
            HwEdge *bigger = realloc(hw_edges, (size_t)cap * sizeof *bigger);
            if (bigger == NULL) {
                hw_head->edges_dropped++;
                return;
            }
            hw_edges = bigger;
            hw_edges_cap = cap;
        }
        e = (int32_t)hw_edges_used++;
    }
    HwEdge *x = &hw_edges[e];
    x->h = h;
    x->v = v;
    x->where = where;
    x->count = 1;
    if (hw_edge_sites != NULL) {
        __atomic_store_n(&hw_edge_sites[h->ty->site], 1, __ATOMIC_RELAXED);
        __atomic_store_n(&hw_edge_sites[v->ty->site], 1, __ATOMIC_RELAXED);
    }
    x->hprev = x->vprev = -1;
    x->hnext = hw_map_get(&hw_by_holder, (uintptr_t)h, 0);
    x->vnext = hw_map_get(&hw_by_value, (uintptr_t)v, 0);
    if (x->hnext >= 0) hw_edges[x->hnext].hprev = e;
    if (x->vnext >= 0) hw_edges[x->vnext].vprev = e;
    hw_map_put(&hw_by_holder, (uintptr_t)h, 0, e);
    hw_map_put(&hw_by_value, (uintptr_t)v, 0, e);
    hw_map_put(&hw_by_key, (uintptr_t)h, hw_key2(v, where), e);
    hw_head->edges++;
}

static bool hw_is_traced(const Obj *v) {
    return v != NULL && v->rc != RC_IMMORTAL && v->ty->site != 0 &&
           __atomic_load_n(&hw_traced[v->ty->site], __ATOMIC_RELAXED) != 0;
}

/* `v` is now held by `h` at `where`, replacing what a field held. */
static void hw_trace_store(Obj *h, Obj *v, uint32_t where) {
    /* Nothing to add, and no old edge this slot could have. */
    if (!hw_is_traced(v) && !(HW_IS_FIELD(where) && hw_may_have_edges(h))) return;
    pthread_mutex_lock(&hw_trace_lock);
    if (HW_IS_FIELD(where)) {
        int32_t old = hw_map_get(&hw_by_key, (uintptr_t)h, hw_key2(NULL, where));
        if (old >= 0) hw_edge_remove(old);
    }
    if (hw_is_traced(v)) hw_edge_add(h, v, where);
    pthread_mutex_unlock(&hw_trace_lock);
}

/* `h` no longer holds one `v` at `where` (a collection), or anything at
 * field `where` (v is ignored). */
static void hw_trace_unstore(Obj *h, Obj *v, uint32_t where) {
    if (!hw_may_have_edges(h)) return;
    pthread_mutex_lock(&hw_trace_lock);
    int32_t e = hw_map_get(&hw_by_key, (uintptr_t)h, hw_key2(v, where));
    if (e >= 0 && --hw_edges[e].count == 0) hw_edge_remove(e);
    pthread_mutex_unlock(&hw_trace_lock);
}

/* `h` holds nothing any more, as a holder; or is being freed. */
static void hw_trace_drop_holder(Obj *h) {
    if (!hw_may_have_edges(h)) return;
    pthread_mutex_lock(&hw_trace_lock);
    int32_t e;
    while ((e = hw_map_get(&hw_by_holder, (uintptr_t)h, 0)) >= 0) hw_edge_remove(e);
    pthread_mutex_unlock(&hw_trace_lock);
}

/* An object is about to be freed: no edge may name it either way. */
static void hw_trace_free(Obj *o) {
    if (!hw_may_have_edges(o)) return;
    pthread_mutex_lock(&hw_trace_lock);
    int32_t e;
    while ((e = hw_map_get(&hw_by_holder, (uintptr_t)o, 0)) >= 0) hw_edge_remove(e);
    while ((e = hw_map_get(&hw_by_value, (uintptr_t)o, 0)) >= 0) hw_edge_remove(e);
    pthread_mutex_unlock(&hw_trace_lock);
}

static void hw_trace_slot(uint32_t s, uint8_t on) {
    uint8_t was = __atomic_exchange_n(&hw_traced[s], on, __ATOMIC_RELAXED);
    if ((was != 0) == (on != 0)) return;
    uint32_t n = on ? __atomic_add_fetch(&hw_traced_slots, 1, __ATOMIC_RELAXED)
                    : __atomic_sub_fetch(&hw_traced_slots, 1, __ATOMIC_RELAXED);
    __atomic_store_n(&rt_hw_tracing, n != 0, __ATOMIC_RELAXED);
    if (on) return;
    /* Off: forget the edges to its objects, so the table only holds what a
     * report could still print. */
    pthread_mutex_lock(&hw_trace_lock);
    for (uint32_t e = 0; e < hw_edges_used; e++) {
        if (hw_edges[e].v != NULL && hw_edges[e].v->ty->site == s) hw_edge_remove((int32_t)e);
    }
    pthread_mutex_unlock(&hw_trace_lock);
}

/* Everything a store has to do beyond the store itself: the slow path of
 * rt_hw_store (rt.h), which the fast path takes when trace is on for
 * anything, or when the value must be adopted into an older epoch. */
void rt_hw_store_slow(Obj *h, Obj *v, uint32_t where) {
    if (!hw_on || v == NULL) return;
    hw_epoch_store(h, v);
    if (__atomic_load_n(&rt_hw_tracing, __ATOMIC_RELAXED)) hw_trace_store(h, v, where);
}

/* Element `i` of List or Array `o` is about to be `v`. */
void rt_hw_store_elem_slow(Obj *o, int64_t i, Obj *v) {
    if (!hw_on || v == NULL) return;
    hw_epoch_store(o, v);
    if (!__atomic_load_n(&rt_hw_tracing, __ATOMIC_RELAXED)) return;
    int64_t n = rt_len_of(o);
    if (i >= 0 && i < n) {
        int64_t *d = rt_is_list(o) ? ((Lst *)o)->data : ((Arr *)o)->data;
        Obj *old = (Obj *)(intptr_t)d[i];
        if (old != NULL) hw_trace_unstore(o, old, HW_WHERE_ELEM);
    }
    hw_trace_store(o, v, HW_WHERE_ELEM);
}

/* Field `where` of `h` was cleared without a store (a payload taken out). */
void rt_hw_unstore(Obj *h, uint32_t where) {
    if (!hw_on || !__atomic_load_n(&rt_hw_tracing, __ATOMIC_RELAXED)) return;
    hw_trace_unstore(h, NULL, where);
}

/* A new object `o` holds what it holds already -- a clone, a copy for a
 * `const` -- with no store the hook saw. Its references, field by field. */
void rt_hw_fresh(Obj *o) {
    if (!hw_on || !__atomic_load_n(&rt_hw_tracing, __ATOMIC_RELAXED)) return;
    const TypeInfo *ty = o->ty;
    int64_t tag = ty->nfields > 0 && ty->fields[0].tag >= 0 ? *(int64_t *)(void *)(o + 1) : -1;
    for (uint32_t i = 0; i < ty->nfields; i++) {
        const RtField *f = &ty->fields[i];
        if (f->tag >= 0 && f->tag != tag) continue;
        Obj *v = *(Obj **)(void *)((char *)o + f->offset);
        if (v != NULL) hw_trace_store(o, v, f->offset);
    }
}

/* A collection made already full -- a clone, a slice, a `[v; n]`, a copy
 * for a `const` -- holds its elements with no store the hook saw. */
static void hw_fresh_collection(Obj *o) {
    if (!hw_on) return;
    const TypeInfo *c = o->ty->canon;
    if (c == &rt_arr_ref_type || c == &rt_lst_ref_type) {
        int64_t n = rt_len_of(o);
        int64_t *d = rt_is_list(o) ? ((Lst *)o)->data : ((Arr *)o)->data;
        for (int64_t i = 0; i < n; i++) {
            Obj *v = (Obj *)(intptr_t)d[i];
            if (v != NULL) hw_trace_store(o, v, HW_WHERE_ELEM);
        }
    } else if (c == &rt_map_type) {
        Map *m = (Map *)o;
        for (int64_t i = 0; i < m->cap; i++) {
            if (m->slots[i].state != SLOT_FULL) continue;
            if (m->key_is_ref) hw_trace_store(o, (Obj *)(intptr_t)m->slots[i].k, HW_WHERE_KEY);
            if (m->val_is_ref) hw_trace_store(o, (Obj *)(intptr_t)m->slots[i].v, HW_WHERE_VALUE);
        }
    }
}

/* ---- trace's report --------------------------------------------------- */

static void hw_trace_write(const char *text) {
    if (hw_path[0] == 0) return;
    if (hw_trace_fd < 0) {
        char p[1200];
        snprintf(p, sizeof p, "%.*s.trace", (int)hw_path_stem, hw_path);
        hw_trace_fd = open(p, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0644);
        if (hw_trace_fd < 0) return;
    }
    write_all(hw_trace_fd, text, strlen(text));
}

/* "Session.conn", "List element", "Map value": the slot `where` of `h`. */
static void hw_slot_name(char *buf, size_t cap, const Obj *h, uint32_t where) {
    const TypeInfo *ty = h->ty;
    const char *tn = ty->name != NULL ? ty->name : "?";
    if (where == HW_WHERE_ELEM) {
        snprintf(buf, cap, "%s element", tn);
    } else if (where == HW_WHERE_KEY) {
        snprintf(buf, cap, "%s key", tn);
    } else if (where == HW_WHERE_VALUE) {
        snprintf(buf, cap, "%s value", tn);
    } else {
        int64_t tag = ty->nfields > 0 && ty->fields[0].tag >= 0
                          ? *(const int64_t *)(const void *)(h + 1) : -1;
        for (uint32_t i = 0; i < ty->nfields; i++) {
            const RtField *f = &ty->fields[i];
            if (f->offset == where && (f->tag < 0 || f->tag == tag)) {
                snprintf(buf, cap, "%s.%s", tn, f->name);
                return;
            }
        }
        snprintf(buf, cap, "%s+%u", tn, where);
    }
}

static void hw_site_text(char *buf, size_t cap, const Obj *o) {
    uint32_t s = o->ty->site;
    if (s == 0) {
        snprintf(buf, cap, "?");
        return;
    }
    snprintf(buf, cap, "%s:%u", hw_str(hw_slots[s].file_off), hw_slots[s].line);
}

/* One value's holders, as lines, up to `depth` levels: a holder that is
 * itself traced is followed to its own holders; a cycle ends the line with
 * "<- cycle"; a holder trace is off for is turned on (it will show next
 * time), "--depth" levels up from the object asked about. Called with
 * hw_trace_lock held, so every object named is alive: its free removes its
 * edges first, under the same lock. */
#define HW_PATH_MAX 16
static void hw_paths(char *out, size_t cap, size_t *len, Obj *v, Obj **path, int at, int depth) {
    int32_t e = hw_map_get(&hw_by_value, (uintptr_t)v, 0);
    if (e < 0) {
        /* The end of the line: what holds this holds it from a local -- a
         * root -- or stored it before trace began. */
        *len += (size_t)snprintf(out + *len, cap - *len,
                                 "  %*s(no recorded holder: a local, or stored before trace began)\n",
                                 2 * at, "");
        return;
    }
    for (; e >= 0 && *len + 300 < cap; e = hw_edges[e].vnext) {
        HwEdge *x = &hw_edges[e];
        char name[200], site[200];
        hw_slot_name(name, sizeof name, x->h, x->where);
        hw_site_text(site, sizeof site, x->h);
        bool cycle = false;
        for (int i = 0; i <= at; i++) cycle = cycle || path[i] == x->h;
        *len += (size_t)snprintf(out + *len, cap - *len, "  %*sheld by  %-24s %s%s\n", 2 * at, "",
                                 name, site, cycle ? "  <- cycle" : "");
        if (cycle || at + 1 >= HW_PATH_MAX) continue;
        if (!hw_is_traced(x->h)) {
            if (at + 1 < depth && x->h->ty->site != 0) {
                /* --depth: trace the holder's site from now on (turning a
                 * slot on takes no lock), so its own holders show next. */
                hw_trace_slot(x->h->ty->site, 1);
                *len += (size_t)snprintf(out + *len, cap - *len,
                                         "  %*s(untraced holder: trace is now on for %s %s)\n",
                                         2 * at + 2, "", x->h->ty->name, site);
            } else {
                *len += (size_t)snprintf(out + *len, cap - *len,
                                         "  %*s(untraced holder, past --depth)\n", 2 * at + 2, "");
            }
            continue;
        }
        if (at + 1 >= depth) continue;
        path[at + 1] = x->h;
        hw_paths(out, cap, len, x->h, path, at + 1, depth);
    }
}

/* Which traced objects a report is about. */
typedef bool (*HwPick)(const Obj *v, const void *arg);

/* Write the holder paths of every traced object of slot `s` that `pick`
 * picks, under a header line. Objects with the same paths are written once,
 * with their number. Reads only the edge table: no heap walk. */
#define HW_REPORT_DISTINCT 16
static void hw_trace_report(uint32_t s, const char *header, HwPick pick, const void *arg) {
    /* On the heap: this runs on a green thread's small stack at an epoch's
     * close, and on the monitor thread for a suspect, at the same time. */
    typedef char HwText[4096];
    HwText *texts = malloc((HW_REPORT_DISTINCT + 1) * sizeof *texts);
    if (texts == NULL) return;
    uint32_t counts[HW_REPORT_DISTINCT];
    uint32_t distinct = 0, objects = 0, other = 0;
    uint32_t depth = hw_head->trace_depth == 0 ? 1 : hw_head->trace_depth;
    pthread_mutex_lock(&hw_trace_lock);
    for (uint32_t e = 0; e < hw_edges_used && objects < 100000; e++) {
        Obj *v = hw_edges[e].v;
        if (v == NULL || v->ty->site != s) continue;
        /* Once per object: at the head of its list of holders. */
        if (hw_edges[e].vprev >= 0) continue;
        if (!pick(v, arg)) continue;
        objects++;
        char *text = texts[HW_REPORT_DISTINCT];
        size_t len = 0;
        Obj *path[HW_PATH_MAX];
        path[0] = v;
        text[0] = 0;
        hw_paths(text, sizeof(HwText), &len, v, path, 0, (int)depth);
        uint32_t i = 0;
        while (i < distinct && strcmp(texts[i], text) != 0) i++;
        if (i == distinct) {
            if (distinct == HW_REPORT_DISTINCT) {
                other++;
                continue;
            }
            memcpy(texts[i], text, len + 1);
            counts[i] = 0;
            distinct++;
        }
        counts[i]++;
    }
    pthread_mutex_unlock(&hw_trace_lock);
    char when[32], line[700];
    hw_stamp(when, sizeof when);
    snprintf(line, sizeof line, "%s %s  %s:%u  %s\n", when, hw_str(hw_slots[s].type_off),
             hw_str(hw_slots[s].file_off), hw_slots[s].line, header);
    hw_trace_write(line);
    if (objects == 0) hw_trace_write("  no traced object of it has a recorded holder\n");
    for (uint32_t i = 0; i < distinct; i++) {
        if (distinct > 1 || counts[i] > 1) {
            snprintf(line, sizeof line, "  %u object%s:\n", counts[i], counts[i] == 1 ? "" : "s");
            hw_trace_write(line);
        }
        hw_trace_write(texts[i]);
    }
    if (other > 0) {
        snprintf(line, sizeof line, "  ... and %u more objects with other paths\n", other);
        hw_trace_write(line);
    }
    free(texts);
    if (hw_head->edges_dropped > 0) {
        snprintf(line, sizeof line, "  (%u edges dropped at the cap; M31_HEAPWATCH_EDGES)\n",
                 hw_head->edges_dropped);
        hw_trace_write(line);
    }
}

typedef struct {
    uint32_t born_by; /* the youngest birth minute picked */
} HwOld;

static bool hw_pick_old(const Obj *v, const void *arg) {
    uint32_t m = (uint32_t)((v->rc & RC_BIRTH) >> RC_BIRTH_SHIFT);
    return m != RC_BIRTH_FOREVER && m <= ((const HwOld *)arg)->born_by;
}

static bool hw_pick_all(const Obj *v, const void *arg) {
    (void)v;
    (void)arg;
    return true;
}

/* A traced slot just became a suspect, or got a doubling older: who holds
 * its old objects. */
static void hw_trace_suspect(uint32_t s, uint32_t now, uint32_t limit) {
    if (__atomic_load_n(&hw_traced[s], __ATOMIC_RELAXED) == 0) return;
    HwOld old = { now > limit ? now - limit - 1 : 0 };
    char header[96];
    char age[16];
    hw_age_text(age, sizeof age, limit);
    snprintf(header, sizeof header, "(suspect: alive > %s)", age);
    hw_trace_report(s, header, hw_pick_old, &old);
}

/* ---- the control page --------------------------------------------------
 *
 * The first page of the ledger is where `m31c -heapwatch --trace` asks a
 * running process for something, with plain writes to the file (the tool
 * has no mmap): requests, then `seq` one higher. The tick compares `seq`
 * with `applied` once a second, applies what is there and sets `applied`.
 * Two tools never write at once: the tool holds `<ledger>.lock`, made with
 * O_EXCL, while it writes, and puts its pid in the lock word for anyone
 * looking. Layout (src/tools/heapwatch.rs has the same):
 *
 *   0   "M31HWCTL"
 *   8   u32 version
 *   12  u32 lock: the writing tool's pid, 0 when none
 *   16  u32 seq       written by the tool
 *   20  u32 applied   written by the process
 *   24  u32 depth     a new --depth, 0 for no change
 *   28  u32 count     requests at 64, 128 bytes each:
 *                     u32 op (1 trace, 2 untrace, 3 dump), char spec[124]
 *
 * Who may ask is whoever may write the file: the ledger is 0644 in a 0700
 * directory, so its owner (open question 9). */
#define HW_CTL_OPS 31

static void hw_control_init(void) {
    memcpy(hw_base + 8, &(uint32_t){HW_VERSION}, 4);
    memcpy(hw_base, "M31HWCTL", 8);
}

/* Apply a trace spec to every slot it names, now and later. */
static void hw_trace_apply(const char *text, bool on) {
    HwSpec spec;
    if (!hw_parse_spec(text, strlen(text), &spec)) return;
    pthread_mutex_lock(&hw_lock);
    if (on) {
        if (hw_ntrace < HW_MAX_TRACE) hw_trace_specs[hw_ntrace++] = spec;
    } else {
        uint32_t k = 0;
        for (uint32_t i = 0; i < hw_ntrace; i++) {
            const HwSpec *t = &hw_trace_specs[i];
            bool same = strcmp(t->type, spec.type) == 0 && strcmp(t->file, spec.file) == 0 &&
                        t->line == spec.line;
            if (!same) hw_trace_specs[k++] = *t;
        }
        hw_ntrace = k;
    }
    uint32_t n_slots = hw_head->n_slots;
    for (uint32_t s = 1; s < n_slots; s++) {
        const HwSlot *sl = &hw_slots[s];
        if (hw_spec_matches(hw_str(sl->type_off), hw_str(sl->file_off), sl->line, spec.type,
                            spec.file, spec.line)) {
            hw_trace_slot(s, on ? 1 : 0);
        }
    }
    pthread_mutex_unlock(&hw_lock);
}

static void hw_control(void) {
    uint32_t seq = __atomic_load_n((uint32_t *)(void *)(hw_base + 16), __ATOMIC_ACQUIRE);
    uint32_t applied = __atomic_load_n((uint32_t *)(void *)(hw_base + 20), __ATOMIC_RELAXED);
    if (seq == applied) return;
    uint32_t depth, count;
    memcpy(&depth, hw_base + 24, 4);
    memcpy(&count, hw_base + 28, 4);
    if (depth > 0 && depth <= 64) hw_head->trace_depth = depth;
    if (count > HW_CTL_OPS) count = HW_CTL_OPS;
    for (uint32_t i = 0; i < count; i++) {
        const unsigned char *r = hw_base + 64 + 128 * i;
        uint32_t op;
        memcpy(&op, r, 4);
        char spec[125];
        memcpy(spec, r + 4, 124);
        spec[124] = 0;
        if (op == 1 || op == 2) {
            hw_trace_apply(spec, op == 1);
        } else if (op == 3) {
            uint32_t n_slots = hw_head->n_slots;
            for (uint32_t s = 1; s < n_slots; s++) {
                if (__atomic_load_n(&hw_traced[s], __ATOMIC_RELAXED) == 0) continue;
                hw_trace_report(s, "(asked for)", hw_pick_all, NULL);
            }
        }
    }
    __atomic_store_n((uint32_t *)(void *)(hw_base + 20), seq, __ATOMIC_RELEASE);
}

/* ---- epoch close (phase 3) ----------------------------------------------
 *
 * docs/heap-watch-design.md, "Epoch close". An EPOCH is a unit of work: a
 * task (every green thread, `main` included) and, inside one, a block
 * `heapwatch.epoch(name, body)` opens. Every object is counted against the
 * innermost epoch open when it was made (the EPOCH field of its header,
 * 62 - depth), in a row of per-site counts kept with the task. An object
 * leaves its epoch's row only by a legitimate way out:
 *
 *   - stored into an object of an older epoch: ADOPTED into that epoch,
 *     with every object it reaches that is younger still (the spreading
 *     walk: it stops at anything as old as the target, so each object is
 *     moved at most once per level it climbs);
 *   - stored into an object counted against no epoch: KEPT, likewise;
 *   - sent to another task, or passed to a spawned one: taken out of the
 *     sender's books (TRANSIT) and adopted by the receiver on arrival;
 *   - returned out of a block (heapwatch.epoch_of): which is a store into a
 *     holder made outside it, so it is the first rule.
 *
 * So when an epoch closes, its locals are gone, and anything its row still
 * counts is held only by other dead objects of the same epoch: a leaked
 * cycle. It is reported then -- the suspect log, the ledger's epoch table,
 * trace's paths for traced sites -- and in a test build it traps.
 *
 * The rows belong to the task, not the thread: a green thread keeps its
 * own (HwTask, reached through the scheduler's per-green-thread slot), and
 * every object is the task's that made or received it -- the move rule
 * means no two tasks ever reach one object. The row of a depth is a small
 * open-addressed table, site -> count. */

typedef struct {
    uint32_t site;   /* 0: empty */
    int32_t  count;
} HwCell;

typedef struct {
    HwCell  *cells;  /* open addressing, home cell `site & (cap - 1)` */
    uint32_t cap;    /* a power of two, or 0 */
    uint32_t used;
    char     name[48];
} HwRow;

typedef struct {
    uint32_t depth;     /* the innermost open epoch; 0 is the task itself */
    uint32_t overflow;  /* blocks nested past the deepest depth counted */
    uint32_t cap;
    HwRow   *rows;
} HwTask;

#define HW_MAX_DEPTH 61

/* The current green thread's slot for its HwTask (scheduler.c sets it
 * around each dispatch); NULL on a thread that is not running a task. */
_Thread_local void **rt_hw_task_slot;

static bool hw_trap_on_leak;

static HwTask *hw_task_new(void **slot);

/* The running task's books, made on its first use; NULL outside a task. */
static inline HwTask *hw_task(void) {
    void **slot = rt_hw_task_slot;
    if (slot == NULL) return NULL;
    HwTask *t = *slot;
    return t != NULL ? t : hw_task_new(slot);
}

static HwTask *hw_task_new(void **slot) {
    HwTask *t = calloc(1, sizeof *t);
    if (t == NULL) rt_trap("out of memory");
    t->cap = 4;
    t->rows = calloc(t->cap, sizeof *t->rows);
    if (t->rows == NULL) rt_trap("out of memory");
    snprintf(t->rows[0].name, sizeof t->rows[0].name, "task");
    *slot = t;
    return t;
}

static void hw_row_add_slow(HwRow *r, uint32_t site, int32_t d) {
    if (r->cap == 0 || (r->used + 1) * 4 >= r->cap * 3) {
        uint32_t cap = r->cap == 0 ? 16 : r->cap * 2;
        HwCell *c = calloc(cap, sizeof *c);
        if (c == NULL) rt_trap("out of memory");
        for (uint32_t i = 0; i < r->cap; i++) {
            if (r->cells[i].site == 0) continue;
            uint32_t j = r->cells[i].site & (cap - 1);
            while (c[j].site != 0) j = (j + 1) & (cap - 1);
            c[j] = r->cells[i];
        }
        free(r->cells);
        r->cells = c;
        r->cap = cap;
    }
    uint32_t j = site & (r->cap - 1);
    while (r->cells[j].site != 0 && r->cells[j].site != site) j = (j + 1) & (r->cap - 1);
    if (r->cells[j].site == 0) {
        r->cells[j].site = site;
        r->used++;
    }
    r->cells[j].count += d;
}

/* Sites are small numbers handed out in order, so a site's home cell is
 * nearly always its own: the common case is a load, a compare and an add. */
static inline void hw_row_add(HwRow *r, uint32_t site, int32_t d) {
    if (r->cap != 0) {
        HwCell *c = &r->cells[site & (r->cap - 1)];
        if (c->site == site) {
            c->count += d;
            return;
        }
    }
    hw_row_add_slow(r, site, d);
}

/* The row an object with EPOCH field `e` is counted in, if it is one of
 * this task's open epochs. */
static HwRow *hw_row_of(HwTask *t, unsigned e) {
    if (t == NULL || e == RC_EPOCH_TRANSIT || e == RC_EPOCH_NONE) return NULL;
    unsigned depth = 62 - e;
    return depth <= t->depth ? &t->rows[depth] : NULL;
}

static inline unsigned hw_epoch_bits(const Obj *o) {
    return (unsigned)(((unsigned long)o->rc >> RC_EPOCH_SHIFT) & 63);
}

static inline void hw_set_epoch(Obj *o, unsigned e) {
    o->rc = (o->rc & ~RC_EPOCH) | ((long)e << RC_EPOCH_SHIFT);
}

static long hw_epoch_of_new(const TypeInfo *ty) {
    HwTask *t = hw_task();
    if (t == NULL) return RC_EPOCH_NONE;
    if (ty->site != 0) hw_row_add(&t->rows[t->depth], ty->site, 1);
    return 62 - (long)t->depth;
}

static void hw_epoch_free(Obj *o) {
    if (o->ty->site == 0) return;
    void **slot = rt_hw_task_slot;
    if (slot == NULL) return;
    HwRow *r = hw_row_of(*slot, hw_epoch_bits(o));
    if (r != NULL) hw_row_add(r, o->ty->site, -1);
}

/* Move `v`, and everything it reaches that is younger than `target`, to
 * epoch field `target`: out of the row it was counted in and into
 * `target`'s (none, for kept). The walk stops at anything already as old as
 * the target, so it is bounded by the objects that move -- each at most once
 * per epoch it climbs -- and never visits the rest of the heap. */
typedef struct {
    Obj    *small[32];
    Obj   **todo;
    size_t  len;
    size_t  cap;
    unsigned target;
} HwSpread;

static void hw_spread_push(HwSpread *w, Obj *o) {
    if (w->len == w->cap) {
        size_t cap = w->cap * 2;
        Obj **bigger = w->todo == w->small ? malloc(cap * sizeof *bigger)
                                           : realloc(w->todo, cap * sizeof *bigger);
        if (bigger == NULL) rt_trap("out of memory");
        if (w->todo == w->small) memcpy(bigger, w->small, sizeof w->small);
        w->todo = bigger;
        w->cap = cap;
    }
    w->todo[w->len++] = o;
}

static void hw_spread_visit(void *ctx, Obj *c) {
    HwSpread *w = ctx;
    if (c != NULL && c->rc != RC_IMMORTAL && hw_epoch_bits(c) < w->target) hw_spread_push(w, c);
}

static void hw_adopt(Obj *v, unsigned target) {
    HwTask *t = hw_task();
    HwSpread w;
    w.todo = w.small;
    w.len = 0;
    w.cap = sizeof w.small / sizeof w.small[0];
    w.target = target;
    hw_spread_visit(&w, v);
    while (w.len > 0) {
        Obj *o = w.todo[--w.len];
        unsigned e = hw_epoch_bits(o);
        if (e >= target) continue; /* reached twice; moved the first time */
        if (o->ty->site != 0) {
            HwRow *from = hw_row_of(t, e);
            if (from != NULL) hw_row_add(from, o->ty->site, -1);
            HwRow *to = hw_row_of(t, target);
            if (to != NULL) hw_row_add(to, o->ty->site, 1);
        }
        hw_set_epoch(o, target);
        if (o->ty->walk != NULL) o->ty->walk(o, hw_spread_visit, &w);
    }
    if (w.todo != w.small) free(w.todo);
}

/* A store: `v` is now held by `h`. If `h` outlives more epochs than `v`
 * (a larger EPOCH field), `v` and what it reaches are adopted into `h`'s. */
static void hw_epoch_store(Obj *h, Obj *v) {
    unsigned eh = hw_epoch_bits(h);
    if (v->rc == RC_IMMORTAL || eh <= hw_epoch_bits(v)) return;
    hw_adopt(v, eh);
}

/* `v` is crossing to another task: take it, and everything it reaches that
 * is counted, out of this task's books. Its uniqueness was just checked, so
 * nothing else here reaches any of it. */
void rt_hw_send(Obj *v) {
    if (!hw_on || v == NULL || v->rc == RC_IMMORTAL) return;
    HwTask *t = hw_task();
    HwSpread w;
    w.todo = w.small;
    w.len = 0;
    w.cap = sizeof w.small / sizeof w.small[0];
    w.target = RC_EPOCH_NONE; /* push everything not yet in transit or kept */
    hw_spread_push(&w, v);
    while (w.len > 0) {
        Obj *o = w.todo[--w.len];
        unsigned e = hw_epoch_bits(o);
        if (e == RC_EPOCH_TRANSIT || e == RC_EPOCH_NONE) continue;
        if (o->ty->site != 0) {
            HwRow *from = hw_row_of(t, e);
            if (from != NULL) hw_row_add(from, o->ty->site, -1);
        }
        hw_set_epoch(o, RC_EPOCH_TRANSIT);
        if (o->ty->walk != NULL) o->ty->walk(o, hw_spread_visit, &w);
    }
    if (w.todo != w.small) free(w.todo);
}

/* `v` arrived in this task -- out of a channel, or as a spawned task's
 * argument: what is in transit is counted here from now on, against the
 * epoch the task is in. */
void rt_hw_receive(Obj *v) {
    if (!hw_on || v == NULL || v->rc == RC_IMMORTAL) return;
    HwTask *t = hw_task();
    if (t == NULL) return;
    hw_adopt(v, 62 - t->depth);
}

/* ---- opening and closing --------------------------------------------- */

static void hw_task_grow(HwTask *t, uint32_t depth) {
    if (depth < t->cap) return;
    uint32_t cap = t->cap * 2;
    while (cap <= depth) cap *= 2;
    HwRow *rows = realloc(t->rows, cap * sizeof *rows);
    if (rows == NULL) rt_trap("out of memory");
    memset(rows + t->cap, 0, (cap - t->cap) * sizeof *rows);
    t->rows = rows;
    t->cap = cap;
}

static void hw_name_row(HwRow *r, const char *name, size_t n) {
    if (n >= sizeof r->name) n = sizeof r->name - 1;
    memcpy(r->name, name, n);
    r->name[n] = 0;
}

/* Which of two leaked sites a report names first: by file, line, then type,
 * so the text is the same on every run whatever order the slots were made
 * in. */
static int hw_site_order(uint32_t a, uint32_t b) {
    const HwSlot *x = &hw_slots[a], *y = &hw_slots[b];
    int c = strcmp(hw_str(x->file_off), hw_str(y->file_off));
    if (c != 0) return c;
    if (x->line != y->line) return x->line < y->line ? -1 : 1;
    return strcmp(hw_str(x->type_off), hw_str(y->type_off));
}

/* The leaks of a closing row, as "2 Node @ main.m31:12, 1 List @ ...". */
static size_t hw_row_text(const HwRow *r, char *out, size_t cap) {
    HwCell left[32];
    uint32_t n = 0, more = 0;
    for (uint32_t i = 0; i < r->cap; i++) {
        const HwCell *c = &r->cells[i];
        if (c->site == 0 || c->count == 0) continue;
        if (n == 32) {
            more++;
            continue;
        }
        uint32_t j = n++;
        while (j > 0 && hw_site_order(left[j - 1].site, c->site) > 0) {
            left[j] = left[j - 1];
            j--;
        }
        left[j] = *c;
    }
    size_t len = 0;
    out[0] = 0;
    for (uint32_t i = 0; i < n && len + 200 < cap; i++) {
        const HwSlot *sl = &hw_slots[left[i].site];
        len += (size_t)snprintf(out + len, cap - len, "%s%d %s @ %s:%u", i == 0 ? "" : ", ",
                                left[i].count, hw_str(sl->type_off), hw_str(sl->file_off), sl->line);
    }
    if (more > 0 && len + 40 < cap) {
        len += (size_t)snprintf(out + len, cap - len, ", and %u more sites", more);
    }
    return len;
}

typedef struct {
    unsigned epoch;
} HwOfEpoch;

/* An object the closing epoch left behind. Once reported it is marked as
 * counted against none, so the next close at the same depth does not
 * report it again: nothing reaches it to change it, and if something did
 * after all, a store into it only keeps what it is given. */
static bool hw_pick_epoch(const Obj *v, const void *arg) {
    if (hw_epoch_bits(v) != ((const HwOfEpoch *)arg)->epoch) return false;
    hw_set_epoch((Obj *)v, RC_EPOCH_NONE);
    return true;
}

/* Close the innermost open epoch of `t`. */
static void hw_close(HwTask *t) {
    HwRow *r = &t->rows[t->depth];
    bool leaked = false;
    for (uint32_t i = 0; i < r->cap; i++) leaked = leaked || (r->cells[i].site != 0 && r->cells[i].count != 0);
    if (leaked) {
        char text[440];
        hw_row_text(r, text, sizeof text);
        char line[600], when[32];
        hw_stamp(when, sizeof when);
        snprintf(line, sizeof line, "%s EPOCH \"%s\" closed with leaks: %s\n", when, r->name, text);
        hw_log(line);
        /* The ledger's table of epochs that leaked, by name. */
        pthread_mutex_lock(&hw_lock);
        HwEpochRow *row = NULL;
        for (int i = 0; i < HW_EPOCH_ROWS && row == NULL; i++) {
            if (hw_epochs[i].times == 0 || strcmp(hw_epochs[i].name, r->name) == 0) row = &hw_epochs[i];
        }
        if (row != NULL) {
            if (row->times == 0) snprintf(row->name, sizeof row->name, "%s", r->name);
            snprintf(row->last, sizeof row->last, "%.63s", text);
            row->minute = hw_now();
            __atomic_store_n(&row->times, row->times + 1, __ATOMIC_RELEASE);
        }
        pthread_mutex_unlock(&hw_lock);
        /* Who holds what leaked, for the sites trace is on for. */
        if (__atomic_load_n(&rt_hw_tracing, __ATOMIC_RELAXED)) {
            HwOfEpoch of = { 62 - t->depth };
            char header[100];
            snprintf(header, sizeof header, "(epoch \"%s\" left them)", r->name);
            for (uint32_t i = 0; i < r->cap; i++) {
                const HwCell *c = &r->cells[i];
                if (c->site == 0 || c->count <= 0) continue;
                if (__atomic_load_n(&hw_traced[c->site], __ATOMIC_RELAXED) == 0) continue;
                hw_trace_report(c->site, header, hw_pick_epoch, &of);
            }
        }
        if (hw_trap_on_leak) {
            char msg[600];
            snprintf(msg, sizeof msg, "epoch \"%.47s\" closed with leaks: %.439s", r->name, text);
            rt_trap(msg);
        }
    }
    free(r->cells);
    memset(r, 0, sizeof *r);
}

/* heapwatch.epoch(name, ..): open an epoch inside the current task. */
void rt_hw_epoch_enter(Obj *name) {
    if (!hw_on) return;
    HwTask *t = hw_task();
    if (t == NULL) return;
    if (t->depth == HW_MAX_DEPTH) {
        t->overflow++;
        return;
    }
    hw_task_grow(t, t->depth + 1);
    t->depth++;
    const Str *s = (const Str *)name;
    hw_name_row(&t->rows[t->depth], s->data, (size_t)s->len);
}

void rt_hw_epoch_leave(void) {
    if (!hw_on) return;
    HwTask *t = hw_task();
    if (t == NULL) return;
    if (t->overflow > 0) {
        t->overflow--;
        return;
    }
    if (t->depth == 0) return;
    hw_close(t);
    t->depth--;
}

/* A task starts: name its own epoch -- "main", or the spawned function. */
void rt_hw_task_begin(const char *name) {
    if (!hw_on) return;
    HwTask *t = hw_task();
    if (t != NULL) hw_name_row(&t->rows[0], name, strlen(name));
}

/* A task ended (scheduler.c, after its entry returned): close whatever it
 * still has open, then the task's own epoch, and drop its books. */
void rt_hw_task_end(void) {
    void **slot = rt_hw_task_slot;
    if (slot == NULL || *slot == NULL) return;
    HwTask *t = *slot;
    if (hw_on) {
        while (t->depth > 0) {
            hw_close(t);
            t->depth--;
        }
        hw_close(t);
    }
    for (uint32_t i = 0; i < t->cap; i++) free(t->rows[i].cells);
    free(t->rows);
    free(t);
    *slot = NULL;
}
