// @error unsupported field type in an `aot type`
// @error `B` is not an `aot type`
// @error an `aot type` cannot contain itself
// @error `a` has two different types
// @error an `aot type` can only have fields
// @error an `aot type` must be declared at the top level of the module
// @error unsupported element type of an array in an `aot type`
// @error `D` is an array, not an object type
// @error the elements of an array in an `aot type` cannot be `null` or `undefined`
// @error an `aot type` needs an object type
type int = number;
type B = { b: number };

aot type Nul = { a: int } | null;
aot type NulItems = { xs: Nul[]; ys: ({ b: int } | undefined)[] };
aot type OnlyNull = null | undefined;
aot type Arr = { xs: number[] };
aot type Mixed = { m: Arr | { y: int } };
aot type D = Arr[];
aot type FromArray = D | { a: int };
aot type UsesPlain = { a: int } | B;
aot type Self = { a: int } & Self;
aot type Clash = { a: int } & { a: string };
aot type Method = { m(): void };

function inner(): void {
  aot type Local = { a: int };
}
