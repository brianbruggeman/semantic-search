const fs = require("fs");
const { parseLine } = require("./parse");

const DEFAULT_WIDTH = 80;

/**
 * Counts words in a text file.
 */
function countWords(path) {
  const text = fs.readFileSync(path, "utf8");
  return text.split(/\s+/).filter(Boolean).length;
}

class Counter {
  constructor() {
    this.seen = new Map();
  }

  add(word) {
    this.seen.set(word, (this.seen.get(word) || 0) + 1);
    return this.seen.get(word);
  }

  report(width = DEFAULT_WIDTH) {
    return [...this.seen].map(([word, n]) => parseLine(word, n, width));
  }
}

const topWord = (counter) => counter.report()[0];

module.exports = { countWords, Counter, topWord };
