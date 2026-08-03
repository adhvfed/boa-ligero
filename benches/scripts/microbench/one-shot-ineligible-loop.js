function main() {
  let total = 0;
  for (let i = 0; i < 2000000; i++) {
    total = (total + i) | 0;
  }
  return total;
}
