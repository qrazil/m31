# The standard library, and what an error is

`docs/errors-decision.md` left exactly one question open: **what is `E` in a
standard library?** It said to settle it last, once there was a library to say
what actually fails. This is that.

---

## The question, and why it looked hard

A function that can fail returns `Result<T, E>`. What is `E`?

  - **`Result<T, str>`** — a message. Easy, and lossy: a caller cannot branch
    on *which* failure without parsing English.
  - **A single blessed error enum** — callers can branch, and the enum is
    frozen forever.
  - **One enum per library** — specific and honest, but `?` requires the error
    types to match exactly, so an error cannot cross a library boundary
    without being rewrapped.
  - **An `Error` interface** — any type with the right methods.

---

## What is decided

### The interface option is dead on arrival

An interface-typed error is **write-only**. The language has no downcasting
and no runtime type queries (§9), so a caller holding an `Error` can ask it
for a message and nothing else — it can never recover the concrete type to
branch on. That is strictly worse than `str`, which at least admits what it
is. Ruled out on a property the language already has, not on taste.

### There is no blessed error type

One global error enum cannot work here, and the reason is the language's own
best feature: **`match` is exhaustive and has no `default`.** That means
adding a variant to the blessed enum is a compile error in every program that
handles errors. Not a deprecation, not a warning — a break. So the one type
every library depends on becomes the one type that can never learn a new kind
of failure. `TimedOut` could never be added.

A `str` error would avoid that and pay for it every time a caller wants to
distinguish "not found" from "permission denied", which is the case that
matters most.

**So: `E` is whatever the library says it is.** The compiler blesses
`Option` and `Result` — the shapes — and nothing about what goes inside.

### Composition is explicit, and that is the honest cost

`?` requires the error types to match exactly, so an error crossing a library
boundary has to be rewrapped:

```c
match (io.read(path)) {
    case Err(io.Error e): { return Result<Config, ConfigError>.Err(ConfigError.Io(e)); }
    case Ok(str text):    { ... }
}
```

That is verbose, and it is what Go makes you do too — `fmt.Errorf("...: %w")`
is a rewrap with a nicer face. The alternative is an implicit conversion at
every `?`, which means deciding *silently* how one library's failure becomes
another's. Explicit is the right default for a language that refuses
implicit numeric conversion.

Relaxing this later is additive: a conversion could be found by name, the way
`to_str` is, and no existing program would change meaning.

### The built-ins dodge the question entirely

`"42".parse_int()` returns **`Option<int>`**, not a `Result`.

The compiler has to know the return type of a method it provides, so a
built-in genuinely cannot return a library's error enum — the same constraint
that forced `Option` and `Result` to be built in. But it does not need to:
the question a built-in parse answers is *"did it parse?"*, and that is
exactly `Option`-shaped.

It is lossy — "not a number" and "out of range" are both `None` — and that is
the right trade for the built-in. A library that needs the distinction builds
its own on top of `str.byte_at`, and declares its own error.

---

## What the standard library is

Written **in this language**, not in the compiler, except where a primitive is
genuinely unavailable from inside. Every module below is ordinary source that
a program could have written itself. That is the point: a standard library
that needs compiler privileges is a language admitting it is not finished.

### Primitives the compiler must provide

These cannot be written in the language, so they are methods rather than
modules:

| | |
|---|---|
| `s.byte_at(i)` | one byte as an `int`; traps out of range. The foundation everything textual is built on. |
| `s.parse_int()`, `s.parse_float()` | `Option<T>`. Correct float parsing is not something to reimplement. (It was reimplemented once bit operators existed: `parse_float` is language source now, stdlib-seam §6.) |
| `n.to_str()` on `int`, `float`, `bool` | shortest round-tripping form for a float; a program cannot format a double from inside. (It can now, with bit operators, and the float case is language source: stdlib-seam §6.) |

`to_str` completes the conversion story: `print(v)` and `str(v)` already look
for `to_str` by name on a user type, and now the built-in types answer the
same call. One rule for all of them.

### Modules, in the order they earn their place

| Module | Holds |
|---|---|
| `io` | read and write a whole file, read a line from stdin, write to stderr. Errors are `io.Error`, its own enum. |
| `math` | `abs`, `min`, `max`, `pow`, `sqrt`, `floor`, `ceil`, `round`. Float-heavy, and mostly one-liners over C. |
| `sort` | sorting a `List` by a comparison the caller supplies. Still waiting on function references; sorting by the element type's OWN `cmp` is built in and needs none — see below. |
| `text` | what `str`'s built-in methods leave out: padding, `replace`, `lines`, a richer parse that says *why* it failed. |

**`sort` by a comparison the CALLER supplies** needs a function reference in
the IR, which does not exist and is the same machinery closures need.

**Sorting by the element type's own order needs none of that**, and now
works: `sort()` handles `int`, `float`, `str`, and any type that declares
`int T.cmp(T other)` — the method `<` already uses. The runtime is holding
the element and the element carries its type, so the compiler stores `cmp`
in the type's metadata and the runtime calls it there. A paragraph of this
record used to say otherwise; see docs/closures-decision.md §"Two of the
three blocked features are not blocked by this".

**A `Hashable` interface** so a `Map` can take a user type as a key is
wanted, and is not free: `Map` is a runtime structure and hashing a user type
means calling back into generated code. The same mechanism should answer it
-- the record above argues it does -- and it has not been built yet.

---

## Order of work

1. The three primitives: `byte_at`, `parse_int`/`parse_float`, `to_str`.
2. `io`, because nothing else can be tested from the outside without it.
3. `math`, which is small and unblocks anything numeric.
4. `text`, once `io` has shown what a library's error type wants to look like.

Of the three features this record once said were waiting on one mechanism,
`sort()` on a user type was not: it shipped without one, against a
reserved-method-name convention, and a `Map` keyed on a user type should go
the same way. `spawn` should never take a closure at all
(docs/closures-decision.md). What genuinely waits on a function reference is
a `sort` that takes the comparison as an argument.
