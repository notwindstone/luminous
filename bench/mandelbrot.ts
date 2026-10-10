function mandel(cx: number, cy: number, maxIter: number): number {
  let x = 0, y = 0, i = 0;
  while (i < maxIter && x * x + y * y <= 4) {
    const t = x * x - y * y + cx;
    y = 2 * x * y + cy;
    x = t;
    i++;
  }
  return i;
}

function render(w: number, h: number, maxIter: number): number {
  let sum = 0;
  for (let py = 0; py < h; py++) {
    for (let px = 0; px < w; px++) {
      sum += mandel((px / w) * 3.5 - 2.5, (py / h) * 2 - 1, maxIter);
    }
  }
  return sum;
}

const start = Date.now();
console.log(`mandelbrot checksum = ${render(400, 300, 200)}`);
console.log(`elapsed: ${Date.now() - start} ms`);
