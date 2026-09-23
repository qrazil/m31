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
    static   str       this      true      type      void      while

Keywords are reserved: none may be used as an identifier. `Array`, `Chan`,
`List` and `Map` are not keywords — they are predeclared type names, and are
reserved only in the sense that nothing may shadow a type name (§4.1).

### 1.5 Literals

| | |
|---|---|
| Integer | `0`, `42`, `1_000_000`; hex `0x1F`, octal `0o17`, binary `0b101`. No sign — `-1` is unary minus applied to `1`. The rules are below. |
| Float | `1.0`, `3.14`, `2.5e3`. **Always a dot with digits on both sides** — not `1.` and not `.5`. An exponent only after the dot form: `1.0e9`, not `1e9`. A literal too large **or too small** to represent is an error: one that parses to exactly zero has lost its whole value. Arithmetic that underflows at run time is ordinary IEEE. |
| Boolean | `true`, `false` |
| String | `"..."`, with the escapes below. Any other character stands for itself, byte for byte — control characters, a BOM and combining marks included — except a raw newline, which is an error: a literal ends on its line. |

| Escape | Bytes |
|---|---|
| `\\` `\"` | a backslash, a quote |
| `\n` `\t` `\r` `\0` | 0A, 09, 0D, 00 |
| `\xNN` | the one byte `NN`: exactly two hex digits, either case, **00 to 7F** |
| `\u{N}` | the Unicode scalar value `N`, one to six hex digits, as its UTF-8 bytes |

Anything else after a backslash is an error, including C's `\a` `\b` `\f`
`\v` (write `\x07` and so on) and C's and Go's fixed-width `\u00e9` (write
`\u{e9}`). `\x` takes exactly two digits, where C takes as many as follow:
`"\x411"` is `A1`. `\u{}` refuses a surrogate and anything past `10FFFF`,
which have no UTF-8 form.

**A string literal is always valid UTF-8**, as every `str` is (§3.2a). The
source is, `\u{}` produces only scalar values, and `\x` stops at 7F — a lone
byte from 80 up is not text. Raw octets are a `bytes`, such as `[233]`
(§3.10).

A `str` carries its length, so `\0` is an ordinary character:
`"a\0b".size()` is 3. Strings are not NUL-terminated.

String literals are **immortal**: their refcount never reaches zero, so they
are never freed (§7.3).

There is no `bytes` literal. A `bytes` is written with the sequence literal,
`[104, 105]`, or converted from text, `"hi".to_bytes()` (§3.10).

**Integer literals.** Decimal, or a prefix and digits in its base: `0x`
hex, `0o` octal, `0b` binary. `_` may stand anywhere after the first digit
— of the number, or of the digits after a prefix — and means nothing:
`1_000_000`, `0xFFFF_FFFF`, `0b1010_0101`. A digit outside the base, or a
letter run on the end, is an error, not a suffix.

  - **The prefix is lowercase; hex digits are either case.** `0X1F` is an
    error. One spelling per base, because `0O17` is hard to tell from
    `0017` and a formatter that keeps spellings (below) could never bring
    two files that chose differently together. The digits are the other way
    round because the constants people copy are published in both —
    RFCs in upper case, C sources in lower — and forcing one would turn
    copying a constant into transcribing it.
  - **No leading zero on a decimal.** `017` is an error that suggests `17`
    or `0o17`. C, and JavaScript outside strict mode, read it as octal 15;
    a reader who does not know that sees seventeen, and one who does
    cannot tell whether the author did. `0` alone is fine, and so is
    `0.5`: the trap is octal integers only.
  - **A decimal literal is a number; a prefixed literal is 64 bits.** A
    decimal must fit in `int`. A prefixed literal may use all 64 bits,
    which are the `int`'s bits, top bit the sign: `0x7FFF_FFFF_FFFF_FFFF` is
    the largest `int`, `0x8000_0000_0000_0000` the smallest, and
    `0xFFFF_FFFF_FFFF_FFFF` is `-1`. A base other than ten is chosen to
    write bits, and `int` is the only integer type, so there is no unsigned
    one for a mask with the top bit set to belong to. Rust and Go refuse
    such a literal because they have `u64` to send it to; Java, which has
    no unsigned `long` either, allows it, for the same reason as here.
    Without it, published constants — FNV's offset basis
    `0xcbf29ce484222325`, a float's sign bit — would have to be transcribed
    into negative decimals nobody can check against the source. More than
    64 bits is an error in any base, and a decimal too large for `int` but
    within 64 bits is refused with the hex spelling of its bits.
  - **There are no hex floats.** `0x1.8` is an error; a float is decimal.

The formatter prints every integer literal as it was written: `0o755`
stays `0o755` and `1_000` stays `1_000`, not `493` and `1000`.

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

A file is a sequence of five kinds of item, in any order:

  - a **type declaration** — `type`, `interface`, or `distinct`
  - a **function declaration**
  - a **method declaration**
  - a **constant declaration** — `const` at the top level (§4.5)
  - a **top-level statement**

Top-level statements run, in source order, as the program. Declarations do
not: a file that is all declarations is a program that does nothing.

Forward references are fine. Types, functions and constants are collected
before any body is checked, so an item may name one declared later in the
file.

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
    everything a module declares — functions, types, methods, fields
    (§3.3) and constants (`lib.MAX`, `lib.TABLE[0]`, `lib.TABLE.size()`) —
    not only functions. An enum's variants and an interface's
    methods are the exception that is not one: they are what the enum or
    interface *is*, so they are exactly as visible as it, and `pub` on one
    is an error. A method that could not be called from
    here does not satisfy an interface here either, and neither does one
    that `print` or an operator would find by name: an interface or an
    operator is another way of calling the method, not a way around `pub`.
  - **A type from another module is `mod.Type`**, anywhere a type may be
    written: a declaration, a parameter, a type argument, a construction
    (`shapes.Point(3, 4)`, `shapes.Wrap<int>(1)`), a static method
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
    statements at the top level of any *imported* file are an error. A
    constant is a declaration, not a statement, so any module may declare
    one — a module may consist of nothing else.

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

### 3.2a `str`

Immutable text. **A `str` is always valid UTF-8**: every way to make one —
a literal, `bytes.utf8()`, concatenation, `split`, `substr`,
`str.from_chars` and the rest of §6.5, and every standard library function
that returns one — yields valid UTF-8 or does not return a `str`. Arbitrary
octets are a `bytes` (§3.10), and `b.utf8()` is the checked way from one to
the other. Rust makes the same promise; `docs/text-decision.md` has the
comparison with Go, Swift, Python and UTF-16, and why.

Two units, each where it is cheap:

  - **Sizes and offsets are in bytes.** `size()`, `substr`, `index_of` and
    `byte_at` count bytes: O(1), and what files and sockets speak.
    `"é".size()` is 2. An offset **inside** a character is refused —
    `substr` traps on one — so a byte offset cannot produce broken text.
    Every offset the language hands out (`index_of`, `size()`, the position
    of an ASCII byte) is already on a boundary.
  - **Text-level work is in code points**, and a code point is an `int`.
    `s.chars()` is the list of a string's Unicode scalar values;
    `str.from_chars(xs)` builds text from them and traps on a value that is
    not a scalar value — a surrogate, a negative number, anything past
    U+10FFFF. There is no `char` type, for the reason there is no byte type:
    the language has one integer type.

A code point is not always what a reader calls a character: `👍🏽` is two
code points, a flag is two, `👨‍👩‍👧` is five, and `é` may be one (U+00E9)
or two (`e` and U+0301). Grouping those — grapheme clusters — and
comparing the two `é`s equal — normalisation — need Unicode's tables, and
are for a `unicode` library module, not the language (`docs/text-decision.md`
§8). `==` compares bytes, and `sort` orders strings by their bytes (§3.9),
which for UTF-8 is code point order — deterministic, and not a
dictionary's.

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

**A field is private to its module unless marked `pub`** — the rule every
other declaration follows (§2.1), for the same reason: a field left public
by mistake is exported for good, while a missing `pub` is a one-word fix
(docs/modules-decision.md §2).

```c
pub type Account {
    pub str owner;
    int balance;        // only this module reads or writes it
    int moves = 0;
}

pub Account open(str owner) { return Account(owner, 0); }
```

Inside the declaring module every field is reachable, as before. From
another module a private field cannot be read, written, or named in a
construction, and the diagnostic says whose it is:

  - **Construction** from outside writes every field, so it is allowed only
    when every field it would write is one the caller could write anyway. A
    private field **without** a default refuses construction outright —
    `acct.Account("ann", 5)` — and the module must provide a function (or a
    static method) that builds one. That is the purpose: a type with an
    invariant hides the field the invariant is about, and its module's own
    functions become the only way in. A private field **with** a default
    blocks nothing while it is left out; naming it is refused.
  - **Operations the compiler provides are the type's own**, not a reach
    into it: `clone(x)` copies every field, private ones included. `print`,
    `==` and friends call the type's methods (§6.2), whose `pub` decides.
  - A `pub` field on a private type is allowed and means nothing until the
    type is exported; a value of a private type that escapes through a
    `pub` function can be passed along, but its fields can be neither read
    nor written outside.

`pub` may be written on a field, never on a parameter, a variant or an
interface method.

### 3.4 Interfaces

An interface is a set of method signatures:

```c
interface HasArea {
    int area();
}
```

A listed parameter may not have a **default**. A default is a name the
caller writes (§4.2), and a call through an interface passes positions to a
vtable slot, so the name never arrives; declaring one is an error where it is
written. The type's own method may still have one, and a direct call on that
type honours it.

Satisfaction is **structural**: a type that has every listed method with a
matching signature satisfies the interface, with no declaration saying so.
Assigning such a value to an interface-typed slot is allowed; assigning one
that does not is a compile error naming the missing method.

The standard library uses this rather than declaring the relationship:
`io.Stream` is the five-method read/write protocol, and `io.File` satisfied
it before it was written — the interface was added to `io` without touching
`File` at all. `io.Buffer` satisfies it too, which is why `io.copy` and
`io.read_line_of` work the same on a file, on standard input and on bytes in
memory, and why a type declared in another program is a stream as soon as it
has the methods.

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

**A promoted field keeps its own visibility** (§3.3), judged on the type
that declares it: embedding another module's type promotes its `pub` fields
and none of its private ones, and a `pub` field stays public when promoted
through an embedded field that is itself private. A private field this
module cannot see does not claim its name here either, so a method of the
embedding type may give a local that name — otherwise adding a private
field to a library type would break the modules that embed it.

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

A distinct type may declare methods of its own, and one it declares comes
before any built-in method of its base: `str Price.show()` is called as
`p.show()` although `int` has built-in methods too. Inside one, `this` is the
distinct value (§4.3).

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

`Result` has two, and no more:

| | |
|---|---|
| `r.is_ok()`, `r.is_err()` | whether it failed, without a `match` |

They are there for symmetry: `Option` can be asked its question, so
`Result` can be asked its own, and counting failures or choosing what to try
next should not take a four-line `match` that binds a payload only to
ignore it. **`?` propagates the failure; `is_ok` asks without taking
anything apart** — the Result is unchanged and can still be matched.
`is_err` is generated from `is_ok`, as `is_none` is from `is_some`.

There is no `or` on a `Result`. `None` carries nothing, so falling back
from it loses nothing; an `Err` carries the reason, and a one-call way to
throw it away is what the discarded-Result rule exists to prevent.

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
type Wrap<T> { T item; }
T unwrap<T>(Wrap<T> b) { return b.item; }
```

Generics are **monomorphised**: each instantiation becomes a separate
concrete type or function before the IR, so there is no boxing and no runtime
type argument. Unused instantiations are not emitted.

A **method on a generic type names the type's parameters on its receiver**,
and a method may have type parameters of its own, on any type:

```c
T Wrap<T>.get() { return item; }
Wrap<U> Wrap<T>.swap<U>(U v) { return Wrap<U>(v); }
T Picker.pick<T>(List<T> xs) { return xs[at]; }
```

The names on the receiver are the method's; they need not match the type
declaration's, and there must be as many. `T Wrap.get()` for a generic `Wrap`
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
binds `T` to `int` — and an empty one contributes nothing.

What the arguments leave open is inferred from **where the value goes**, if
that has a written type: a declaration, an assignment, a `return`, or a
parameter of a non-generic function it is passed to — the same places a
collection literal takes its type from. So a helper whose type parameter is
only in its return type can be called:

```c
Result<T, str> fail<T>(str why) { return Result<T, str>.Err(why); }

Result<int, str> half(int n) {
    if (n % 2 != 0) { return fail("odd"); }     // T is int, from the return type
    ...
}
```

The arguments speak first; the destination only fills in what they left.
Anywhere else — `print(nothing().is_some())`, a receiver, an operand — there
is nothing to infer from, and the compiler says so and asks for a local with
a written type. A `pub` generic function is called from another module like any other,
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

A **`Map` key** is an `int`, a `str`, or a user type that declares both
`int T.hash()` and `bool T.eq(T other)` (§4.4a) — and that **hashes equal
keys equally**: if `a.eq(b)` then `a.hash() == b.hash()`. There is no
`Hashable` interface to implement; declare the two methods and the type is a
key. A key type missing either is refused at the line that builds the map,
naming both if both are missing. A **module constant** map (§4.5) keyed on a
user type is refused: its table is static data, so the compiler has to hash
every key itself, and it cannot run the program's `hash`.

The contract is the program's to keep — the compiler checks that both
methods are there with the right signatures, not what they compute. A `hash`
that contradicts `eq` loses entries; a `hash` that answers the same for
every key is merely slow, and correct. The runtime mixes whatever `hash`
returns before probing, so a plain field or a small sum is a fine `hash`.

Indexing with `[]` works on `Array` and `List`, reads and writes both. An
index outside `0 .. len-1` **traps**.

Methods:

| Receiver | Method | Meaning |
|---|---|---|
| `Array`, `List`, `Map`, `str` | `size()` | element count |
| `Array`, `List` | `contains(v)` | `int`, `float`, `bool` and `str` elements only |
| `Array`, `List` | `index_of(v)` | `Option<int>` — `None` if it is not there |
| `Array`, `List` | `reverse()` | in place |
| `Array`, `List` | `sort()` | in place, ascending; `int`, `float`, `str`, or a type with `cmp` |
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
order lexicographically, and a prefix sorts before what extends it. Floats
get a total order: `-inf < … < ±0.0 < … < +inf < NaN`.

A **user type sorts by its own `cmp`** (§4.4a, §6.2) — the same method `<`
uses, so a sorted list and a comparison can never disagree:

```c
type Item { int rank; str tag; }
int Item.cmp(Item o) { return rank - o.rank; }

List<Item> xs = [Item(3, "a"), Item(1, "b"), Item(3, "c")];
xs.sort();                      // 1-b, 3-a, 3-c
```

Nothing is passed to `sort`: the runtime is holding the element, and the
element carries its type. A type with no `cmp` is refused where the `sort`
is written, naming the method to declare. Two cases are refused for the same
reason: a `cmp` **inherited by embedding** (§3.5) takes the *embedded*
type — the promoted method keeps its original parameter — so the embedding
type must declare its own; and a list of an **interface** cannot be sorted,
because two elements can be different types and one's `cmp` would be handed
the other. Sorting a `List<T>` of a type whose `cmp` is private to another
module is refused too (§2.1), like every other by-name lookup.

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

**`str` is text and `bytes` is octets** — decided, and the question this
section used to leave open (docs/text-decision.md). A `str` is always valid
UTF-8 (§3.2a); `bytes` is the only home for anything else, and `utf8()` is
the one door between them. Every source of text from outside goes through
it and answers with an error value when it says no:

| | |
|---|---|
| `io.read` | `Err(io.Error.InvalidUtf8)`; `io.read_bytes` is the file exactly |
| `io.read_line_of` | `Err(io.Error.InvalidUtf8)` — a `Result` has room to say why, where an `Option` would have to call it the end of the input |
| `fs.listdir` | an error for a name that is not UTF-8 |
| `os.args()` | **traps**, naming the argument and `os.args_bytes()`, which is every argument exactly |
| `os.env(name)` | `None`, as for an unset variable; `os.env_bytes(name)` tells the two apart |
| `os.env_map()` | leaves the variable out; `os.env_bytes(name)` still finds it |

Only `os.args()` traps, and it is the exception on purpose: every other
answer would put a `match` in every program that reads its command line, for
input almost no program ever meets. Rust makes the same trade -- its
`env::args()` panics and its `args_os()` is `args_bytes`. Everything else
here reports a value, because a file or a variable that is not UTF-8 is the
world, not a bug in the program (§6.6).

Text built from values at run time is built from code points with
`str.from_chars(xs)` (§6.5), or from octets by pushing them into a `bytes`
and decoding once with `utf8()`. A `\uXXXX` from a wire format is the first:
one code point, `str.from_chars([cp])`.

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

**`const` means the value never changes, deeply** — on a local exactly as on
a module constant (§4.5). The name may not be reassigned, and the object it
holds, and everything reachable from it, may not be changed by anyone:

```c
const List<Point> ps = [Point(1, 2), Point(3, 4)];
ps.push(Point(5, 6));   // error: `ps` is const and cannot be changed
ps[0].x = 9;            // error: the same, through an index and a field
Point p = ps[0];
p.x = 9;                // traps: the Point was frozen with the list
```

Constness belongs to the **object**, not the name, because `=` aliases:
`List<int> b = a;` is a second name for the same list, and a rule about the
name `a` would say nothing about `b`. So a frozen object is marked, and every
way of changing an object checks the mark (§7.3).

**A `const` binds a constant snapshot of its value.** Which of two things
happens is decided at the binding, when it runs:

  - **Nothing else holds the value** — a literal, a fresh construction, a
    call's result nobody kept: it is **frozen in place**, with no copy.
  - **Something else holds any part of it** — `const List<int> a = b;`, or
    `[p]` while `p` still names the Point: it is **deep-copied and the copy
    frozen**. `b` stays as mutable as it was, and later changes to `b` do
    not reach `a`.

No `clone` is needed, and only one thing is refused (below): a value that
owns a resource. **The cost is stated at the
line: binding a `const` from a shared value costs a deep copy of it** —
time and memory proportional to the part of the graph that is not already
frozen. The copy keeps the shape: two references to one object stay two
references to one copy, and a cycle is copied as a cycle. Frozen and
immortal parts (another `const`, a module constant) and every `str` are
shared, not copied — so binding a `const` from another `const` costs
nothing. A copied cycle can never be broken — breaking it is a change — so,
as cycles are not collected (§7.1), it lives until the program ends.
  - **A change the compiler can see is an error**: assigning to the name,
    `a[i] = ..`, `a.f = ..`, or a method that changes a built-in collection
    (`push`, `pop`, `insert`, `remove_at`, `clear`, `set`, `remove`,
    `reverse`, `sort`, `extend`), through any chain of fields and indexes
    starting at the name.
  - **A change it cannot see traps**: the value passed to a parameter — there
    is no read-only parameter type — or held by another name, or a method
    that assigns a field of its frozen receiver. The trap is `cannot modify
    a constant`.
  - **`clone(x)` is the copy that can be changed.** It is shallow, like
    every clone: a clone of a frozen `List<Point>` is a new list holding the
    same, still frozen, Points.
  - A frozen value is otherwise ordinary: it is read, passed, returned and
    stored like any other, is freed when its last reference goes, and moves
    across a thread boundary under the same rule as anything else (§8.3).
  - `int`, `float`, `bool` and `str` are immutable already, so `const` on
    them only forbids reassignment.
  - **A `const` cannot hold a value that owns a resource** (§4.4): an
    object whose type has a destructor, or anything that can hold one — an
    `io.File`, a struct with a `File` field, a `List<File>`. Copying one
    would release its resource twice; freezing one would leave it half
    usable, and its destructor would still change it. It is refused whether
    the value is shared or fresh:

    ```c
    const io.File g = f;                        // error: `io.File` owns a resource
    const Log log = Log(tags, io.open(p)?);     // error: `Log` can hold `io.File`
    ```

    The compiler refuses every binding whose type (after generics are
    instantiated) can hold one. Through an interface it cannot see the
    value, so the binding **traps** instead, before anything is frozen or
    copied: `a const cannot hold a value of type `T``. Bind such a value
    without `const`.

**Shadowing is not allowed.** A declaration whose name is already in scope is
an error telling you to rename one. A name means one thing for the whole
region a reader can see it in. Nothing shadows a type name either, nor a
builtin function (§6.6) in any module, nor a module the file imports, nor a
constant of the module (§4.5) — a parameter included.

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

Inside an instance method there is **one way to reach each part of the
receiver**:

  - a **field** by its bare name — `w`, never `this.w`;
  - a **sibling method** — an instance method of the receiver's type,
    declared or promoted by embedding (§3.5) — by its bare name, `area()`,
    never `this.area()`;
  - the **whole receiver** as `this`.

```c
enum Shape { Circle(int); Square(int); }

int Shape.area() {
    match (this) {
        case Circle(int r): { return 3 * r * r; }
        case Square(int s): { return s * s; }
    }
}

str Shape.to_str() { return "area " + str(area()); }

Counter Counter.bump() { n = n + 1; return this; }
```

A bare name or a bare call is unambiguous because nothing shadows anything
(§4.1). `this.w` is refused with a diagnostic naming `w`, and `this.area()`
likewise, so the same read is never written two ways. A method the type does
not declare — a built-in one of a distinct collection's base, such as
`this.push(x)` on a `distinct List<int>` — is reached through `this`, because
it has no bare form.

A bare call inside a method that names both a sibling method and a function
in scope — this module's, the entry file's, or a builtin — is an error asking
for one to be renamed. Ranking one above the other would let a declaration
elsewhere in the module silently change what an existing call means. A
**static** method is not a sibling: it has no receiver, so it is always
called on its type, `Point.origin()`, inside a method as anywhere else.

`this` is an expression denoting the receiver. It is **borrowed**, exactly
like a parameter (§7.2): it can be matched on, passed to a function or a
method, given to a generic function (which infers its type argument from it),
compared with `==` where the type defines `eq` (§6.2), and assigned to an
interface-typed slot. Returning it is an ordinary owned return (+1). It is
not a variable: `this = x` is an error, and like any borrowed value it
cannot be moved across a thread boundary — send `clone(this)`.

Its type is the receiver's type. In a method on a distinct type it is the
distinct value (`int(this)` converts it to its base); in a method reached
through embedding it is the **embedded** value the forwarder called, not the
outer one.

`this` exists only inside an instance method. In a static method, a free
function or top-level code it is an error saying there is no receiver there.
It may not appear in a parameter's or a field's default either: a default is
evaluated where the call or construction is written, where `this` would be
some other method's receiver or none at all. As a keyword it can never be
declared as a name.

A method may be declared anywhere in the file, including before its type.

### 4.4 Destructors

A method named **`drop`** is the type's destructor. It runs when the
object's count reaches zero (§7.1) — at the end of the scope that held the
last reference, on a `return`, `?` or `break` that leaves it, when a variable
or element holding it is overwritten or removed — and never at any other
time:

```c
type Conn { int fd; bool open = true; }

Result<bool, Error> Conn.close() { ... }   // the early, checked way

void Conn.drop() {
    if (open) {
        ... release fd, ignoring failure ...
    }
}
```

  - It is declared `void T.drop()`: **no parameters, `void`**, not `static`,
    not `pub`, and with no type parameters of its own. Anything else named
    `drop` on a type is an error. A generic type may have one; it is
    instantiated with the type.
  - Only a **struct** may have one — not an enum, a distinct type or an
    interface — and an interface may not declare a method named `drop`.
  - **It cannot be called.** `x.drop()`, `this.drop()`, a bare `drop()`
    inside a method and `T.drop()` are all errors. Work a program may want
    to do early goes in an ordinary method, which the destructor calls too.
  - **It cannot fail.** It returns nothing, so it has no error to report; a
    failure the program must see is a `Result` from that ordinary method.
  - **It runs first, on a whole object**: every field is still alive. Then
    the fields are released, so a field's destructor runs after its
    owner's. Locals are released in reverse order of declaration.
  - `this` is borrowed, as in any method (§4.3). Storing it anywhere that
    outlives the call **resurrects** a dead object, and traps (§7.4).
  - It is **not promoted** by embedding (§3.5): the embedded value is a
    field, and its own destructor runs when it is released.
  - It runs on the thread that releases the last reference — for a value
    moved to another thread (§8.3), that thread.
  - **A value that owns a resource is never copied.** A type *owns a
    resource* if it has a destructor or can hold a value that does, through
    a field, an element or a variant's payload. `clone(x)` of a type with a
    destructor is an error — the copy would release the same resource
    again; `=` shares it instead. (A type that merely holds one, like a
    struct with a `File` field, may be cloned: the clone is shallow, so the
    File is shared.) And a `const` cannot hold one at all (§4.1).
  - Consequently **a destructor never runs on a frozen object**: nothing it
    could be called on is frozen. It may change its own fields, and any
    object it reaches that is not itself a constant — a `Lease` giving its
    slot back to its pool.

Not guaranteed: members of a cycle are never destroyed, so their destructors
never run (§7.1); nor do those of objects still alive when the program ends
— held by a running thread, or abandoned by `os.exit` or a trap. The top
level's own locals are released when it ends, so theirs do run.

`io.File` has one: a `File` let go without `close()` is closed then, silently
(docs/destructors-decision.md).

### 4.4a Reserved method names

Five method names have a **meaning the language gives them**. A type may
declare any of them, and gets the behaviour in the right-hand column; it may
not declare one with a different signature and mean something else by it.

| Name | Signature | What it is for |
|---|---|---|
| `drop` | `void T.drop()` | the destructor (§4.4) |
| `to_str` | `str T.to_str()` | `print(v)` and `str(v)` (§6.6) |
| `cmp` | `int T.cmp(T other)` | `<`, `<=`, `>`, `>=` (§6.2) and `sort()` (§3.9) |
| `eq` | `bool T.eq(T other)` | `==`, `!=` (§6.2) and a `Map` key (§3.9) |
| `hash` | `int T.hash()` | a `Map` key (§3.9) |

**A `cmp` or an `eq` that takes TWO parameters is a different method**, and
is allowed: `interface Less { int cmp(Point a, Point b); }` compares two
values handed to it, where the reserved one compares the receiver with one
other. Only the receiver-plus-one shape is installed in a type's method
table, so `sort()` and a `Map` key never reach the two-parameter one, and
which is which is visible in the declaration. This is the shape a comparison
callback takes (docs/closures-decision.md).

`cmp`, `eq` and `hash` are **checked where they are declared**, and a wrong
one is refused there rather than at a use:

```
`Key.hash` is a reserved method and must be declared `int Key.hash()`: this
one returns `str`. The language calls it itself -- a `Map` keyed on this
type -- so its signature is not the type's to choose; if this method means
something else, give it another name.
```

The reason is that **nothing in the program writes those calls**. `print(v)`
is visibly `v.to_str()`, so a wrong `to_str` can be refused at the `print`
that wanted it. But `sort()` and a `Map`'s probe call `cmp`, `eq` and `hash`
from the *runtime*, through pointers the compiler puts in the type's
metadata, and there is no line to hang the message on. Checking the
declaration means a type with a wrong `cmp` cannot compile at all, instead
of compiling until the first module that sorts it.

The shape rules, and what each refuses:

  - **not `static`**: all three act on a receiver.
  - **no type parameters of their own**: the call is made through one
    pointer, so nothing would infer them. (A generic *type* may declare
    them; they are instantiated with it.)
  - **`cmp` and `eq` take exactly one parameter**, not optional, of the
    receiver's own type — or of an **interface**, which is the other honest
    reading: `interface Ord { int cmp(Ord other); }` is an ordinary
    one-method interface (§3.4), and `int C.cmp(Ord)` satisfies it. That
    method means "compare me with any `Ord`", which is a different promise
    from "compare me with another `C`", so it is *not* what `sort` or a map
    key accepts. One name; the signature says which of the two it is.
  - **`hash` takes nothing**, and `cmp` and `hash` return `int`, `eq`
    returns `bool`.

They are **found by name, so they obey privacy** (§2.1), exactly as `to_str`
does: sorting another module's type needs its `cmp` to be `pub`, and using
another module's type as a map key needs both `hash` and `eq` to be `pub`.
The refusal names the method and the module it belongs to. (`drop` is the
exception, and the opposite one: it may not be `pub`, because nothing may
call it by name from anywhere.)

A method **promoted by embedding** (§3.5) keeps the embedded type's
parameter, so `Outer` embedding an `Inner` that has `cmp` does **not** get a
`cmp` of its own: the forwarder is `int Outer.cmp(Inner)`. That is checked on
the written signature and not on the machine-level one, which is identical
either way — calling it with two `Outer`s would read an `Inner`'s fields out
of an `Outer`. Declare `int Outer.cmp(Outer other)`, which wins over the
promoted one, and order by the promoted *field*.

### 4.5 Module constants

```c
pub const int MINUTE = 60 * SECOND;
const int SECOND = 1000;
const Array<str> NAMES = ["ms", "s", "min"];
const Map<str, int> KEYWORDS = {"if": 1, "else": 2};
```

`const` at the top level of a file declares a **module constant**, in any
module, the entry file included. At the top level `const` always means this;
a `const` local is written inside a block. The type is mandatory, as it is
on every other name.

**The value is computed by the compiler.** The initialiser is a *constant
expression*:

  - a literal — `int`, `float`, `bool`, `str`;
  - another module constant, of this module (`SECOND`) or another
    (`units.SECOND`, which must be `pub`), declared anywhere;
  - an operator applied to those: the arithmetic, bit, comparison and logical
    operators of §6.1 and `str` `+`, with the same types and the same rules
    as at run time — an overflow, a division by zero or a shift out of range,
    which would trap, is an error, and a float result must be finite (the
    rule a float literal follows, §1.5). `&&` and `||` short-circuit as they
    do at run time: `false && 1 / 0 == 1` is `false`, because the operand
    that would trap never runs — it must still be well typed. A string
    computed with `+` may be at most 2^20 bytes;
  - a collection literal of those, written into an `Array`, a `List`, a
    `bytes` or a `Map` (§3.9): `[a, b]`, `[v; n]`, `{k: v}`.

No call, field, index, method or construction: a constant is never computed
by running the program. Constants may refer to each other in any order but
not in a circle; a cycle is an error that prints the whole chain.

**A constant's type** is `int`, `float`, `bool`, `str` or `bytes`, or an
`Array`, `List` or `Map` of those, nested to any depth. A map key is `int` or
`str`, and a constant map may not repeat a key. A collection holds at most
2^20 elements. A user type is not allowed yet.

**The value is static data.** A collection constant is laid out by the
compiler in the emitted program as an immortal object (§7.3), so there is no
initialisation order and no cost at run time — reading `TABLE[i]` is an
ordinary bounds-checked load. A scalar is folded into every use.

**It never changes**, by the rule every `const` follows (§4.1): assigning to
it, `TABLE[0] = 1` and `TABLE.sort()` are errors, and a change the compiler
cannot see — the table passed to a parameter — traps. `clone(TABLE)` is a
copy that can be changed (shallow: a cloned `Array<Array<int>>` holds the
same constant rows).

**Names.** A constant is private to its module unless `pub`, is reached from
another module as `mod.NAME`, and takes a name in the module's one namespace:
no function, type or other constant of the module may share it, and no local
or parameter anywhere in the module may take it (§4.1). There is no rule
about case; the standard library spells constants in `UPPER_SNAKE_CASE`,
which keeps them out of the way of locals.

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

`cmp` and `eq` are **reserved method names** (§4.4a): the language uses them
elsewhere too — `sort()` calls `cmp`, and a `Map` keyed on the type calls
`eq` — so neither may be declared with another signature, and both are
checked where they are written. `add`, `sub`, `mul`, `div` and `rem` are
not reserved: an operator call is written in the source, so a wrong one is
refused at the operator that wanted it.

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

`?` propagates; `r.is_ok()` and `r.is_err()` (like `o.is_some()`) only ask,
and take nothing apart (§3.7a). A caller that needs the value or the reason
uses `?` or `match`; one that needs only to know whether it failed asks.

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

A `str` is valid UTF-8 text (§3.2a). **Sizes and offsets are in bytes**;
**code points are `int`s**, reached through `chars()` and `from_chars`.
`"é".size()` is 2 and `"é".chars()` is `[233]`.

| | |
|---|---|
| `s.size()` | length in **bytes** |
| `s.chars()` | `List<int>`, the Unicode scalar values in order; `s.chars().size()` is the code point count |
| `s.substr(from, to)` | half-open byte range; **traps** if out of bounds, or if either offset is inside a character |
| `s.contains(sub)` | substring search; an empty needle is found |
| `s.index_of(sub)` | `Option<int>`, a byte offset — always on a character boundary |
| `s.starts_with(p)`, `s.ends_with(p)` | |
| `s.split(sep)` | `List<str>`; keeps empty fields, **traps** on an empty separator |
| `s.trim()` | ASCII whitespace from both ends |
| `s.to_upper()`, `s.to_lower()` | ASCII only; any other character is left alone |
| `s.repeat(n)` | |
| `s.byte_at(i)` | one byte as an `int`, anywhere, including inside a character; **traps** out of range |
| `s.parse_int()` | `Option<int>` — the whole string, decimal, no surrounding space |
| `s.parse_float()` | `Option<float>` |
| `s.to_str()` | itself |
| `s.to_bytes()` | a `bytes` copy of the same octets; cannot fail (§3.10) |

And static, on the type:

| | |
|---|---|
| `str.from_chars(xs)` | the text whose scalar values are the `List<int>` `xs`, as UTF-8; **traps** on a surrogate, a negative value or one past U+10FFFF |

And on a collection of `str`:

| | |
|---|---|
| `xs.join(sep)` | the inverse of `split` — the parts always rejoin |

A `str` is immutable, so every one of these returns a new string.

**Offsets are bytes because that is what a UTF-8 string is**: O(1), and the
unit of every file and socket. Code point offsets would make `substr` O(n)
and a loop over `index_of` quadratic. An offset inside a character traps,
as in Rust, rather than rounding or returning an `Option`: every offset the
language gives out is on a boundary, so one that is not came from
arithmetic on a guess — a bug, like an index out of range. The one honest
computed offset, a byte budget, backs up over continuation bytes first
(`b & 192 == 128`; `corpus/core/793-truncate-on-a-boundary.src`).

**A code point is an `int`**, as a byte is (§3.10): one integer type. `chars`
is a list rather than a loop form, so `for (int c in s.chars())` needs
nothing new. There is no `char_count()` — the count is `chars().size()`,
one spelling that shows it walks the string — and no `from_char(c)`, which
is `from_chars([c])`. An invalid scalar value traps for the reason storing
256 in a `bytes` does: it has no encoding, and it is the program's own
value. Data from outside is checked before it is built into text, as `json`
refuses an unpaired `\u` surrogate.

`to_upper`, `to_lower` and `trim` stay ASCII until the Unicode tables exist:
full case mapping, grapheme clusters and normalisation are a `unicode`
module's (docs/text-decision.md §8). `chars` and `from_chars` are written in
the language (`lib/__text.src`); the rest of this table is the runtime.

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
int h = 0xcbf29ce484222325;                 // FNV-1a's offset basis (§1.5)
h = (h ^ s.byte_at(i)).wrapping_mul(0x100000001b3);
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
| `clone(x)` | a **shallow** copy; not of a type with a destructor (§4.4) |
| `int(x)`, `bool(x)`, `str(x)`, `bytes(x)` | convert a distinct value to its base |
| `send(ch, v)`, `recv(ch)`, `close(ch)` | channels (§8) |
| `trap(msg)` | stop the program with a `str` message (§7.4); a statement, never a value |

**`trap(msg)` is for a bug, never for the world.** The language traps on
the mistakes it can see — an index out of range, an overflow — and `trap`
is the same thing for the ones only the program can see: an argument
outside what a function accepts, an invariant that does not hold. A failure
a caller should handle is a `Result` (docs/errors-decision.md); nothing
catches a trap. It never returns, so it ends its block like `return` does:
a function may end in one with no return after it, and a statement after one
is unreachable. For the same reason it has no value and may only be written
as a statement.

There is no `assert(cond, msg)`: it would be a second spelling of
`if (!cond) { trap(msg); }`, and one way to write a thing beats two.

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

When a count reaches zero the type's destructor runs, if it declares one
(§4.4), and then the object's fields are released, each of which may reach
zero in turn. A cycle never reaches zero, so its destructors never run.

Refcount operations are non-atomic (§8.3).

### 7.2 Ownership protocol

  - **Arguments are borrowed.** Passing a value you already hold to a
    function costs no refcount traffic. The caller keeps every argument —
    the receiver of a method, and the operands of an operator, included —
    alive until the call returns, even when the callee overwrites the field
    or element it was read from: `f(h.p, h)` is safe when `f` assigns
    `h.p`. A local, a parameter or `this` costs nothing to pass; a value
    read out of a field or an element costs one retain and one release,
    and only when something evaluated after it could run the program's
    own code (docs/ir-v0.md §5.1).
  - **Returns are owned (+1).** The caller receives a reference and is
    responsible for it.
  - A value stored into a field, an element or a map entry is **retained**
    by the container.

The compiler inserts every retain and release. There is no manual
`retain`/`release`, and no way to write one.

### 7.3 Immortal and frozen values

A string literal is allocated once, statically, with a count that never
reaches zero. Retaining and releasing one is a no-op, so a literal in a hot
loop costs nothing. A module constant's collection is the same (§4.5):
static, immortal, never freed, not counted as a leak, and in read-only
memory.

A **frozen** value is one bound to a `const` (§4.1). It is counted and freed
like any other object — freezing marks it, it does not pin it — but every
operation that would change it traps instead: a field store, an index store,
and every changing method of a collection or a `bytes`, whichever name or
parameter the change arrives through. An immortal value is frozen too.

Freezing is transitive, over everything the value reaches, except what is
immutable anyway (`str`) and what is already frozen. It happens in place
only when that whole graph is reachable from the value alone; otherwise the
binding freezes a deep copy (§4.1). Either way it costs time proportional
to the graph, once, at the binding. A graph containing an object that owns
a resource is neither frozen nor copied: the binding is refused, or traps
(§4.1), so no frozen object ever has a destructor.

### 7.4 Traps

A trap prints a message to stderr and aborts. It is not catchable: there are
no exceptions. A trap is for a bug; a failure the program should handle is an
error value, `Result` or `Option` (docs/errors-decision.md).

Trapping conditions:

  - integer overflow, on any arithmetic operator including unary minus —
    not on a bit operator, and not in a `wrapping_` method
  - a shift count outside `0 .. 63`
  - division or remainder by zero, and `INT_MIN / -1`
  - an index outside `0 .. len-1`
  - a `substr` offset inside a character (§6.5)
  - `str.from_chars` given a value that is not a Unicode scalar value (§6.5)
  - `pop` on an empty list or an empty `bytes`
  - storing a value outside 0..255 into a `bytes` (§3.10)
  - `recv` on a channel that is closed and drained
  - a length or capacity too large to allocate
  - a uniqueness violation at a thread boundary (§8.3)
  - a destructor that leaves `this` referenced from anywhere when it
    returns — a resurrected object (§4.4)
  - changing a frozen or constant value (§7.3)
  - binding a `const`, through an interface, to a value that owns a
    resource (§4.1)
  - `trap(msg)`, with the program's own message (§6.6)

A trap inside a destructor is a trap like any other.

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
type Wrap { str s; }
str shared = concat("ab", "cd");
spawn eat(Wrap(shared));      // each Wrap is unique -- the str is not
```

So the whole graph reachable from the moved value must be unreachable from
anywhere else. The check counts the references into each reachable object
from within the graph and requires that to equal its refcount. An object
reached twice *inside* the graph is fine — one thread still owns all of it.
Immortal objects are skipped, which is why a literal and a channel cost
nothing here.

It traps rather than corrupting the heap, and costs time proportional to the
graph, paid once per crossing.

**Module constants are shared, not moved.** Every thread may read one — by
name, or passed to `spawn` or `send` — because it is immortal and immutable:
no thread ever writes its count, which a retain or release leaves alone, or
its contents, which nothing may change. There is nothing to race on.

A **frozen** value is not a constant. It is immutable, but its count is an
ordinary non-atomic count, so it crosses a thread boundary under the rule
above — moved, and unique — exactly like any other value.

Mutable state at module level does not exist; see
`docs/module-state-decision.md` for why, and for what might replace it.

---

## 9. Not in the language

Stated so the absence is a decision and not an oversight:

  - exceptions and unwinding; a fault traps, and a recoverable failure is a
    `Result` (§3.7a)
  - `defer`, `finally`, finalizers — a destructor (§4.4) runs at the exact
    moment an object dies, on every path out of a scope
  - multiple returns — a `Result` or an enum carries what a second return
    value would have
  - a function *type* — no `fn` keyword, no `Fn<..>`. A callback's type is an
    ordinary one-method interface, and a function's name is a value exactly
    where such an interface is expected: `interface Less { int cmp(Point a,
    Point b); }` and then `smallest(ps, by_x)`. Calling one is a method call,
    `order.cmp(a, b)`, never `order(a, b)` (docs/closures-decision.md)
  - closures and lambdas
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
item        = [ "pub" ] decl_item | stmt ;            (* stmt: entry file only;
                                                         never a `const` decl, §4.5 *)
decl_item   = typedecl | interface | enumdecl | distinct | func | prim
            | constdecl ;
constdecl   = "const" type IDENT "=" expr ";" ;       (* a constant expression, §4.5 *)

typedecl    = "type" IDENT [ tparams ] "{" { field ";" } "}" ;
interface   = "interface" IDENT [ tparams ] "{" { sig ";" } "}" ;
enumdecl    = "enum" IDENT [ tparams ] "{" { variant ";" } "}" ;
variant     = IDENT [ "(" type { "," type } ")" ] ;
distinct    = "distinct" type IDENT ";" ;
field       = [ "pub" ] ( type IDENT [ "=" expr ] | type ) ;  (* bare type = embedded *)
sig         = type IDENT "(" [ sigparams ] ")" ;    (* no defaults, §3.4 *)
sigparams   = type IDENT { "," type IDENT } ;

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

decl        = [ "const" ] type IDENT "=" expr ";" ;   (* always initialised; const
                                                         binds a snapshot, §4.1 *)
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
                                                         float.from_bits, str.from_chars *)
            | "this"                                  (* instance methods only, §4.3 *)
            | seqlit | maplit
            | "(" expr ")" ;

seqlit      = "[" [ expr { "," expr } ] "]"           (* List, Array or bytes, §3.9 *)
            | "[" expr ";" expr "]" ;                 (* value; count *)
maplit      = "{" [ expr ":" expr { "," expr ":" expr } ] "}" ;

args        = "(" [ arg { "," arg } ] ")" ;
arg         = expr | IDENT ":" expr ;                 (* positional before named *)
```

Lexical: `INT` is decimal digits with no leading zero, or `0x`, `0o` or `0b`
(lowercase) and digits of that base, hex ones in either case; `_` may
follow any digit. A decimal fits in `int`, a prefixed one in 64 bits
(§1.5). `FLOAT` has a dot with digits on **both**
sides — `1.0`, never `1.` or `.5` — and an exponent only after that form
(`1.0e9`), so whether a literal is a float is decided by one character.
`STR` is `"`, then any bytes but `"`, `\` and a newline, or an escape —
`\\` `\"` `\n` `\t` `\r` `\0` `\x` two hex digits up to `7F`, `\u{` one to six hex
digits `}` naming a scalar value — then `"` (§1.5). `IDENT` is a letter or
`_` followed by letters, digits or `_`, and **may not begin with `__`**,
which is reserved (§10.1).

**No trailing commas**, anywhere a list is closed by a bracket. The compiler
currently accepts `[1, 2,]` and `{"a": 1,}`; that is a bug, and it is
refused rather than extended to arguments and parameters because allowing
trailing commas later is additive and forbidding them later is not.

### 10.1 The standard library's seam

`prim` and `__`-prefixed names are available only to modules the compiler
ships in `lib/`. To any other program they do not exist: `prim` is refused
and a `__` name is a reserved-name error. See `docs/stdlib-seam.md`.
