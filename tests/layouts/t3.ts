const outside = { o: 1 };
function withEval() { const local = { ev: 1 }; eval("local.x = 1"); return local; }
withEval();
