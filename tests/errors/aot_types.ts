// @error optional fields are not supported in an `aot type`
// @error unsupported field type in an `aot type`
// @error `B` is not an `aot type`
// @error an `aot type` cannot contain itself
// @error `a` has two different types
// @error an `aot type` can only have fields
// @error an `aot type` must be declared at the top level of the module
type int = number;
type B = { b: number };

aot type Opt = { x?: number };
aot type Arr = { xs: number[] };
aot type UsesPlain = { a: int } | B;
aot type Self = { a: int } & Self;
aot type Clash = { a: int } & { a: string };
aot type Method = { m(): void };

function inner(): void {
  aot type Local = { a: int };
}
