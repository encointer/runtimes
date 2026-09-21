#!/usr/bin/env python3
"""Derives the para ids that have a stale downward message queue on Polkadot and Kusama.

Before paritytech/polkadot-sdk#6604 the relay chain accepted downward messages for paras
without a head, and nothing removes what was left behind. This lists the paras that still
have a `Dmp::DownwardMessageQueues` entry or a `Dmp::DownwardMessageQueueHeads` entry but
no `Paras::Heads` entry, which is what `STALE_PARAS` in
`relay/{polkadot,kusama}/src/migrations/stale_dmp_queues.rs` hardcodes.

See https://github.com/polkadot-fellows/runtimes/issues/513.

Reads public RPC endpoints, needs nothing but the standard library:

    python3 stale-dmp-queues.py
"""

import json
import struct
import sys
import urllib.request

CHAINS = [("Polkadot", "https://rpc.polkadot.io"), ("Kusama", "https://kusama-rpc.polkadot.io")]

# --- xxhash64, so that twox128 storage prefixes need no dependency ----------------------

MASK = (1 << 64) - 1
P1, P2, P3, P4, P5 = (
    11400714785074694791,
    14029467366897019727,
    1609587929392839161,
    9650029242287828579,
    2870177450012600261,
)


def _rol(x, r):
    return ((x << r) | (x >> (64 - r))) & MASK


def _round(acc, inp):
    return _rol((acc + inp * P2) & MASK, 31) * P1 & MASK


def xxh64(data, seed):
    length, i = len(data), 0
    if length >= 32:
        v1, v2, v3, v4 = (seed + P1 + P2) & MASK, (seed + P2) & MASK, seed, (seed - P1) & MASK
        while i + 32 <= length:
            v1 = _round(v1, struct.unpack_from("<Q", data, i)[0])
            v2 = _round(v2, struct.unpack_from("<Q", data, i + 8)[0])
            v3 = _round(v3, struct.unpack_from("<Q", data, i + 16)[0])
            v4 = _round(v4, struct.unpack_from("<Q", data, i + 24)[0])
            i += 32
        h = (_rol(v1, 1) + _rol(v2, 7) + _rol(v3, 12) + _rol(v4, 18)) & MASK
        for v in (v1, v2, v3, v4):
            h = (h ^ _round(0, v)) & MASK
            h = (h * P1 + P4) & MASK
    else:
        h = (seed + P5) & MASK
    h = (h + length) & MASK
    while i + 8 <= length:
        h = (h ^ _round(0, struct.unpack_from("<Q", data, i)[0])) & MASK
        h = (_rol(h, 27) * P1 + P4) & MASK
        i += 8
    if i + 4 <= length:
        h = (h ^ struct.unpack_from("<I", data, i)[0] * P1) & MASK
        h = (_rol(h, 23) * P2 + P3) & MASK
        i += 4
    while i < length:
        h = (h ^ data[i] * P5) & MASK
        h = _rol(h, 11) * P1 & MASK
        i += 1
    h ^= h >> 33
    h = h * P2 & MASK
    h ^= h >> 29
    h = h * P3 & MASK
    h ^= h >> 32
    return h


def twox128(name):
    data = name.encode()
    return struct.pack("<Q", xxh64(data, 0)) + struct.pack("<Q", xxh64(data, 1))


# The well-known `System::Account` prefix, so a wrong hash fails loudly rather than silently
# returning an empty key set.
assert (twox128("System") + twox128("Account")).hex() == (
    "26aa394eea5630e07c48ae0c9558cef7b99d880ec681799c0cf30e8886371da9"
)

# --- RPC -------------------------------------------------------------------------------


def rpc(url, method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    request = urllib.request.Request(
        url, data=body, headers={"Content-Type": "application/json", "User-Agent": "curl/8"}
    )
    with urllib.request.urlopen(request, timeout=60) as response:
        result = json.load(response)
    if "error" in result:
        raise RuntimeError(f"{method}: {result['error']}")
    return result["result"]


def para_ids(url, pallet, item):
    """The para ids keying `pallet::item`, a `Twox64Concat` map of `ParaId` to something."""
    prefix = "0x" + (twox128(pallet) + twox128(item)).hex()
    keys, start = [], None
    while True:
        page = rpc(url, "state_getKeysPaged", [prefix, 1000, start])
        keys += page
        if len(page) < 1000:
            break
        start = page[-1]
    # Twox64Concat appends the 8 byte hash and then the key itself, a little endian `u32`.
    return {struct.unpack("<I", bytes.fromhex(key[len(prefix) + 16 :]))[0] for key in keys}


# --- Report ----------------------------------------------------------------------------


def main():
    stale = {}
    for name, url in CHAINS:
        number = int(rpc(url, "chain_getHeader", [])["number"], 16)
        queues = para_ids(url, "Dmp", "DownwardMessageQueues")
        queue_heads = para_ids(url, "Dmp", "DownwardMessageQueueHeads")
        heads = para_ids(url, "Paras", "Heads")

        print(f"=== {name} @ #{number} ===")
        print(f"queues: {len(queues)}  queue_heads: {len(queue_heads)}  para_heads: {len(heads)}")
        print(f"queue w/o head : {sorted(queues - heads)}")
        print(f"qhead w/o head : {sorted(queue_heads - heads)}")
        print()

        # The migration removes both entries of a para, so the two sets have to agree.
        if queues - heads != queue_heads - heads:
            print(f"{name}: queues and queue heads disagree, the migration needs a closer look")
            return 1
        stale[name] = sorted(queues - heads)

    for name, paras in stale.items():
        print(f"{name} STALE_PARAS: {paras}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
