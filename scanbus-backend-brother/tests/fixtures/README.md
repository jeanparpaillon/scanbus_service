# skey protocol fixtures

Every file here is a **payload observed outside this crate**, so that the parsers in
`src/skey/` are tested against something they did not produce. Each one names where it
came from; a fixture with no provenance is worthless, because the only thing it can then
prove is that the code agrees with itself.

`tests/protocol_fixtures.rs` reads them. Nothing here needs hardware or a network.

## What is here now

| File | Provenance |
| --- | --- |
| `keypress-image.payload` | The sample key-press payload Brother compiled into `brscan-skey-exe` 0.3.4-0, at `.data` offset `0x61b480` |
| `keypress-image.header` | The four framing bytes in front of it, at the same offset |
| `registration-image.value` | Rendered from `brscan-skey`'s own format string at `.rodata` `0x414a78`, with the arguments the `sprintf` at `0x406efe` passes |

Recover the first two with:

```sh
objdump -s -j .data /opt/brother/scanner/brscan-skey/brscan-skey-exe |
    sed -n '/61b480/,/61b510/p'
```

Note that the header Brother compiled in declares a payload length that does not match
the payload: the branch it was written for (`b[0] == 0x02`) returns from
`check_udp_data` before the length is checked. `keypress-image.header` is that literal
blob; the test reframes the payload with a consistent header before parsing it, and says
so.

## What is missing, and why

**A packet capture.** Issue 5.7 asks for `tcpdump -i any -s0 -w skey.pcap 'udp port 161
or udp port 54925'` around a live `brscan-skey`, with one press per panel entry. That
needs `CAP_NET_RAW` and someone standing at the printer, so it is not something the
implementation could do for itself. `scripts/capture-skey.sh` runs it; drop the result
here as `skey.pcap` and extend `protocol_fixtures.rs` over it.

Until then, two claims in the design remain **unverified**, and no test in this crate
asserts either:

- that the device *accepts* a `SetRequest` on `1.3.6.1.4.1.2435.2.3.9.2.11.1.1.0` — the
  OID exists and reads back `TRUE`, which is a different statement;
- the `(id, code)` a real device puts in front of a key press. `check_udp_data` requires
  `0x01`/`0x01`, and `src/skey/event.rs` enforces exactly that, so a device that used
  another pair fails loudly with the bytes in the message.

What the capture is *not* needed for is the encoding itself. The vendor binary ships
unstripped, so `BerEncode1` and friends were read directly; that covers every input,
including the length and integer boundaries a single capture would never happen to cross.
