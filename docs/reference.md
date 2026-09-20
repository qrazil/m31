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

    bool     break     case      const     continue  distinct  else
    enum     false     for       if        in        int       interface
    match    return    spawn     str       true      type      void
    while

Keywords are reserved: none may be used as an identifier. `Array`, `Chan`,
`List` and `Map` are not keywords — they are predeclared type names, and are
reserved only in the sense that nothing may shadow a type name (§4.1).

### 1.5 Literals

| | |
|---|---|
| Integer | `0`, `42`. Decimal only. No sign — `-1` is unary minus applied to `1`. |
| Boolean | `true`, `false` |
| String | `"..."`, with escapes `\\` `\"` `\n` `\t` `\0` |

A `str` carries its length, so `\0` is an ordinary character:
`"a\0b".size()` is 3. Strings are not NUL-terminated.

String literals are **immortal**: their refcount never reaches zero, so they
are never freed (§7.3).

### 1.6 Operators and punctuation

    +   -   *   /   %
    ==  !=  <   <=  >   >=
    &&  ||  !
    =   .   ,   ;   :
    (   )   {   }   [   ]   <   >

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

There are no modules. One file is the whole program.

---

## 3. Types

### 3.1 Value types

`int` is a 64-bit signed integer. `bool` is `true` or `false`. Neither is a
reference; both are copied by assignment.

`int` is deliberately unqualified. The IR may lower it to i32 or i128 for a
target that wants that; the source spells one integer type.

### 3.2 Reference types

`str`, the built-in collections, and every `type` are **references**.
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

### 3.8 Generics

Type parameters on a type or a function:

```c
type Box<T> { T item; }
T unwrap<T>(Box<T> b) { return b.item; }
```

Generics are **monomorphised**: each instantiation becomes a separate
concrete type or function before the IR, so there is no boxing and no runtime
type argument. Unused instantiations are not emitted.

There are no constraints on type parameters yet. A generic body that does
something a given argument cannot do fails when that instantiation is
checked, naming the instantiation.

### 3.9 Collections

| | |
|---|---|
| `Array<T>(n, v)` | fixed length `n`, every slot initialised to `v` |
| `List<T>()` | growable, starts empty |
| `Map<K, V>()` | `K` is `int` or `str` |
| `Chan<T>(cap)` | bounded channel, capacity `cap` |

`Array` requires a fill value. There is no null, so there is no such thing as
an uninitialised slot to read.

`Map` keys are `int` or `str`. Hashing a user type would need a `Hashable`
interface, which does not exist.

Indexing with `[]` works on `Array` and `List`, reads and writes both. An
index outside `0 .. len-1` **traps**.

Methods:

| Receiver | Method | Meaning |
|---|---|---|
| `Array`, `List`, `Map`, `str` | `size()` | element count |
| `Array`, `List` | `contains(v)` | `int`, `bool` and `str` elements only |
| `Array`, `List` | `reverse()` | in place |
| `Array`, `List` | `sort()` | in place, ascending; `int` and `str` only |
| `List` | `push(v)` | append |
| `List` | `pop()` | remove and return the last element; traps if empty |
| `List` | `insert(i, v)` | at `i`; `i == len` appends, beyond that traps |
| `List` | `remove_at(i)` | remove and return the element at `i` |
| `List` | `clear()` | drop every element |
| `Map` | `set(k, v)` | insert or replace |
| `Map` | `get(k)` | **traps** on a missing key |
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
There is no `index_of` yet: with no null there is nothing honest for it to
return when the element is absent, and a `-1` sentinel is not something to
lock into a language that intends to freeze. It waits for `Option<int>`,
which waits for enums.

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

`get` traps rather than returning a default, because there is no null to
return: the honest choices are to trap or to force every read through a
check, and `contains` is the check.

A map is enumerated through `keys()` and `values()`, which each build a new
`List`. It is not iterable directly: its slots are sparse, so a loop over
them needs a cursor that skips, which is not the shape `for ... in` has.
Building a list makes the allocation visible at the call site instead of
hiding it in the loop. **The order is the table's, not insertion order**, and
it changes when the table rehashes — do not depend on it.

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
region a reader can see it in. Nothing shadows a type name either.

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
end is a compile error. There are no multiple return values yet.

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

Iterates an `Array` or a `List`. The loop variable is a fresh binding each
iteration and is **borrowed** from the collection; it is not a copy.

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
| 7 | `-x` `!x` | prefix |
| 6 | `*` `/` `%` | left |
| 5 | `+` `-` | left |
| 4 | `<` `<=` `>` `>=` | left |
| 3 | `==` `!=` | left |
| 2 | `&&` | left |
| 1 | `\|\|` | left |

C's precedence, for the operators that exist. `&&` and `||` short-circuit.

On the built-in types, `==` works on `int`, `bool` and `str`; `str` compares
**by value**. On a user type an operator is a method call (§6.2).

Arithmetic on a distinct type yields **that same distinct type**, not the
base — `Price + Price` is a `Price`. Mixing two distinct types, or a distinct
type and its base, is an error; convert explicitly (§3.6).

**Integer arithmetic traps on overflow** — `+`, `-`, `*`, `/`, `%` and
unary `-`, on every target. There is no wrapping variant; one way to do each
thing, and a `wrapping_add` can arrive the day something needs it. Division
and remainder by zero trap too, as does `INT_MIN / -1`.

Overflow trapping is why an overflowing program cannot have a Go twin in the
corpus: Go wraps, so the twin would be confidently wrong. Those cases live in
`corpus/traps/` instead.

There is no unsigned type and no bitwise operator yet.

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
change an operator's meaning on a built-in type.

### 6.3 Postfix

`a.field`, `a.method(..)`, `a[i]`, and chains of them: `xs[0].name`,
`grid[i][j]`.

### 6.4 Construction

```c
Point(1, 2)             // mandatory fields, positional
Point(1, 2, label: "a") // a defaulted field, named
Array<int>(4, 0)
List<str>()
Map<str, int>()
Chan<int>(8)
```

### 6.5 Built-in functions

| | |
|---|---|
| `print(x)` | `int`, `bool` or `str`, one argument, newline-terminated |
| `concat(a, b)` | joins two `str` |
| `clone(x)` | a **shallow** copy |
| `int(x)`, `bool(x)`, `str(x)` | convert a distinct value to its base |
| `send(ch, v)`, `recv(ch)`, `close(ch)` | channels (§8) |

`print` selects its runtime helper from the static argument type. That is not
user-visible function overloading, which does not exist. It refuses a user
type: until a type can say how it prints, an address is worse than a refusal.

`clone` is shallow — the copy holds the same references, each retained once
more. Deep copying would have to decide what copying each field means, which
is a question only the program can answer. `clone` works on a `str`, an
`Array`, a `List` and a struct; a channel and an interface value cannot be
cloned.

There is no string library yet: no `substr`, no `split`, no `contains`.

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

  - integer overflow, on any arithmetic operator including unary minus
  - division or remainder by zero, and `INT_MIN / -1`
  - an index outside `0 .. len-1`
  - `Map.get` on a key that is not there
  - `pop` on an empty list
  - `recv` on a channel that is closed and drained
  - a length or capacity too large to allocate
  - a uniqueness violation at a thread boundary (§8.3)

---

## 8. Concurrency

### 8.1 Threads

`spawn f(..)` runs `f` on a new OS thread. The program waits for all of them
before exiting. There is no handle, no join, no cancellation, and no thread
identity.

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

  - modules, imports, visibility — one file is the program
  - exceptions and unwinding; a fault traps, and a recoverable failure is a
    `Result`-shaped enum the program declares itself
  - multiple returns
  - closures, function values, lambdas
  - inheritance, method overriding, abstract types
  - defining an operator outside the fixed set of §6.2, or changing one on a
    built-in type
  - user-defined conversions
  - shadowing — see §4.1
  - null, optionals, zero values — every declaration initialises
  - type aliases — see §3.6
  - unsigned and sized integer types, bitwise operators, floats
  - a string library — see §6.4
  - `switch`, ternary `?:`, three-clause `for`, labelled break
  - variadic functions
  - constraints on type parameters
  - reflection, runtime type queries, downcasting from an interface
  - unsafe, raw pointers, manual allocation
  - a garbage collector, and therefore cycle collection

---

## 10. Grammar

EBNF. `{ x }` is zero or more, `[ x ]` optional.

```ebnf
program     = { item } ;
item        = typedecl | interface | enumdecl | distinct | func | stmt ;

typedecl    = "type" IDENT [ tparams ] "{" { field ";" } "}" ;
interface   = "interface" IDENT [ tparams ] "{" { sig ";" } "}" ;
enumdecl    = "enum" IDENT [ tparams ] "{" { variant ";" } "}" ;
variant     = IDENT [ "(" type { "," type } ")" ] ;
distinct    = "distinct" type IDENT ";" ;
field       = type IDENT [ "=" expr ] | type ;        (* bare type = embedded *)
sig         = type IDENT "(" [ params ] ")" ;

func        = type [ IDENT "." ] IDENT [ tparams ] "(" [ params ] ")" block ;
tparams     = "<" IDENT { "," IDENT } ">" ;
params      = param { "," param } ;
param       = type IDENT [ "=" expr ] ;

type        = "int" | "bool" | "str" | "void"
            | IDENT [ "<" type { "," type } ">" ] ;

block       = "{" { stmt } "}" ;
stmt        = decl | assign | eval | if | while | forin | match
            | "return" [ expr ] ";" | "break" ";" | "continue" ";"
            | "spawn" IDENT args ";" ;
match       = "match" "(" expr ")" "{" { case } "}" ;
case        = "case" IDENT [ "(" bind { "," bind } ")" ] ":" block ;
bind        = type IDENT ;

decl        = [ "const" ] type IDENT "=" expr ";" ;
assign      = lvalue "=" expr ";" ;
lvalue      = IDENT | expr "." IDENT | expr "[" expr "]" ;
eval        = expr ";" ;
if          = "if" "(" expr ")" block [ "else" ( if | block ) ] ;
while       = "while" "(" expr ")" block ;
forin       = "for" "(" type IDENT "in" expr ")" block ;

expr        = unary { binop unary } ;                 (* precedence per 6.1 *)
unary       = [ "-" | "!" ] postfix ;
postfix     = atom { "." IDENT [ args ] | "[" expr "]" } ;
atom        = INT | STR | "true" | "false"
            | IDENT [ args ]
            | type args                               (* construction *)
            | type "." IDENT [ args ]                 (* enum variant *)
            | "(" expr ")" ;

args        = "(" [ arg { "," arg } ] ")" ;
arg         = expr | IDENT ":" expr ;                 (* positional before named *)
```
