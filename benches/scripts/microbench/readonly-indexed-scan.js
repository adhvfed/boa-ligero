// Scan a contiguous array-like object whose indexed data properties are
// readonly. Web IDL getter-only collections (including NodeList) have this
// shape, so Boa stores their indices as sparse property descriptors rather
// than dense array elements.

const SIZE = 100;
const RUNS = 5_000;
const list = { length: SIZE };
for (let i = 0; i < SIZE; i++) {
  Object.defineProperty(list, i, {
    value: { index: i },
    enumerable: true,
    configurable: true,
  });
}
const needle = list[50];
const { indexOf } = Array.prototype;

function main() {
  let result = -1;
  for (let i = 0; i < RUNS; i++) {
    result = indexOf.call(list, needle);
  }
  return result;
}
