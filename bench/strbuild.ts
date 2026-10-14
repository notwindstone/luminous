// String building: template literals, +=, join, replace, case conversion.
function row(i: number): string {
  return `<tr><td>${i}</td><td>${(i * 1.5).toFixed(2)}</td><td>${i % 2 === 0 ? "even" : "odd"}</td></tr>`;
}
const start = Date.now();
let total = 0;
for (let r = 0; r < 10; r++) {
  let html = "<table>";
  for (let i = 0; i < 20000; i++) html += row(i);
  html += "</table>";
  const parts: string[] = [];
  for (let i = 0; i < 20000; i++) parts.push(`k${i}=${i * 3}`);
  const q = parts.join("&");
  const up = q.toUpperCase().replace(/K1/g, "X");
  total += html.length + q.length + up.length + html.split("<td>").length;
}
console.log(`total: ${total}`);
console.log(`elapsed: ${Date.now() - start} ms`);
