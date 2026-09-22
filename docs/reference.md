# Language reference

This is the **normative** description of the language as it stands. The README
introduces; `docs/types.md`, `docs/ir-v0.md` and `docs/concurrency.md` argue.
This one only states.

Anything not written here does not exist. That is the point of writing it: the
plan is to freeze early, and you cannot freeze a surface you have not written
down. Where this document and the implementation disagree, one of them is a
bug — say which in the report.

Every rule below is backed by a corpus program. A rule with no test is a
wish, so if you add one here, add one there.

---

## 1. Lexical

### 1.1 Source

UTF-8. A source file is a sequence of items and statements; there is no
enclosing declaration and no `main`.

### 1.2 Comments

`// to end of line` and `/* ... */`. Block comments do not nest.

### 1.3 Identifiers

A letter or `_`, then letters, digits or `_`. Case-sensitive.

### 1.4 Keywords

    bool     break     bytes     case      const     continue  distinct
    else     enum      false     float     for       if        import
    in       int       interface match     pub       return    spawn
    static   str       true      type      void      while

Keywords are reserved: none may be used as an identifier. `Array`, `Chan`,
`List` and `Map` are not keywords — they are predeclared type names, and are
reserved only in the sense that nothing may shadow a type name (§4.1).

### 1.5 Literals

| | |
|---|---|
| Integer | `0`, `42`. Decimal only. No sign — `-1` is unary minus applied to `1`. |
| Float | `1.0`, `3.14`, `2.5e3`. **Always a dot with digits on both sides** — not `1.` and not `.5`. An exponent only after the dot form: `1.0e9`, not `1e9`. A literal too large **or too small** to represent is an error: one that parses to exactly zero has lost its whole value. Arithmetic that underflows at run time is ordinary IEEE. |
| Boolean | `true`, `false` |
| String | `"..."`, with escapes `\\` `\"` `\n` `\t` `\0` |

A `str` carries its length, so `\0` is an ordinary character:
`"a\0b".size()` is 3. Strings are not NUL-terminated.

String literals are **immortal**: their refcount never reaches zero, so they
are never freed (§7.3).

There is no `bytes` literal. A `bytes` is written with the sequence literal,
`[104, 105]`, or converted from text, `"hi".to_bytes()` (§3.10).

### 1.6 Operators and punctuation

    +   -   *   /   %
    ==  !=  <   <=  >   >=
    &&  ||  !
    &   |   ^   ~   <<  >>
    =   .   ,   ;   :
    (   )   {   }   [   ]   <   >

`>>` is not a token. The two characters close two type argument lists in
`List<List<int>>`, so the lexer always produces single `>`s, and the parser
reads two of them **with nothing between** as a right shift wherever a binary
operator may stand. `a > > b` is not a shift. `<<` has no such double life —
no valid program has two `<` in a row — and is one token.

---

## 2. Program structure

A file is a sequence of four kinds of item, in any order:

  - a **type declaration** — `type`, `interface`, or `distinct`
  - a **function declaration**
  - a **method declaration**
  - a **top-level statement**

Top-level statements run, in source order, as the program. Declarations do
not: a file that is all declarations is a program that does nothing.

Forward references are fine. Types and functions are collected before any
body is checked, so an item may name one declared later in the file.

**Nesting is limited to 256 levels**, counting statements, expressions and
types together: a block inside a block, an argument inside a call, a type
argument inside a type. It is the depth of the tree that counts, so a flat
`a + b + c + ...` of more than 256 terms is refused too — it is as deep as it
is long. Past the limit the compiler says so, rather than overflowing its
own stack.

## 2.1 Modules

**A file is a module**, and its name is the file's basename: `strings.src` is
the module `strings`. There is no `mod` declaration and no module tree to
keep in agreement with the filesystem.

```c
import greet;
import count;

print(greet.shout("world"));
```

  - **`import` comes first**, before any declaration, so a reader learns a
    file's dependencies without reading the file. Importing twice, or
    importing yourself, is an error.
  - **Private by default.** A declaration is visible only inside its module
    unless marked `pub`. Reaching a name from another module means writing
    `mod.name`; unqualified, it is not in scope at all. That holds for
    everything a module declares — functions, types, methods and an enum's
    variants — not only functions. A method that could not be called from
    here does not satisfy an interface here either, and neither does one
    that `print` or an operator would find by name: an interface or an
    operator is another way of calling the method, not a way around `pub`.
  - **A type from another module is `mod.Type`**, anywhere a type may be
    written: a declaration, a parameter, a type argument, a construction
    (`shapes.Point(3, 4)`, `shapes.Box<int>(1)`), a static method
    (`shapes.Point.origin()`) or an enum variant (`shapes.Colour.Red`).
  - **A method may only be added to a type its own module declared.** Go and
    Rust both draw this line, and without it a module could reach into
    another's private type by declaring a method on it.
  - **No wildcard import and no aliasing.** A reader can always tell which
    module a name came from — which matters more here than elsewhere,
    because §4.1 already bans shadowing. Module names are unique, so two
    imports cannot collide and an alias would have nothing to resolve.
  - **Import cycles are refused**, and the diagnostic prints the whole chain.
    Self-import is the same rule, not a special case.
  - **One entry file.** The file named on the command line is the program;
    statements at the top level of any *imported* file are an error.

A module's name has to be usable as an identifier, because it is written in
source. The entry file is exempt — it is named on the command line and never
in source, so `011-types.src` is a fine program and a hopeless import.

**Two files whose names differ only in case are an error on every platform**,
not only where the filesystem would confuse them.

Module names are therefore unique across a program: two files called
`util.src` in different directories are a collision rather than two modules.
That is the cost of naming by basename, and it is the same cost OCaml pays.

---

## 3. Types

### 3.1 Value types

`int` is a 64-bit signed integer. `float` is an IEEE 754 double. `bool` is
`true` or `false`. None is a reference; all are copied by assignment.

**There is no implicit conversion, in either direction.** `1.5 + 2` is an
error, not a widening — convert explicitly with `float(n)` or `int(x)`.
`int(x)` truncates toward zero and **traps** on a NaN or a value outside the
integer range, because the C cast is undefined there.

The two number types differ in what they do when an operation has no
in-range answer, and both are right for what they are:

| | |
|---|---|
| `int` | **traps** on overflow and on division by zero |
| `float` | follows IEEE: an infinity, or a NaN. Those are defined answers, not faults |

`%` is integer remainder and does not apply to `float`.

A NaN is not equal to itself, so `x == x` is `false` for one. Sorting needs
a total order that `<` does not give, so `sort()` places NaNs last (§3.9).
A `float` cannot be a `Map` key.

`int` is deliberately unqualified. The IR may lower it to i32 or i128 for a
target that wants that; the source spells one integer type. The bit
operators and the wrapping methods (§6.1, §6.5a) are defined on exactly 64
bits, though, so a target with a narrower `int` has to emulate them there.

### 3.2 Reference types

`str`, `bytes`, the built-in collections, and every `type` are **references**.
Assignment aliases rather than copies:

```c
type Point { int x; int y; }
Point a = Point(1, 2);
Point b = a;
b.x = 99;
print(a.x);        // 99 -- one object, two names
```

`clone(x)` is the explicit second object (§6.4).

`str` is immutable, so its aliasing is not observable except through
identity, which matters only at a thread boundary (§8.3).

### 3.3 Structs

```c
type Point {
    int x;
    int y = 0;      // a default makes the field optional at construction
}
```

A field with no default is **mandatory** and is passed positionally; a field
with a default is **optional** and is passed by name. This is the same rule
as for function arguments (§4.2), and it is the only rule:

```c
Point p = Point(3, 4);          // both mandatory
Point q = Point(3);             // y defaults to 0
```

Naming a mandatory field is an error, not a courtesy — one spelling per call
means a reader never sees the same construction written two ways.

Duplicate field names are an error.

### 3.4 Interfaces

An interface is a set of method signatures:

```c
interface HasArea {
    int area();
}
```

Satisfaction is **structural**: a type that has every listed method with a
matching signature satisfies the interface, with no declaration saying so.
Assigning such a value to an interface-typed slot is allowed; assigning one
that does not is a compile error naming the missing method.

Dispatch goes through a vtable in the object header. An interface value is a
single pointer, not a pair — the Java model, not Go's (`docs/types.md` §4).
A slot is keyed by a method's **name and its shape**, so two interfaces
declaring `m` with different signatures take different slots rather than
sharing one that each call site would cast its own way.

Two kinds of method cannot satisfy an interface, both because of how
dispatch works rather than by policy:

  - a **static** method, which has no receiver to dispatch on — its
    signature is one argument short of what the slot is cast to;
  - a method on a **distinct** type, which is erased before the IR and so has
    no object header of its own to carry a vtable. Dispatch would always find
    the base type's method.

### 3.5 Embedding

An anonymous field, named after its type, promotes that type's fields and
methods into the outer type:

```c
type Animal { str name; }
type Dog { Animal; int legs; }

Dog d = Dog(Animal("Rex"), 4);
print(d.name);      // "Rex", promoted from Animal
```

Promotion is by synthesised forwarder methods, run to a fixpoint, so
embedding an embedder works. A real method on the outer type **shadows** a
forwarder of the same name; two embedded types offering the same name with no
outer method to break the tie is an error at the use site.

**Static methods are not promoted.** A static has no receiver, so there is
nothing for a forwarder to forward to: `Dog` does not gain `Animal`'s
statics by embedding one.

Embedding is composition. There is no inheritance and no method overriding
(`docs/types.md` §5).

### 3.6 Distinct types

```c
distinct int Price;
distinct int Quantity;
```

A distinct type has the **same representation** as its base and a different
identity. `Price` and `Quantity` are not interchangeable, and neither is
interchangeable with `int`.

Distinctness is erased before the IR, so it costs nothing: no allocation, no
wrapper, no indirection. A `distinct int` is an `int` in the emitted C.

Conversion is explicit and free, spelled as the base type:

```c
Price p = Price(500);
int cents = int(p);
```

Arithmetic on a distinct type stays in that type: `p + p` is a `Price`. That
is the point — a `Price` that decayed to `int` on the first addition would
protect nothing. Mixing `Price` and `Quantity`, or `Price` and `int`, is an
error; convert first.

The base may be any type, generic ones included. A distinct **collection**
is still a collection — it indexes, iterates and answers its base's
methods — and a collection literal written where one is expected builds it
directly, because a literal has no type of its own until its place gives it
one. Converting back is spelled as the base type, like `int(p)`:

```c
distinct List<int> Bag;

Bag b = [1, 2, 3];
b.push(4);
List<int> plain = List<int>(b);
```

There are no type aliases. An alias that is not distinct documents and does
not enforce, which is a comment with syntax.

### 3.7 Enums

A value that is exactly one of several shapes, each able to carry its own
data:

```c
enum Option<T> {
    None;
    Some(T);
}

enum Shape {
    Empty;
    Circle(int);
    Rect(int, int);
}
```

A payload is **positional and unnamed**. A variant is not a struct — if
there is enough in it to want field names, the payload should *be* a struct.
An enum with no variants is an error: no value of it could ever exist.

Construction writes the enum type in full:

```c
Option<int> a = Option<int>.Some(1);
Option<int> b = Option<int>.None;
Shape c = Shape.Rect(3, 4);
```

The type is written rather than inferred because a variant with no payload
has nothing to infer it from, and one rule beats a rule with an exception.

An enum is a reference type like a struct, and it owns its payload: a
reference going in is retained, and released when the enum is freed.

There is no subtyping here. `Circle` is not a type and not a subclass of
`Shape`; it is one of the shapes a `Shape` can be. The only way to get at a
payload is `match`.

### 3.7a `Option` and `Result`

Two enums the language declares for you:

```c
enum Option<T>    { None; Some(T); }
enum Result<T, E> { Ok(T); Err(E); }
```

They are built in for a hard reason rather than a convenient one: **a
built-in method cannot return a type the program defines**, because the
compiler has to know what `xs.index_of(v)` gives back. Without a blessed
`Option` that method cannot exist at all. The second reason is composition —
two libraries with their own `Result` cannot pass one through the other.

A program may not redeclare either.

Past the declaration they are ordinary enums: the same `match`, the same
exhaustiveness, no special construction syntax. `Option` has three methods,
and no more:

| | |
|---|---|
| `o.is_some()`, `o.is_none()` | the question, without a `match` |
| `o.or(v)` | the value, or `v` if there is none |

`is_none` is kept even though it is `!o.is_some()`. The single-`cmp` rule
looks like it forbids this, but it is about something else: four comparison
methods could disagree, because each is an implementation the author writes.
`is_none` is generated from `is_some` by the compiler and cannot drift.

There is no `unwrap`. Trapping on `None` is what `Map.get` used to do, and
putting it back behind a shorter name would undo the reason for the change.
Taking the value out and keeping it is what `match` is for.

**A function that can fail but has nothing to return is
`Result<void, E>`.** A `void` payload is no value, so it is dropped: at
`Result<void, E>`, `Ok` is a variant that carries nothing, and it is written
exactly like every other payload-less variant — no special case:

```c
Result<void, str> check(int x) {
    if (x < 0) { return Result<void, str>.Err("negative"); }
    return Result<void, str>.Ok;
}

check(a)?;                  // a statement: there is no value to give
match (check(b)) {
    case Ok: { ... }
    case Err(str e): { ... }
}
```

The same holds for any generic enum at `void` (`Option<void>` has a `Some`
that carries nothing, and no `or`). A struct cannot be instantiated at
`void`: a field is named at every construction and every read, so it cannot
quietly vanish the way a payload does.

### 3.8 Generics

Type parameters on a type or a function:

```c
type Box<T> { T item; }
T unwrap<T>(Box<T> b) { return b.item; }
```

Generics are **monomorphised**: each instantiation becomes a separate
concrete type or function before the IR, so there is no boxing and no runtime
type argument. Unused instantiations are not emitted.

A **method on a generic type names the type's parameters on its receiver**,
and a method may have type parameters of its own, on any type:

```c
T Box<T>.get() { return item; }
Box<U> Box<T>.swap<U>(U v) { return Box<U>(v); }
T Picker.pick<T>(List<T> xs) { return xs[at]; }
```

The names on the receiver are the method's; they need not match the type
declaration's, and there must be as many. `T Box.get()` for a generic `Box`
is refused: the signature should say what `T` is without sending the reader
to the type. A method on a generic type is instantiated with each
instantiation of the type, and is checked only then, like the rest of a
generic declaration.

A generic method's own type arguments are inferred like a function's, and
that needs the receiver's type written down too: the receiver must be a
local, a parameter, a construction, or — inside a method — a receiver
field (on a generic type, when the method names the type's parameters as
its declaration does). `make().pick(xs)` is refused, with that said; bind `make()` to a
local first.

A generic function's type arguments are **inferred from its arguments**;
there is no `f<int>(x)`, because after a name that is not a type `<` would be
a comparison. Each type parameter must be reached by a mandatory parameter
whose argument has a written type: a literal, a construction, an enum variant,
or a local or parameter (whose declaration spells its type). A collection
literal contributes its first element — `first([1, 2])` against `List<T>`
binds `T` to `int` — and an empty one contributes nothing. When a parameter
cannot be reached the compiler says so and asks for a local with a written
type. A `pub` generic function is called from another module like any other,
as `lib.first(xs)`.

There are no constraints on type parameters yet. A generic body that does
something a given argument cannot do fails when that instantiation is
checked, naming the instantiation.

### 3.9 Collections

A collection is written as a **literal**, and the type it is being written
into says which collection it is. There is no constructor form; `List<int>()`
is refused, because two spellings of one thing is one too many.

| Literal | Type it makes |
|---|---|
| `[]` | an empty `List<T>` or `Array<T>` or, in `Array` position, length 0 |
| `[a, b, c]` | a `List<T>` or an `Array<T>` of length 3 |
| `[v; n]` | `n` slots, every one of them `v` |
| `{}` | an empty `Map<K, V>` |
| `{k: v, ...}` | a `Map<K, V>` with those entries |
| `Chan<T>(cap)` | a bounded channel — not a literal, because a capacity is not contents |

```c
List<int> xs = [];              Array<int> a = [10, 20, 30];
List<int> ys = [1, 2, 3];       Array<int> z = [0; 5];
Map<str, int> m = {};           Map<str, int> n = {"a": 1, "b": 2};
```

`[v; n]` evaluates `v` **once** and puts the same value in every slot. For a
reference type that means `n` slots pointing at one object, which is what
sharing already means everywhere else in the language.

A literal has no type of its own — it takes one from where it is written.
That is the only place in the language where type information flows inwards,
and it flows exactly one level: a literal needs a declared type, a field, a
parameter or a return type above it. `[].size()` is an error, because there
is nothing above it to ask.

`Array` has no uninitialised slot: there is no null, so a length and a fill
value arrive together or the length is the number of elements written.

`Map` keys are `int` or `str`. Hashing a user type would need a `Hashable`
interface, which does not exist.

Indexing with `[]` works on `Array` and `List`, reads and writes both. An
index outside `0 .. len-1` **traps**.

Methods:

| Receiver | Method | Meaning |
|---|---|---|
| `Array`, `List`, `Map`, `str` | `size()` | element count |
| `Array`, `List` | `contains(v)` | `int`, `float`, `bool` and `str` elements only |
| `Array`, `List` | `index_of(v)` | `Option<int>` — `None` if it is not there |
| `Array`, `List` | `reverse()` | in place |
| `Array`, `List` | `sort()` | in place, ascending; `int` and `str` only |
| `List` | `push(v)` | append |
| `List` | `pop()` | remove and return the last element; traps if empty |
| `List` | `insert(i, v)` | at `i`; `i == len` appends, beyond that traps |
| `List` | `remove_at(i)` | remove and return the element at `i` |
| `List` | `clear()` | drop every element |
| `Map` | `set(k, v)` | insert or replace |
| `Map` | `get(k)` | `Option<V>` — `None` if the key is not there |
| `Map` | `contains(k)` | is the key present |
| `Map` | `remove(k)` | delete if present |
| `Map` | `clear()` | remove every entry |
| `Map` | `keys()` | a fresh `List<K>` of the keys |
| `Map` | `values()` | a fresh `List<V>` of the values |

**One name for each question.** `size()` answers "how big" on a `str`, an
`Array`, a `List` and a `Map`; `contains()` answers "is it in there" on all
of them. There is no `len`, and no `has`. A free function for strings and a
method for everything else was two spellings of one idea.

On a `Map`, `contains` asks about a **key** — the same thing `get` and
`remove` take.

`contains` compares the way `==` does — `str` by value, `int` and `bool`
directly — and refuses a user type, which would need its own comparison.
`index_of` returns `Option<int>`. The runtime finds `-1` for absent and the
compiler turns that into a `None` before anything can see it — the sentinel
never reaches the language.

`sort` is a **stable** merge sort — equal elements keep their order — because
sorting by one key and then another is the ordinary way to get a compound
order, and that only works if the second sort leaves ties alone. Strings
order lexicographically, and a prefix sorts before what extends it. It
handles `int` and `str`; a user type already spells its order as `cmp`, but
calling back into generated code needs a function reference in the IR, which
does not exist yet and is the same thing closures will need.

`remove_at` is spelled that way because Java has both `remove(int)` and
`remove(Object)` and the overload is a standing trap. One name, and it says
which it means.

`get` returns an `Option<V>` (§3.7a). It used to trap, because with no way
to express absence the only honest choices were to trap or to make every read
go through a separate check — and an `Option` cannot be forgotten the way a
preceding `contains` can, nor does it hash the key twice.

A map is enumerated through `keys()` and `values()`, which each build a new
`List`. It is not iterable directly: its slots are sparse, so a loop over
them needs a cursor that skips, which is not the shape `for ... in` has.
Building a list makes the allocation visible at the call site instead of
hiding it in the loop. **The order is the table's, not insertion order**, and
it changes when the table rehashes — do not depend on it.

### 3.10 `bytes`

A growable, **mutable** run of octets: the buffer a read fills and a codec
works in. It is a builtin like `str` — lowercase, a type keyword, no type
argument — and a reference type like a `List`.

```c
bytes empty = [];                   // the sequence literal, typed by its place
bytes hi = [104, 105];
bytes zero = [0; 4096];
bytes text = "GET ".to_bytes();     // from text: a copy, and it cannot fail
```

**A byte is an `int` from 0 to 255.** `b[i]` reads one, `b[i] = v` writes
one, and `for (int x in b)` iterates them. There is no byte type: the
language has one integer type, and a byte is a value of it. Storing a value
outside the range **traps** rather than truncating — 256 silently becoming
0 is a wrong answer in exactly the code, checksums and codecs, least able to
notice. A constant outside the range is a compile error. An index outside
`0 .. size-1` traps, as on a `List`.

The literal is the `List` literal, and the same rule gives it a type (§3.9):
it needs a declaration, a parameter, a field or a return above it. That is
the whole construction story. There is **no `b"..."`**: a literal of a
mutable type cannot be one shared immortal object the way a string literal
is (§7.3), so it would allocate on every evaluation while looking like a
constant. Text that should become bytes says so with `to_bytes()`, and the
allocation is visible where it happens.

The representation is one byte per element — a length, a capacity and a
`uint8_t` buffer that `push` doubles — so a 4096-byte buffer is 4096 bytes.

Methods. The names are `str`'s wherever `str` asks the question, with the
same meaning over octets and **`bytes` arguments, never `str`**; crossing
between the two is always written out. On top of those, what a buffer needs
and an immutable `str` cannot have:

| | |
|---|---|
| `b.size()` | length in bytes |
| `b.push(v)` | append one byte; **traps** outside 0..255 |
| `b.pop()` | remove and return the last byte; **traps** if empty |
| `b.clear()` | size 0, keeping the buffer for reuse |
| `b.extend(other)` | append another `bytes` in place; `b.extend(b)` doubles `b` |
| `b.substr(from, to)` | a new `bytes`, half-open; **traps** if out of bounds |
| `b.contains(sub)` | run-of-bytes search; an empty needle is found |
| `b.index_of(sub)` | `Option<int>` |
| `b.starts_with(p)`, `b.ends_with(p)` | |
| `b.split(sep)` | `List<bytes>`; keeps empty fields, **traps** on an empty separator |
| `b.trim()` | ASCII whitespace from both ends |
| `b.to_upper()`, `b.to_lower()` | ASCII only; a byte above 127 is left alone |
| `b.repeat(n)` | |
| `b.hex()` | a `str`: lowercase, two digits a byte, no separator |
| `b.utf8()` | `Option<str>` — the text, if `b` is valid UTF-8 |
| `xs.join(sep)` | on a collection of `bytes`, the inverse of `split` |

`push`, `pop`, `clear`, `extend` and index assignment change `b`; every
other method returns a new object and leaves `b` alone. `contains` and
`index_of` take a `bytes` because that is what they take on `str` — they
find a run, not an element.

`==` and `!=` compare by value, as on `str`. There is no `+`: appending is
`extend`, in place, which is what a buffer is for. There is no ordering,
because `str` has none.

**Decoding is `utf8()`, not `to_str()`.** `to_str` is the name `print` and
`str(v)` find a type's text by (§6.6), so it has to be infallible; decoding
can fail, so it answers with an `Option`, the way `parse_int` does. It is
strict UTF-8: an overlong form, a surrogate, a code point past U+10FFFF or a
truncated sequence is `None`, never a replacement character — a decoder that
repairs hides the bug that produced its input. `hex()` is the other text a
`bytes` has, and it cannot fail.

So **`print(b)` and `str(b)` are refused**, naming `hex()` and `utf8()`. A
run of octets has no one text form — hex, decoded UTF-8 and an escaped
literal are all reasonable — and picking one for the program would freeze a
guess.

`clone(b)` is a copy that shares nothing: its bytes are not references, so
shallow and deep are the same. Sending a `bytes` over a channel moves it,
under the same rule and the same run-time uniqueness check as anything else
(§8.3); it holds no references, so the check is its count alone.

A `distinct bytes` is still a `bytes`, as a distinct collection is still a
collection (§3.6), and converts back with `bytes(v)`. A `bytes` cannot be a
`Map` key.

**Open question: `str` holds arbitrary bytes.** Nothing checks that a `str`
is valid UTF-8 — a file read can put anything in one, and `substr` can cut a
character in half. Oro made `str` always valid and `bytes` the only home for
raw octets; this language has not decided. Now that `bytes` exists the Oro
rule is reachable (the file reader would return `bytes`, and `utf8()` would
be the one way in), but it changes what `io.read` returns and what
`substr` may do, so it waits for the `io` rewrite rather than riding in with
the type.

---

## 4. Declarations

### 4.1 Variables

Type-first, C and Java shaped:

```c
int x = 5;
const int limit = 10;
Point p = Point(1, 2);
```

The initialiser is **mandatory**. There is no declaration without a value,
because there is no zero value to give one.

`const` forbids assignment to the name. It says nothing about the object: a
`const List<int>` may not be reassigned, and may still be pushed to.

**Shadowing is not allowed.** A declaration whose name is already in scope is
an error telling you to rename one. A name means one thing for the whole
region a reader can see it in. Nothing shadows a type name either, nor a
builtin function (§6.6) in any module, nor a module the file imports.

### 4.2 Functions

```c
int add(int a, int b) {
    return a + b;
}
```

A parameter may have a default, which makes it optional:

```c
int scale(int v, int by = 2) { return v * by; }
```

A default — of a parameter or of a field (§3.3) — is evaluated afresh at
each call or construction that leaves it out, but it **belongs to the
declaration**: it is checked with the declaring module's names and privacy,
so a `pub` type may default a field to one of its module's private types,
and it sees no local, parameter or receiver field of whoever is calling. It
means what it meant where it was written.

**Mandatory parameters are positional. Optional ones are named.** There is
no choice about it, so one call is written exactly one way:

```c
scale(10);              // 20
scale(10, by: 3);       // 30

scale(10, 3);           // error: `by` is optional, so it is named
scale(v: 10, by: 3);    // error: `v` is mandatory, so it is positional
```

Positional arguments come before named ones. Named arguments may appear in
any order, since they carry their own name:

```c
int volume(int w, int h = 1, int depth = 1) { return w * h * depth; }
volume(5, depth: 3, h: 2);      // order among named arguments is free
```

The reason for the rule is that a reader should never have to count commas to
find which parameter a value lands in, and should never meet the same call
spelled two ways.

A function returning non-`void` must return on every path; falling off the
end is a compile error. A `while (true)` with no `break` out of it is a path
that never ends, so a function may finish with one and nothing after it —
and, like a statement after a `return`, a statement after one is an
unreachable-statement error. Only the literal `true` counts: there is no
constant folding, and a rule a reader can check by eye beats one that needs
the compiler's arithmetic. There are no multiple return values yet.

### 4.3 Methods

A method is a function whose name is qualified by its receiver type:

```c
int Rect.area() {
    return w * h;
}
```

The receiver is **not named**. Its fields are in scope bare — `w`, not
`self.w` — because the receiver type is already in the name. There is no
`this` and no `self`.

A method may be declared anywhere in the file, including before its type.

---

## 5. Statements

### 5.1 Expression statement

```c
f(x);
```

**A discarded `Result` is an error**, not a warning — the language has no
warnings and should not grow the category for one thing. Handle it with
`match`, propagate it with `?`, or bind it to a name. An `Option` is exempt:
ignoring one is often reasonable, and what it reports is absence rather than
something going wrong.

### 5.2 Assignment

```c
x = 1;
p.y = 2;
xs[0] = 3;
```

The left side is a name, a field, or an index. Nothing else is assignable.

### 5.3 `if`

```c
if (cond) { ... } else if (other) { ... } else { ... }
```

The condition must be `bool` — there is no truthiness, so `if (n)` on an
`int` is an error. Braces are **mandatory**.

### 5.4 `while`

```c
while (cond) { ... }
```

### 5.5 `for ... in`

The only `for`. There is no three-clause form.

```c
for (int v in xs) { ... }
```

Iterates an `Array`, a `List` or a `bytes` — whose elements are `int`s
(§3.10). The loop variable is a fresh binding each iteration and is
**borrowed** from the collection; it is not a copy.

Mutating the collection's length while iterating it is not defined and is not
checked. Do not.

### 5.6 `match`

The only way to read an enum's payload:

```c
match (s) {
    case Empty: {
        print(0);
    }
    case Circle(int r): {
        print(3 * r * r);
    }
    case Rect(int w, int h): {
        print(w * h);
    }
}
```

Bindings are **type-first**, like every other binding in the language, and
they bind the payload positionally. A binding is borrowed from the enum,
which stays alive for the whole `match`.

**Exhaustive**: every variant must have a case. **No fallthrough** — one case
runs. **No `default`**, so adding a variant to an enum is a compile error at
every `match` that has to learn about it, which is the entire reason to have
the compiler check this. A `default` can be added later without breaking
anything; taking one away could not.

Duplicate cases, unknown variants, and a case that binds the wrong number of
values are all errors.

### 5.7 `break`, `continue`

Innermost enclosing loop. There are no labels.

### 5.8 `return`

```c
return;         // in a void function
return expr;
```

### 5.9 `spawn`

```c
spawn worker(ch, 1);
```

Calls the named function on a new thread. `spawn` is a statement, not an
expression: there is no handle and no join. The program waits for every
spawned thread before exiting (§8.1).

---

## 6. Expressions

### 6.1 Operators, tightest first

| Level | Operators | Associativity |
|---|---|---|
| 11 | `-x` `!x` `~x` | prefix |
| 10 | `*` `/` `%` | left |
| 9 | `+` `-` | left |
| 8 | `<<` `>>` | left |
| 7 | `&` | left |
| 6 | `^` | left |
| 5 | `\|` | left |
| 4 | `<` `<=` `>` `>=` | left |
| 3 | `==` `!=` | left |
| 2 | `&&` | left |
| 1 | `\|\|` | left |

C's precedence for the operators C and Python agree on, and **Python's for
the bitwise ones**: `|` loosest, then `^`, then `&`, then the shifts, every
one of them tighter than every comparison, and the shifts looser than `+`
and `-`. So `x & 1 == 0` is `(x & 1) == 0` — in C it is `x & (1 == 0)`, a
trap every C programmer has fallen into once — and `1 << n + 1` is
`1 << (n + 1)`, as in both. `&&` and `||` short-circuit.

On the built-in types, `==` works on `int`, `bool`, `str` and `bytes`; `str`
and `bytes` compare **by value**. On a user type an operator is a method call (§6.2).

Arithmetic on a distinct type yields **that same distinct type**, not the
base — `Price + Price` is a `Price`. Mixing two distinct types, or a distinct
type and its base, is an error; convert explicitly (§3.6).

**Integer arithmetic traps on overflow** — `+`, `-`, `*`, `/`, `%` and
unary `-`, on every target. Division and remainder by zero trap too, as does
`INT_MIN / -1`. The operators never wrap. Hashes and PRNGs are defined modulo
2^64 and need wrapping, so it exists — as the methods `wrapping_add`,
`wrapping_sub` and `wrapping_mul` (§6.5a), whose names put every wrap in
plain sight at the place it happens.

Overflow trapping is why an overflowing program cannot have a Go twin in the
corpus: Go wraps, so the twin would be confidently wrong. Those cases live in
`corpus/traps/` instead.

#### Bit operators

`&` `|` `^` `<<` `>>` and unary `~` apply to **`int` only**. A `bool` is not
an integer — `&&`, `||` and `!=` are its operators, and the diagnostic says
so. A `float` operand is an error rather than a truncation: a float's bits are
reached through `to_bits()` (§6.5a), which says that a reinterpretation is
what is meant. Both operands must have the same type; a distinct `int` stays
distinct, as under `+`. None of them is overloadable (§6.2).

They are operations on the 64-bit two's-complement pattern, not arithmetic
on the number, so the overflow rule does not reach them:

  - `&`, `|`, `^` and `~` **never trap**. `~x` is `-x - 1` for every `x`,
    including `INT_MIN`, because nothing overflows.
  - `>>` is an **arithmetic** shift: the sign bit is copied in, as in Python,
    so `-7 >> 1` is `-4` (rounded toward negative infinity). A logical shift
    is `(x >> n) & ~(-1 << (64 - n))`.
  - `<<` **discards** the bits shifted out of the top and does not trap.
    It is a bit operation, and a shift that trapped whenever a bit fell off
    could not build a mask or rotate a hash. The cost is that **`x << n` is
    not `x * 2^n`** once bits fall off — `3 << 63` is `INT_MIN` — which is
    what `*` is for, and `*` traps.
  - **A shift count outside `0..63` traps**, negative counts included. In C
    it is undefined behaviour, and x86 masks the count to six bits, so
    `1 << 64` would quietly be `1`. A negative count is not a shift the
    other way.

The emitted C relies on nothing C leaves open: `<<` is done on `uint64_t`
(left-shifting a negative signed value is undefined), the result comes back by
bit pattern, and the sign extension of `>>` is spelled out rather than left
to the implementation.

There is no `&=`, `<<=` or any other augmented assignment, because there is
no `+=`; the bit operators do not get a spelling arithmetic lacks.

There is no unsigned type. A `uint64` algorithm is written on `int` with the
bit operators and the wrapping methods — equal bits, and the one difference,
the logical right shift, is the mask above.

### 6.2 Operators on user types

An operator applied to a user type is **desugared to a method call**. Define
the method and the operator works; do not and it is a compile error naming
the method it wanted.

| Operator | Method | Signature |
|---|---|---|
| `a + b` | `add` | `T T.add(T other)` |
| `a - b` | `sub` | `T T.sub(T other)` |
| `a * b` | `mul` | `T T.mul(T other)` |
| `a / b` | `div` | `T T.div(T other)` |
| `a % b` | `rem` | `T T.rem(T other)` |
| `a == b`, `a != b` | `eq` | `bool T.eq(T other)` |
| `a < b`, `<=`, `>`, `>=` | `cmp` | `int T.cmp(T other)` |

```c
type V { int x; int y; }
V    V.add(V o) { return V(x + o.x, y + o.y); }
bool V.eq(V o)  { return x == o.x && y == o.y; }
int  V.cmp(V o) { return (x + y) - (o.x + o.y); }
```

`!=` is `eq` negated — you cannot define the two inconsistently. All four
orderings go through a **single `cmp`** returning negative, zero or positive,
rather than four methods, for the same reason: one implementation is a total
order, and four can disagree.

There is no way to define an operator that has no entry above, and no way to
change an operator's meaning on a built-in type. The bit operators are
deliberately absent: they are defined on the bits of an `int`, and a user
type that wants something like them should name it as a method.

### 6.3 Postfix

`a.field`, `a.method(..)`, `a[i]`, `a?`, and chains of them in any order:
`xs[0].name`, `grid[i][j]`, `m.get(k)?.size()`.

#### `?` — propagate a failure

```c
Result<int, str> quarter(int n) {
    int a = half(n)?;       // or return the Err from here
    int b = half(a)?;
    return Result<int, str>.Ok(b);
}
```

`e?` gives the payload of an `Ok` or a `Some`, and otherwise **returns** the
failure from the enclosing function. It is sugar for a `match` whose failing
arm returns unchanged.

  - It is only allowed in a function that returns an `Option` or a `Result`,
    and the two must be the same kind — `?` on an `Option` needs a function
    returning an `Option`.
  - For a `Result`, the **error types must match exactly**. There is no
    conversion mechanism, and inventing one here would be a large feature
    hiding inside a small one. Relaxing this later cannot change what an
    existing program means.
  - The success types need not match: the failure is rebuilt at the
    enclosing function's own return type.

It hides a return, which is a fair thing to dislike — but it hides *one*
specific return, always in the same place, and the signature still says the
function can fail.

### 6.4 Construction

```c
Point(1, 2)             // mandatory fields, positional
Point(1, 2, label: "a") // a defaulted field, named
Array<int> a = [0; 4]
List<str> l = []
Map<str, int> m = {}
Chan<int>(8)
```

### 6.5 Methods on `str`

**Everything here works in BYTES, not characters.** `size()` is a byte count,
`substr` takes byte offsets, and the case conversions touch only ASCII. Go
makes the same choice, and it is the honest one for a type that carries
bytes — the alternative is pretending to understand an encoding the language
has no other opinion about. `"é".size()` is 2.

| | |
|---|---|
| `s.size()` | length in bytes |
| `s.substr(from, to)` | half-open byte range; **traps** if out of bounds |
| `s.contains(sub)` | substring search; an empty needle is found |
| `s.index_of(sub)` | `Option<int>`, a byte offset |
| `s.starts_with(p)`, `s.ends_with(p)` | |
| `s.split(sep)` | `List<str>`; keeps empty fields, **traps** on an empty separator |
| `s.trim()` | ASCII whitespace from both ends |
| `s.to_upper()`, `s.to_lower()` | ASCII only |
| `s.repeat(n)` | |
| `s.byte_at(i)` | one byte as an `int`; **traps** out of range |
| `s.parse_int()` | `Option<int>` — the whole string, decimal, no surrounding space |
| `s.parse_float()` | `Option<float>` |
| `s.to_str()` | itself |
| `s.to_bytes()` | a `bytes` copy of the same octets; cannot fail (§3.10) |

And on a collection of `str`:

| | |
|---|---|
| `xs.join(sep)` | the inverse of `split` — the parts always rejoin |

A `str` is immutable, so every one of these returns a new string.

### 6.5a Methods on numbers

| | |
|---|---|
| `v.to_str()` | on `int`, `float` and `bool` (§6.6) |
| `a.wrapping_add(b)`, `a.wrapping_sub(b)`, `a.wrapping_mul(b)` | `int`: two's-complement, modulo 2^64; never trap |
| `f.to_bits()` | `float` → `int`: the IEEE-754 bit pattern |
| `float.from_bits(n)` | `int` → `float`: the inverse, every pattern accepted |

These are here because the language cannot write them itself — the operators
trap by design, and no arithmetic reaches a float's bits. Everything else a
number might answer is a library's job.

The wrapping methods take an argument of the receiver's type and return that
type, so a distinct `int` stays distinct, the way it does under `+`. They
exist for hashes and PRNGs:

```c
int h = -3750763034362895579;               // FNV-1a's offset basis, as an int
h = (h ^ s.byte_at(i)).wrapping_mul(1099511628211);
```

`to_bits` and `from_bits` are a reinterpretation, not a conversion:
`1.0.to_bits()` is `4607182418800017408` (`0x3FF0000000000000`), `-0.0`
gives `INT_MIN` where `0.0` gives `0` — the one way to tell the two zeros
apart — and a NaN's payload survives the round trip. They exist so float
formatting and parsing can be written in the language.

The spelling follows the rule §6.6 gives for conversions. `to_bits` is a
method, like `to_str`, because the **source** is what varies and a method
dispatches on its receiver. `from_bits` is a **static method on the target**,
like the `int.parse(s)` that section anticipates, because its source is
always an `int` and it is the result type the name has to state — an
`n.to_float_bits()` on `int` would put the float's name on the int. `to_`
and `from_` make them a visible pair, the one Rust uses.

### 6.6 Built-in functions

| | |
|---|---|
| `print(x)` | `int`, `float`, `bool`, `str`, or anything with `to_str`; one argument, newline-terminated. Not `bytes` (§3.10) |
| `concat(a, b)` | joins two `str` |
| `clone(x)` | a **shallow** copy |
| `int(x)`, `bool(x)`, `str(x)`, `bytes(x)` | convert a distinct value to its base |
| `send(ch, v)`, `recv(ch)`, `close(ch)` | channels (§8) |

`print` selects its runtime helper from the static argument type. That is not
user-visible function overloading, which does not exist.

A **user type says how it prints by having a `to_str` method** — found by
name, the way `add`, `eq` and `cmp` already are. `str(v)` is the same call,
so the conversion family reads alike whatever it is applied to. Without one,
`print` refuses and names the method it wanted.

There is deliberately no built-in `ToStr` type. None is needed: interfaces
are structural, so a program that wants to pass "anything printable" around
declares

```c
interface ToStr { str to_str(); }
```

itself, and every type with the method satisfies it with no further
ceremony. Through such a value the call goes via the vtable rather than
directly. The same shape gives `to_int`, `to_float` and `to_bool` — all of
them dispatch on the SOURCE, which is what an interface does.

**Parsing is not this.** `"42"` to an int reads text and can fail; the source
is always `str` and it is the *target* that varies, which single dispatch
cannot express. That is a **static method** — `int.parse(s)`, `Price.parse(s)`
— and it waits on what an error's type should be.

`clone` is shallow — the copy holds the same references, each retained once
more. Deep copying would have to decide what copying each field means, which
is a question only the program can answer. `clone` works on a `str`, a
`bytes`, an `Array`, a `List` and a struct; a channel and an interface value
cannot be cloned.

**`int`, `float` and `bool` answer `to_str`** — so `v.to_str()` means the
same thing whatever `v` is, and `str(v)` is that same call. A number finally
composes into a message. Past `to_str`, only the few methods of §6.5a.

**Parsing returns an `Option`, not a `Result`.** The question a built-in parse
answers is "did it parse", which is Option-shaped; it is lossy on purpose,
since "not a number" and "out of range" are both `None`. A built-in cannot
return a library's own error type — the same constraint that made `Option`
and `Result` built in — and a library that needs the distinction builds one
on `byte_at` and declares its own error. See `docs/stdlib-decision.md`.

---

## 7. Memory

### 7.1 Reference counting

Every reference-typed object carries a count. There is no garbage collector
and no cycle detector: **a cycle leaks**. That is the trade — predictable,
immediate destruction at the cost of a shape the program has to avoid.

Refcount operations are non-atomic (§8.3).

### 7.2 Ownership protocol

  - **Arguments are borrowed.** Passing a value you already hold to a
    function costs no refcount traffic.
  - **Returns are owned (+1).** The caller receives a reference and is
    responsible for it.
  - A value stored into a field, an element or a map entry is **retained**
    by the container.

The compiler inserts every retain and release. There is no manual
`retain`/`release`, and no way to write one.

### 7.3 Immortal values

A string literal is allocated once, statically, with a count that never
reaches zero. Retaining and releasing one is a no-op, so a literal in a hot
loop costs nothing.

### 7.4 Traps

A trap prints a message to stderr and aborts. It is not catchable — there are
no exceptions and no error values yet.

Trapping conditions:

  - integer overflow, on any arithmetic operator including unary minus —
    not on a bit operator, and not in a `wrapping_` method
  - a shift count outside `0 .. 63`
  - division or remainder by zero, and `INT_MIN / -1`
  - an index outside `0 .. len-1`
  - `Map.get` on a key that is not there
  - `pop` on an empty list or an empty `bytes`
  - storing a value outside 0..255 into a `bytes` (§3.10)
  - `recv` on a channel that is closed and drained
  - a length or capacity too large to allocate
  - a uniqueness violation at a thread boundary (§8.3)

---

## 8. Concurrency

### 8.1 Threads

`spawn f(..)` runs `f` on a new OS thread. It is **fire-and-forget**: the
spawning scope does not wait, there is no handle, no join, no cancellation
and no thread identity. The program waits for every spawned thread before
exiting, and that is the only synchronisation there is — a channel is how a
spawned thread reports back.

This is a decision rather than an omission. A scope that joins its own
children (Loom's shape) was considered and not taken; adding one later is
additive, where removing one would not be.

These are OS threads today. Green threads are stage 3 of
`docs/concurrency-decision.md`; the surface here does not change when they
arrive.

### 8.2 Channels

```c
Chan<int> ch = Chan<int>(8);
spawn worker(ch, 1);
print(recv(ch));
```

`Chan<T>(cap)` is bounded. `send` blocks when full, `recv` blocks when empty,
`close` wakes every waiter. `recv` on a closed and drained channel traps.

A channel is **aliased, not moved** — it is how threads share. Channels are
immortal in v0.

### 8.3 Moves

Refcounts are **non-atomic**, which is only sound while one thread can reach
a value at a time. So `send` and `spawn` **move** a reference: the sender
gives it up, the receiver acquires it, with no retain or release between.
Using a moved local afterwards is a compile error.

Only a value the current block **owns** may be moved: an owned temporary, or
a local declared in that block. Refused:

  - a **parameter**, because it is borrowed and the caller still holds it;
  - a local declared in an **enclosing** block, because a loop body or one
    arm of an `if` would move the same reference twice.

The fix for both is `clone(x)`, which hands over a copy.

Retaining instead of moving is not an alternative. Two threads on one
non-atomic counter is the defect; a retain before the handoff only makes the
race start at 2.

Aliasing the compiler cannot see is caught at run time, and the check is
**transitive**. A unique wrapper is not enough:

```c
type Holder { str s; }
str shared = concat("ab", "cd");
spawn eat(Holder(shared));      // each Holder is unique -- the str is not
```

So the whole graph reachable from the moved value must be unreachable from
anywhere else. The check counts the references into each reachable object
from within the graph and requires that to equal its refcount. An object
reached twice *inside* the graph is fine — one thread still owns all of it.
Immortal objects are skipped, which is why a literal and a channel cost
nothing here.

It traps rather than corrupting the heap, and costs time proportional to the
graph, paid once per crossing.

---

## 9. Not in the language

Stated so the absence is a decision and not an oversight:

  - exceptions and unwinding; a fault traps, and a recoverable failure is a
    `Result` (§3.7a)
  - multiple returns — a `Result` or an enum carries what a second return
    value would have
  - closures, function values, lambdas
  - inheritance, method overriding, abstract types
  - defining an operator outside the fixed set of §6.2, or changing one on a
    built-in type
  - function overloading — one name, one function
  - shadowing — see §4.1
  - **null** — every declaration initialises, and absence is `Option<T>`
  - type aliases — see §3.6
  - unsigned and sized integer types
  - `switch`, ternary `?:`, three-clause `for`, labelled break
  - variadic functions
  - constraints on type parameters
  - reflection, runtime type queries, downcasting from an interface
  - wildcard imports, import aliases, and import cycles — see §2.1
  - unsafe, raw pointers, manual allocation
  - a garbage collector, and therefore cycle collection: **a cycle leaks**

---

## 10. Grammar

EBNF. `{ x }` is zero or more, `[ x ]` optional. This is normative: a program
the grammar does not derive is not in the language, whatever the compiler
happens to accept, and a compiler that refuses a derivable program is wrong
unless a later section of this document forbids it on non-grammatical
grounds (types, privacy, shadowing).

```ebnf
program     = { import } { item } ;
import      = "import" IDENT ";" ;                    (* before any item *)
item        = [ "pub" ] decl_item | stmt ;            (* stmt: entry file only *)
decl_item   = typedecl | interface | enumdecl | distinct | func | prim ;

typedecl    = "type" IDENT [ tparams ] "{" { field ";" } "}" ;
interface   = "interface" IDENT [ tparams ] "{" { sig ";" } "}" ;
enumdecl    = "enum" IDENT [ tparams ] "{" { variant ";" } "}" ;
variant     = IDENT [ "(" type { "," type } ")" ] ;
distinct    = "distinct" type IDENT ";" ;
field       = type IDENT [ "=" expr ] | type ;        (* bare type = embedded *)
sig         = type IDENT "(" [ params ] ")" ;

func        = [ "static" ] type [ IDENT [ tparams ] "." ] IDENT [ tparams ]
              "(" [ params ] ")" block ;              (* static needs a receiver;
                                                         receiver tparams: §3.8 *)
prim        = "prim" type IDENT "(" [ params ] ")" ";" ;  (* stdlib source only, §10.1 *)
tparams     = "<" IDENT { "," IDENT } ">" ;
params      = param { "," param } ;
param       = type IDENT [ "=" expr ] ;

type        = "int" | "float" | "bool" | "str" | "bytes" | "void"
            | [ IDENT "." ] IDENT [ "<" type { "," type } ">" ] ;

block       = "{" { stmt } "}" ;
stmt        = decl | assign | eval | if | while | forin | match
            | "return" [ expr ] ";" | "break" ";" | "continue" ";"
            | "spawn" IDENT args ";" ;
match       = "match" "(" expr ")" "{" { case } "}" ;
case        = "case" IDENT [ "(" bind { "," bind } ")" ] ":" block ;
bind        = type IDENT ;

decl        = [ "const" ] type IDENT "=" expr ";" ;   (* always initialised *)
assign      = lvalue "=" expr ";" ;
lvalue      = IDENT | expr "." IDENT | expr "[" expr "]" ;
eval        = expr ";" ;
if          = "if" "(" expr ")" block [ "else" ( if | block ) ] ;
while       = "while" "(" expr ")" block ;
forin       = "for" "(" type IDENT "in" expr ")" block ;

expr        = unary { binop unary } ;                 (* precedence per 6.1 *)
binop       = "||" | "&&" | "==" | "!=" | "<" | "<=" | ">" | ">="
            | "|" | "^" | "&" | "<<" | ">" ">"        (* ">" ">": adjacent, §1.6 *)
            | "+" | "-" | "*" | "/" | "%" ;
unary       = [ "-" | "!" | "~" ] postfix ;
postfix     = atom { "." IDENT [ args ] | "[" expr "]" | "?" } ;
atom        = INT | FLOAT | STR | "true" | "false"
            | IDENT [ args ]
            | type args                               (* construction *)
            | type "." IDENT [ args ]                 (* enum variant, static method,
                                                         float.from_bits *)
            | seqlit | maplit
            | "(" expr ")" ;

seqlit      = "[" [ expr { "," expr } ] "]"           (* List, Array or bytes, §3.9 *)
            | "[" expr ";" expr "]" ;                 (* value; count *)
maplit      = "{" [ expr ":" expr { "," expr ":" expr } ] "}" ;

args        = "(" [ arg { "," arg } ] ")" ;
arg         = expr | IDENT ":" expr ;                 (* positional before named *)
```

Lexical: `INT` is decimal digits. `FLOAT` has a dot with digits on **both**
sides — `1.0`, never `1.` or `.5` — and no exponent, so whether a literal is
a float is decided by one character. `IDENT` is a letter or `_` followed by
letters, digits or `_`, and **may not begin with `__`**, which is reserved
(§10.1).

**No trailing commas**, anywhere a list is closed by a bracket. The compiler
currently accepts `[1, 2,]` and `{"a": 1,}`; that is a bug, and it is
refused rather than extended to arguments and parameters because allowing
trailing commas later is additive and forbidding them later is not.

### 10.1 The standard library's seam

`prim` and `__`-prefixed names are available only to modules the compiler
ships in `lib/`. To any other program they do not exist: `prim` is refused
and a `__` name is a reserved-name error. See `docs/stdlib-seam.md`.
