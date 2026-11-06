# Module `grid`

## constant `MAX_WIDTH`

```m31
pub const int MAX_WIDTH
```

The largest width a row may have.

## type `Point`

```m31
pub type Point
```

A point on the grid.

Only the `pub` fields are part of the API.

- field `int across`: Distance from the left edge.
- field `int down`

## method `Point.distance`

```m31
pub int Point.distance(Point other)
```

Straight-line distance, rounded down.

## method `Point.origin`

```m31
pub static Point Point.origin()
```

## enum `Failure`

```m31
pub enum Failure
```

The ways a move can fail.

- variant `Edge`: The edge of the grid.
- variant `Blocked(str)`: Something stands there:
  what it is.

## interface `Sized`

```m31
pub interface Sized
```

Anything with a size.

- method `int size()`: How big it is.
- method `int weight()`

## distinct `Metres`

```m31
pub distinct int Metres
```

## function `pair`

```m31
pub List<T> pair<T>(T left, T right, int times = ...)
```

Pairs two values.
