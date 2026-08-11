// Repeated direct reads from a contiguous array-like object whose indexed
// properties are readonly. This models the hot JavaScript shape used when a
// page manually scans Web IDL getter-only collections such as NodeList.

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

function main() {
  let checksum = 0;
  for (let i = 0; i < RUNS; i++) {
    for (let j = 0; j < list.length; j++) {
      if (list[j] === needle) {
        checksum += j;
        break;
      }
    }
  }
  return checksum;
}
