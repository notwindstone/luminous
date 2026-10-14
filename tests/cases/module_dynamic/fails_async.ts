console.log("fails_async evaluating");
await 0;
export const never = 1;
throw new Error("async boom");
