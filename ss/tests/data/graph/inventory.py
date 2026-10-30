"""Stock bookkeeping for a small warehouse."""

import os.path
from collections import defaultdict
from dataclasses import dataclass

MAX_BIN_ITEMS = 500


class Storage:
    """Anything that can hold stock."""


@dataclass
class Bin(Storage):
    """A numbered shelf bin that holds one sku."""

    label: str
    sku: str
    count: int = 0

    def add(self, amount):
        """Put amount units into the bin, refusing to overfill it."""
        if self.count + amount > MAX_BIN_ITEMS:
            raise ValueError("bin overflow")
        self.count += amount
        return self.count

    def drain(self):
        taken = self.count
        self.count = 0
        return taken


def load_bins(path):
    """Read bins from a tab separated stock file."""
    bins = []
    with open(os.path.join(path, "bins.tsv")) as handle:
        for line in handle:
            label, sku, count = line.rstrip().split("\t")
            bins.append(Bin(label, sku, int(count)))
    return bins


def totals_by_sku(bins):
    totals = defaultdict(int)
    for shelf in bins:
        totals[shelf.sku] += shelf.count
    return dict(totals)


def test_add_refuses_to_overfill():
    shelf = Bin("A1", "bolt-m6")
    shelf.add(400)
    try:
        shelf.add(200)
    except ValueError:
        return
    raise AssertionError("expected overflow")
