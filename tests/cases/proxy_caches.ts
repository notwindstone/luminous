// @native opts rd add has
// Sites that cached what objects of Object.create(null)'s shape have (or
// do not), then given proxies: their handlers answer.
const log = [];
const kEmptyObject = Object.freeze(Object.create(null));
const handler = {
    __proto__: null,
    get(t, k, r) { log.push("get " + String(k)); return Reflect.get(t, k, r); },
    set(t, k, v, r) { log.push("set " + String(k)); return Reflect.set(t, k, v, r); },
    has(t, k) { log.push("has " + String(k)); return Reflect.has(t, k); },
};
function opts(buffer, offset, length, position) {
    if (typeof offset === "object") {
        ({ offset = 0, length = buffer.length - offset, position = null } = offset ?? kEmptyObject);
    }
    return [offset, length, position].join(",");
}
function rd(o) { return o.q; }
function add(o) { o.q = 1; return o; }
function has(o) { return "q" in o; }
const buf = [1, 2, 3];
for (let i = 0; i < 4; i++) {
    log.push(opts(buf, null), String(rd(Object.create(null))), String(has(Object.create(null))));
    add(Object.create(null));
}
log.push(opts(buf, new Proxy({ length: 1 }, handler)));
log.push(opts(buf, new Proxy({ offset: 1, position: 5 }, handler)));
log.push(String(rd(new Proxy({ q: "via handler" }, handler))));
log.push(String(has(new Proxy({ q: 1 }, handler))));
const p = add(new Proxy({}, handler));
log.push(Object.keys(p).join(","));
// A revoked one throws, as before.
const r = Proxy.revocable({}, {});
r.revoke();
try { rd(r.proxy); log.push("no throw"); } catch (e) { log.push(e.constructor.name); }
console.log(log.join("\n"));
