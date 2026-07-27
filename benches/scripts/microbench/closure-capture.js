// Closure captures + invocation. Tests environment chain walking
// for non-trivial closures.

function makeAdder(base) {
  let shift = 0;
  return function (x) {
    shift = shift + 1;
    return x + base + shift;
  };
}

const add = makeAdder(7);
const N = 200_000;

function main() {
  let acc = 0;
  for (let i = 0; i < N; i++) {
    acc = (acc + add(i)) | 0;
  }
  return acc;
}
