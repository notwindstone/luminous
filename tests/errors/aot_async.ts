// @error `await` in a `for` update is not supported yet
aot async function f(n: number): Promise<number> {
  let s = 0;
  for (let i = 0; i < n; i += (await Promise.resolve(1)) as number) s += i;
  return s;
}
