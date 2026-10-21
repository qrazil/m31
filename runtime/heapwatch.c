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
    h->trace_depth = hw_env_u32("M31_HEAPWATCH_DEPTH", 3, 0, 64);
    snprintf(h->program, sizeof h->program, "%s", name);
    hw_control_init();

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

/* ---- trace and epochs: not yet ---------------------------------------- */

static void hw_trace_slot(uint32_t s, uint8_t on) {
    __atomic_store_n(&hw_traced[s], on, __ATOMIC_RELAXED);
}

static void hw_trace_suspect(uint32_t s, uint32_t now, uint32_t limit) {
    (void)s;
    (void)now;
    (void)limit;
}

static void hw_control_init(void) {
    memcpy(hw_base, "M31HWCTL", 8);
}

static void hw_control(void) {}

static long hw_epoch_of_new(const TypeInfo *ty) {
    (void)ty;
    return RC_EPOCH_NONE;
}

static void hw_epoch_free(Obj *o) {
    (void)o;
}
